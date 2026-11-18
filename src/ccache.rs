//! The compile cache (DESIGN section 16, CF-28): the key of a rustc invocation,
//! the entries and their encrypted store.
//!
//! An entry is what a successful, remote-eligible rustc run produced: its
//! output files, the lines it printed and nothing else. The key says
//! everything the output can depend on (see [`cache_key`]); the store keeps
//! the files in a [`BlobStore`] of its own, encrypted with a key derived from
//! the farm's cache secret, so any farm member can read an entry that another
//! member wrote.

use crate::cas::BlobStore;
use crate::proto::{Arg, BlobHash, ClientPath, EnvValue, InputFile, JobSpec, NodeId, PathStyle};
use anyhow::{Context, bail};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Bump when the key recipe or the entry layout changes: every older entry is
/// then simply never found.
pub const CACHE_FORMAT: u32 = 1;
/// An entry with more output bytes than this is not stored.
pub const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
/// Output files per entry.
pub const MAX_ENTRY_FILES: usize = 64;
/// What the wrapper may capture of rustc's printed lines, and an entry keeps.
pub const MAX_LINES_BYTES: usize = 8 * 1024 * 1024;
/// The entry index (not the blobs) is kept below this many bytes.
const MAX_INDEX_BYTES: u64 = 512 * 1024 * 1024;
const ENTRY_MAGIC: &[u8; 4] = b"CFE1";
const NONCE_LEN: usize = 24;
/// Blobs of the cache belong to the farm, not to a submitter.
const FARM_OWNER: NodeId = NodeId([0xCC; 16]);

/// 32-byte BLAKE3 digest naming one invocation.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(pub [u8; 32]);

impl CacheKey {
    pub fn to_hex(&self) -> String {
        data_encoding::HEXLOWER.encode(&self.0)
    }
}

impl std::fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CacheKey({})", &self.to_hex()[..16])
    }
}

// ------------------------------------------------------------------ the key

/// Length-prefixed, tagged fields: no two different field lists can encode
/// to the same byte string.
struct Enc(blake3::Hasher);

impl Enc {
    fn bytes(&mut self, tag: u8, b: &[u8]) {
        self.0.update(&[tag]);
        self.0.update(&(b.len() as u64).to_le_bytes());
        self.0.update(b);
    }
    fn str(&mut self, tag: u8, s: &str) {
        self.bytes(tag, s.as_bytes());
    }
    fn num(&mut self, tag: u8, n: u64) {
        self.0.update(&[tag]);
        self.0.update(&n.to_le_bytes());
    }
    fn path(&mut self, tag: u8, p: &ClientPath) {
        self.0.update(&[tag]);
        self.0.update(&[match p.style {
            PathStyle::Windows => 1,
            PathStyle::Posix => 2,
        }]);
        self.str(tag, &p.path);
    }
}

/// The cache key of a job, from its spec with `inputs` filled in.
///
/// Why this is complete for rustc: the files rustc writes and the lines it
/// prints are a function of (1) the compiler, (2) its argument list, (3) the
/// environment variables it reads, (4) the files it reads and (5) the working
/// directory its relative paths are taken from. (1) is the release and commit
/// hash of the compiler plus the host it runs on and the target; (2) is the
/// rendered argument list with the CLIENT paths cargo passed; (3) is the
/// forwarded environment (cargo's variables, `forward_env`, build-script
/// `rustc-env`), names and values, and the variables `env!` reads that are
/// not in it are caught separately (the wrapper checks the dep-info of every
/// result, section 16); (4) is the hash of every input file (package
/// directory, `OUT_DIR`, the whole `--extern` closure, native libraries), and
/// files read from anywhere else keep an entry from being stored at all
/// ([`deps_covered`]); (5) is `cwd`.
///
/// Deliberately NOT part of it: the job id, `no_retain` and the machine that
/// ran the job (the farm treats every peer's result as equal to a local one),
/// and `needs_host` and `dirs`, which are derived from what is in.
pub fn cache_key(spec: &JobSpec) -> CacheKey {
    let mut e = Enc(blake3::Hasher::new_derive_key("crabfarm compile cache key"));
    e.num(1, CACHE_FORMAT as u64);
    e.str(2, env!("CARGO_PKG_VERSION_MAJOR"));
    e.str(3, &spec.toolchain.release);
    e.str(4, &spec.toolchain.commit_hash);
    e.str(5, &spec.toolchain.host);
    e.str(6, &spec.target);
    e.path(7, &spec.cwd);
    e.num(8, spec.args.len() as u64);
    for a in &spec.args {
        match a {
            Arg::Plain(s) => e.str(10, s),
            Arg::Path(p) => e.path(11, p),
            Arg::PrefixedPath { prefix, path } => {
                e.str(12, prefix);
                e.path(13, path);
            }
            Arg::RemapFrom { from, to } => {
                e.path(14, from);
                e.str(15, to);
            }
        }
    }
    let mut env: Vec<&(String, EnvValue)> = spec.env.iter().collect();
    env.sort_by(|a, b| a.0.cmp(&b.0));
    e.num(20, env.len() as u64);
    for (name, value) in env {
        e.str(21, name);
        match value {
            EnvValue::Plain(v) => e.str(22, v),
            EnvValue::Path(p) => e.path(23, p),
        }
    }
    let mut inputs: Vec<&InputFile> = spec.inputs.iter().collect();
    inputs.sort_by(|a, b| a.path.cmp(&b.path));
    e.num(30, inputs.len() as u64);
    for i in inputs {
        e.path(31, &i.path);
        e.bytes(32, &i.hash.0);
        e.num(33, u64::from(i.executable));
    }
    CacheKey(*e.0.finalize().as_bytes())
}

/// `--target` is a path to a target specification: its content is an input
/// the key does not see.
pub fn target_is_file(target: &str) -> bool {
    target.ends_with(".json") || target.contains('/') || target.contains('\\')
}

/// The identity of the crate a job compiles, for the hermetic proofs
/// ([`crate::hermetic`]): package name and version (`CARGO_PKG_NAME`,
/// `CARGO_PKG_VERSION`), the client manifest dir, the target and the crate
/// types. `None` when the job does not carry them all; such a crate can never
/// be proven and its local results are never stored.
pub fn crate_identity(spec: &JobSpec) -> Option<String> {
    let var = |name: &str| spec.env.iter().find(|(k, _)| k == name).map(|(_, v)| v);
    let (Some(EnvValue::Plain(name)), Some(EnvValue::Plain(version)), Some(EnvValue::Path(dir))) = (
        var("CARGO_PKG_NAME"),
        var("CARGO_PKG_VERSION"),
        var("CARGO_MANIFEST_DIR"),
    ) else {
        return None;
    };
    let mut types: Vec<&str> = Vec::new();
    let mut args = spec.args.iter();
    while let Some(a) = args.next() {
        let Arg::Plain(a) = a else { continue };
        if a == "--crate-type" {
            if let Some(Arg::Plain(v)) = args.next() {
                types.extend(v.split(','));
            }
        } else if let Some(v) = a.strip_prefix("--crate-type=") {
            types.extend(v.split(','));
        }
    }
    types.sort_unstable();
    types.dedup();
    if types.is_empty() {
        return None;
    }
    let mut e = Enc(blake3::Hasher::new_derive_key(
        "crabfarm hermetic crate identity",
    ));
    e.str(1, name);
    e.str(2, version);
    e.path(3, dir);
    e.str(4, &spec.target);
    e.num(5, types.len() as u64);
    for t in types {
        e.str(6, t);
    }
    Some(data_encoding::HEXLOWER.encode(e.0.finalize().as_bytes()))
}

// ----------------------------------------------------------- dep-info check

/// The files a dep-info (`.d`) lists as read: its `path:` lines without
/// dependencies (rustc writes one for every file after the first rule).
pub fn dep_files(dep_info: &str) -> Vec<String> {
    dep_info
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.starts_with('#') && l.ends_with(':') && l.len() > 1)
        .map(|l| l[..l.len() - 1].replace("\\ ", " "))
        .collect()
}

/// True when rustc read nothing the key does not cover: the dep-info names at
/// least one file and every file is one of the job's inputs. This is what
/// keeps `include_str!("../../elsewhere")` (and a primary input outside the
/// package directory) out of the cache: such a file is not in `inputs`, so a
/// change to it would not change the key.
pub fn deps_covered(dep_info: &str, cwd: &ClientPath, inputs: &[InputFile]) -> bool {
    let files = dep_files(dep_info);
    if files.is_empty() {
        return false;
    }
    let fold = |p: &ClientPath| match p.style {
        PathStyle::Windows => p.path.to_lowercase(),
        PathStyle::Posix => p.path.clone(),
    };
    let known: std::collections::HashSet<String> = inputs.iter().map(|i| fold(&i.path)).collect();
    let sep = match cwd.style {
        PathStyle::Windows => '\\',
        PathStyle::Posix => '/',
    };
    files.iter().all(|f| {
        let abs = ClientPath::parse(cwd.style, f)
            .or_else(|| ClientPath::parse(cwd.style, &format!("{}{sep}{f}", cwd.path)));
        abs.is_some_and(|p| known.contains(&fold(&p)))
    })
}

// -------------------------------------------------------- entries and store

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct EntryOutput {
    pub path: ClientPath,
    pub hash: BlobHash,
    pub size: u64,
}

/// One cached result. Encrypted on disk with the farm's cache key; the key it
/// was stored under is authenticated with it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct Entry {
    pub format: u32,
    pub key: [u8; 32],
    pub crate_name: String,
    pub created: u64,
    /// Output files in the order they are replayed.
    pub outputs: Vec<EntryOutput>,
    /// What rustc printed, in order (`true` = stdout).
    pub lines: Vec<(bool, String)>,
}

impl Entry {
    pub fn total_bytes(&self) -> u64 {
        self.outputs.iter().map(|o| o.size).sum()
    }
}

/// What a finished run left behind, ready to become an entry.
pub struct Captured {
    pub outputs: Vec<EntryOutput>,
    pub lines: Vec<(bool, String)>,
    /// Content of the `.d` file, for [`deps_covered`].
    pub dep_info: Option<String>,
}

/// A result may be stored only when it is complete and nothing it read lies
/// outside the key.
pub fn entry_from(spec: &JobSpec, key: &CacheKey, c: Captured, now: u64) -> Option<Entry> {
    let total: u64 = c.outputs.iter().map(|o| o.size).sum();
    let line_bytes: usize = c.lines.iter().map(|(_, l)| l.len() + 1).sum();
    if c.outputs.is_empty()
        || c.outputs.len() > MAX_ENTRY_FILES
        || total > MAX_ENTRY_BYTES
        || line_bytes > MAX_LINES_BYTES
        || target_is_file(&spec.target)
    {
        return None;
    }
    if !deps_covered(c.dep_info.as_deref()?, &spec.cwd, &spec.inputs) {
        return None;
    }
    Some(Entry {
        format: CACHE_FORMAT,
        key: key.0,
        crate_name: spec.crate_name.clone(),
        created: now,
        outputs: c.outputs,
        lines: c.lines,
    })
}

/// The key under which entries and blobs are encrypted, derived from the
/// farm secret the CA handed out.
pub fn storage_key(farm_secret: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("crabfarm compile cache storage v1", farm_secret)
}

pub struct Ccache {
    root: PathBuf,
    blobs: BlobStore,
    key: [u8; 32],
}

/// What is on disk (no key needed to count it).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct DiskUsage {
    pub entries: u64,
    pub blobs: u64,
    pub bytes: u64,
}

pub fn root_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("ccache")
}

/// Count what the cache holds in `data_dir`.
pub fn disk_usage(data_dir: &Path) -> DiskUsage {
    let root = root_dir(data_dir);
    let mut u = DiskUsage::default();
    for (sub, is_entry) in [("entries", true), ("blobs", false)] {
        for f in walkdir::WalkDir::new(root.join(sub))
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            if f.file_name().to_string_lossy().starts_with(".tmp-") {
                continue;
            }
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            u.bytes += len;
            if is_entry {
                u.entries += 1;
            } else {
                u.blobs += 1;
            }
        }
    }
    u
}

/// Delete everything the cache holds in `data_dir`. Returns what was removed.
pub fn clear(data_dir: &Path) -> anyhow::Result<DiskUsage> {
    let before = disk_usage(data_dir);
    let root = root_dir(data_dir);
    for sub in ["entries", "blobs"] {
        let d = root.join(sub);
        match std::fs::remove_dir_all(&d) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", d.display())),
        }
    }
    Ok(before)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Ccache {
    pub fn open(data_dir: &Path, farm_secret: &[u8; 32], max_gb: u64) -> anyhow::Result<Ccache> {
        let root = root_dir(data_dir);
        std::fs::create_dir_all(root.join("entries")).context("create cache entry directory")?;
        Ok(Ccache {
            blobs: BlobStore::open(&root.join("blobs"), max_gb.saturating_mul(1 << 30))?,
            key: storage_key(farm_secret),
            root,
        })
    }

    fn entry_path(&self, key: &CacheKey) -> PathBuf {
        let hex = key.to_hex();
        self.root.join("entries").join(&hex[..2]).join(hex)
    }

    /// Keep one output file (plaintext, verified against `hash` by the store).
    pub fn put_blob(&self, hash: &BlobHash, data: &[u8]) -> anyhow::Result<()> {
        self.blobs.put(FARM_OWNER, &self.key, hash, data)
    }

    /// Write the entry index atomically.
    pub fn store(&self, key: &CacheKey, entry: &Entry) -> anyhow::Result<()> {
        if entry.key != key.0 {
            bail!("entry does not belong to this key");
        }
        let plain = postcard::to_stdvec(entry).context("encode cache entry")?;
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce[..]);
        let ct = XChaCha20Poly1305::new((&self.key).into())
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plain,
                    aad: &aad(key),
                },
            )
            .map_err(|_| anyhow::anyhow!("encrypt cache entry"))?;
        let path = self.entry_path(key);
        let parent = path.parent().context("entry path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let mut tag = [0u8; 8];
        rand::fill(&mut tag[..]);
        let tmp = parent.join(format!(".tmp-{}", data_encoding::HEXLOWER.encode(&tag)));
        let mut file = Vec::with_capacity(ENTRY_MAGIC.len() + NONCE_LEN + ct.len());
        file.extend_from_slice(ENTRY_MAGIC);
        file.extend_from_slice(&nonce);
        file.extend_from_slice(&ct);
        if let Err(e) = std::fs::write(&tmp, &file) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("write cache entry");
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("store cache entry");
        }
        Ok(())
    }

    /// The entry for `key`, when it is there, authentic, of this format and
    /// every one of its blobs is still in the store. A broken entry is deleted.
    pub fn lookup(&self, key: &CacheKey) -> Option<Entry> {
        let path = self.entry_path(key);
        let raw = std::fs::read(&path).ok()?;
        let entry = self.open_entry(key, &raw);
        let ok = entry.as_ref().filter(|e| {
            e.outputs
                .iter()
                .all(|o| self.blobs.has(FARM_OWNER, &o.hash))
        });
        if ok.is_none() {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        // Recently used: the index is evicted by age too.
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
            let _ = f.set_modified(SystemTime::now());
        }
        entry
    }

    fn open_entry(&self, key: &CacheKey, raw: &[u8]) -> Option<Entry> {
        if raw.len() < ENTRY_MAGIC.len() + NONCE_LEN || &raw[..ENTRY_MAGIC.len()] != ENTRY_MAGIC {
            return None;
        }
        let (nonce, ct) = raw[ENTRY_MAGIC.len()..].split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().ok()?;
        let plain = XChaCha20Poly1305::new((&self.key).into())
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ct,
                    aad: &aad(key),
                },
            )
            .ok()?;
        let e: Entry = postcard::from_bytes(&plain).ok()?;
        (e.format == CACHE_FORMAT && e.key == key.0).then_some(e)
    }

    /// Every output file of `entry`, authenticated and with the size and hash
    /// the entry promises. All or an error: a cached result is used whole or
    /// not at all.
    pub fn load(&self, entry: &Entry) -> anyhow::Result<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(entry.outputs.len());
        for o in &entry.outputs {
            let data = self.blobs.get(FARM_OWNER, &self.key, &o.hash)?;
            if data.len() as u64 != o.size {
                bail!("cached file has the wrong size");
            }
            out.push(data);
        }
        Ok(out)
    }

    /// Delete an entry (its blobs stay for other entries and age out).
    pub fn forget(&self, key: &CacheKey) {
        let _ = std::fs::remove_file(self.entry_path(key));
    }

    /// Least-recently-used eviction of the blobs (the size limit) and of the
    /// index.
    pub fn evict(&self) -> anyhow::Result<u64> {
        let freed = self.blobs.evict()?;
        let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        for f in walkdir::WalkDir::new(self.root.join("entries"))
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let Ok(md) = f.metadata() else { continue };
            total += md.len();
            files.push((
                md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                md.len(),
                f.into_path(),
            ));
        }
        files.sort_by_key(|(t, _, _)| *t);
        for (_, len, path) in files {
            if total <= MAX_INDEX_BYTES {
                break;
            }
            if std::fs::remove_file(path).is_ok() {
                total -= len;
            }
        }
        Ok(freed)
    }

    pub fn now() -> u64 {
        unix_now()
    }
}

fn aad(key: &CacheKey) -> Vec<u8> {
    let mut v = ENTRY_MAGIC.to_vec();
    v.extend_from_slice(&key.0);
    v
}

// ----------------------------------------------------------- local outputs

/// The files a local rustc run left in `out_dir` for this invocation: every
/// file named `<crate><extra-filename>.*` or `lib<crate><extra-filename>.*`
/// (rlib, rmeta, `.d`, a proc-macro library and its import library). Cargo's
/// `-C extra-filename` carries a hash that is unique to the unit, which is what
/// makes this exact in a directory shared by every crate; without it the set
/// cannot be told apart and `None` is returned. `None` too when a file the
/// invocation announced is missing or the dep-info is.
pub fn local_output_files(
    spec: &JobSpec,
    produces: &[ClientPath],
    extra_filename: &str,
) -> Option<Vec<ClientPath>> {
    if extra_filename.is_empty() {
        return None;
    }
    let dir = spec.out_dir.to_local();
    let stems = [
        format!("lib{}{}.", spec.crate_name, extra_filename),
        format!("{}{}.", spec.crate_name, extra_filename),
    ];
    let mut found: Vec<ClientPath> = Vec::new();
    for entry in std::fs::read_dir(&dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_str()?;
        if !stems.iter().any(|s| name.starts_with(s.as_str())) || !entry.file_type().ok()?.is_file()
        {
            continue;
        }
        found.push(ClientPath::from_local(&entry.path(), Path::new("."))?);
    }
    found.sort();
    let dep = format!("{}{}.d", spec.crate_name, extra_filename);
    let has_dep = found.iter().any(|p| p.path.ends_with(&dep));
    let has_products = produces.iter().all(|p| found.contains(p));
    (has_dep && has_products && !found.is_empty() && found.len() <= MAX_ENTRY_FILES)
        .then_some(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{JobId, ToolchainId};

    fn cp(s: &str) -> ClientPath {
        ClientPath::parse(PathStyle::Windows, s).unwrap()
    }

    fn input(path: &str, content: &[u8]) -> InputFile {
        InputFile {
            path: cp(path),
            hash: BlobHash::of(content),
            size: content.len() as u64,
            executable: false,
        }
    }

    type Edit = Box<dyn Fn(&mut JobSpec)>;

    fn spec() -> JobSpec {
        JobSpec {
            job_id: JobId([1; 16]),
            toolchain: ToolchainId {
                release: "1.100.0".into(),
                commit_hash: "a".repeat(40),
                commit_date: "2026-11-10".into(),
                host: "x86_64-pc-windows-msvc".into(),
            },
            target: "x86_64-pc-windows-msvc".into(),
            args: vec![
                Arg::Plain("--crate-name".into()),
                Arg::Plain("foo".into()),
                Arg::Path(cp(r"C:\p\src\lib.rs")),
                Arg::PrefixedPath {
                    prefix: "--out-dir=".into(),
                    path: cp(r"C:\p\target\debug\deps"),
                },
                Arg::Plain("-C".into()),
                Arg::Plain("opt-level=0".into()),
            ],
            cwd: cp(r"C:\p"),
            env: vec![
                ("CARGO_PKG_NAME".into(), EnvValue::Plain("foo".into())),
                ("CARGO_MANIFEST_DIR".into(), EnvValue::Path(cp(r"C:\p"))),
            ],
            extra_env: vec![],
            build_env: vec![],
            inputs: vec![
                input(r"C:\p\src\lib.rs", b"fn a() {}"),
                input(r"C:\p\src\b.rs", b"fn b() {}"),
            ],
            dirs: vec![],
            out_dir: cp(r"C:\p\target\debug\deps"),
            crate_name: "foo".into(),
            no_retain: false,
            needs_host: false,
        }
    }

    /// The identity is the package, its version, where it lives, the target and
    /// the crate types, and nothing a source edit changes.
    #[test]
    fn the_crate_identity_is_name_version_dir_target_and_types() {
        let mut base = spec();
        base.env
            .push(("CARGO_PKG_VERSION".into(), EnvValue::Plain("1.2.3".into())));
        base.args.push(Arg::Plain("--crate-type".into()));
        base.args.push(Arg::Plain("lib".into()));
        let id = crate_identity(&base).expect("identity");
        assert_eq!(id.len(), 64);
        let with = |f: &dyn Fn(&mut JobSpec)| {
            let mut s = base.clone();
            f(&mut s);
            crate_identity(&s)
        };
        assert_eq!(
            with(&|s| s.inputs[0].hash = BlobHash::of(b"edited")).as_deref(),
            Some(id.as_str()),
            "a source edit is the same crate"
        );
        assert_eq!(
            with(&|s| {
                s.args.pop();
                s.args.pop();
                s.args.push(Arg::Plain("--crate-type=lib".into()));
            })
            .as_deref(),
            Some(id.as_str()),
            "both spellings of the crate type"
        );
        for (what, other) in [
            (
                "name",
                with(&|s| s.env[0].1 = EnvValue::Plain("bar".into())),
            ),
            (
                "version",
                with(&|s| s.env[2].1 = EnvValue::Plain("1.2.4".into())),
            ),
            ("dir", with(&|s| s.env[1].1 = EnvValue::Path(cp(r"C:\q")))),
            (
                "target",
                with(&|s| s.target = "aarch64-apple-darwin".into()),
            ),
            (
                "types",
                with(&|s| {
                    s.args.pop();
                    s.args.push(Arg::Plain("rlib".into()));
                }),
            ),
        ] {
            assert_ne!(other.as_deref(), Some(id.as_str()), "{what} must matter");
        }
        assert_eq!(
            with(&|s| {
                s.env.remove(2);
            }),
            None,
            "without a version there is no identity"
        );
    }

    /// Every component of the key matters (CF-28): changing any one of them
    /// gives another key, and things that must not matter do not.
    #[test]
    fn the_key_changes_with_every_component_and_only_with_them() {
        let base = cache_key(&spec());
        assert_eq!(base, cache_key(&spec()), "stable");
        let changed: Vec<(&str, Edit)> = vec![
            (
                "release",
                Box::new(|s| s.toolchain.release = "1.101.0".into()),
            ),
            (
                "commit",
                Box::new(|s| s.toolchain.commit_hash = "b".repeat(40)),
            ),
            (
                "host",
                Box::new(|s| s.toolchain.host = "aarch64-apple-darwin".into()),
            ),
            (
                "target",
                Box::new(|s| s.target = "x86_64-unknown-linux-gnu".into()),
            ),
            ("cwd", Box::new(|s| s.cwd = cp(r"C:\q"))),
            (
                "arg value",
                Box::new(|s| s.args[5] = Arg::Plain("opt-level=3".into())),
            ),
            (
                "extra arg",
                Box::new(|s| s.args.push(Arg::Plain("--cfg=x".into()))),
            ),
            (
                "arg path (client path)",
                Box::new(|s| {
                    s.args[3] = Arg::PrefixedPath {
                        prefix: "--out-dir=".into(),
                        path: cp(r"C:\other\deps"),
                    }
                }),
            ),
            (
                "arg kind",
                Box::new(|s| s.args[1] = Arg::Path(cp(r"C:\foo"))),
            ),
            (
                "env value",
                Box::new(|s| s.env[0].1 = EnvValue::Plain("bar".into())),
            ),
            (
                "env name",
                Box::new(|s| s.env[0].0 = "CARGO_PKG_VERSION".into()),
            ),
            (
                "env added",
                Box::new(|s| s.env.push(("X".into(), EnvValue::Plain("1".into())))),
            ),
            (
                "env path",
                Box::new(|s| s.env[1].1 = EnvValue::Path(cp(r"C:\other"))),
            ),
            (
                "input content",
                Box::new(|s| s.inputs[0] = input(r"C:\p\src\lib.rs", b"fn a() { }")),
            ),
            (
                "input path",
                Box::new(|s| s.inputs[1] = input(r"C:\p\src\c.rs", b"fn b() {}")),
            ),
            (
                "input added",
                Box::new(|s| s.inputs.push(input(r"C:\p\x.txt", b"x"))),
            ),
            (
                "input removed",
                Box::new(|s| {
                    s.inputs.pop();
                }),
            ),
            (
                "input exec bit",
                Box::new(|s| s.inputs[0].executable = true),
            ),
        ];
        for (what, f) in &changed {
            let mut s = spec();
            f(&mut s);
            assert_ne!(cache_key(&s), base, "{what} must change the key");
        }
        // Not part of the key.
        let same: Vec<Edit> = vec![
            Box::new(|s| s.job_id = JobId([9; 16])),
            Box::new(|s| s.no_retain = true),
            Box::new(|s| s.needs_host = true),
            Box::new(|s| s.dirs.push(cp(r"C:\d"))),
            // The order inputs and variables were collected in.
            Box::new(|s| s.inputs.reverse()),
            Box::new(|s| s.env.reverse()),
        ];
        for f in same {
            let mut s = spec();
            f(&mut s);
            assert_eq!(cache_key(&s), base);
        }
    }

    #[test]
    fn fields_cannot_run_into_each_other() {
        // ["ab", "c"] and ["a", "bc"] must not collide.
        let mut a = spec();
        a.args = vec![Arg::Plain("ab".into()), Arg::Plain("c".into())];
        let mut b = spec();
        b.args = vec![Arg::Plain("a".into()), Arg::Plain("bc".into())];
        assert_ne!(cache_key(&a), cache_key(&b));
    }

    const DEP: &str = "C:\\p\\target\\debug\\deps\\foo-1.rmeta: src\\lib.rs src\\b.rs\n\nsrc\\lib.rs:\nsrc\\b.rs:\n\n# env-dep:FOO=1\n";

    #[test]
    fn dep_info_files_are_listed_and_checked_against_the_inputs() {
        assert_eq!(dep_files(DEP), vec![r"src\lib.rs", r"src\b.rs"]);
        let s = spec();
        assert!(deps_covered(DEP, &s.cwd, &s.inputs));
        // Absolute spellings and another case on Windows are the same file.
        let abs = "x.rmeta: C:\\p\\SRC\\lib.rs\n\nC:\\p\\SRC\\lib.rs:\n";
        assert!(deps_covered(abs, &s.cwd, &s.inputs));
        // A file outside the inputs (include_str! from elsewhere) is not covered.
        let outside = "x.rmeta: src\\lib.rs ..\\shared.txt\n\nsrc\\lib.rs:\n..\\shared.txt:\n";
        assert!(!deps_covered(outside, &s.cwd, &s.inputs));
        // Nothing listed, or no dep-info at all, is not covered either.
        assert!(!deps_covered("x.rmeta:\n", &s.cwd, &s.inputs));
        assert!(!deps_covered("", &s.cwd, &s.inputs));
        // Escaped spaces.
        assert_eq!(dep_files("a: b\n\nmy\\ file.rs:\n"), vec!["my file.rs"]);
    }

    #[test]
    fn an_entry_needs_complete_covered_output() {
        let s = spec();
        let k = cache_key(&s);
        let out = |n: &str| EntryOutput {
            path: cp(&format!(r"C:\p\target\debug\deps\{n}")),
            hash: BlobHash::of(n.as_bytes()),
            size: n.len() as u64,
        };
        let cap = |dep: Option<&str>, outputs: Vec<EntryOutput>| Captured {
            outputs,
            lines: vec![(false, "warning".into())],
            dep_info: dep.map(String::from),
        };
        assert!(entry_from(&s, &k, cap(Some(DEP), vec![out("foo-1.rlib")]), 1).is_some());
        assert!(entry_from(&s, &k, cap(None, vec![out("foo-1.rlib")]), 1).is_none());
        assert!(entry_from(&s, &k, cap(Some(DEP), vec![]), 1).is_none());
        let outside = "x: ..\\o.rs\n\n..\\o.rs:\n";
        assert!(entry_from(&s, &k, cap(Some(outside), vec![out("a")]), 1).is_none());
        let mut file_target = spec();
        file_target.target = r"C:\specs\mine.json".into();
        assert!(entry_from(&file_target, &k, cap(Some(DEP), vec![out("a")]), 1).is_none());
    }

    fn store(dir: &Path) -> Ccache {
        Ccache::open(dir, &[7; 32], 1).unwrap()
    }

    fn put(c: &Ccache, s: &JobSpec, files: &[(&str, &[u8])]) -> (CacheKey, Entry) {
        let k = cache_key(s);
        let mut outputs = Vec::new();
        for (n, data) in files {
            let h = BlobHash::of(data);
            c.put_blob(&h, data).unwrap();
            outputs.push(EntryOutput {
                path: cp(&format!(r"C:\p\target\debug\deps\{n}")),
                hash: h,
                size: data.len() as u64,
            });
        }
        let e = entry_from(
            s,
            &k,
            Captured {
                outputs,
                lines: vec![(false, "{\"artifact\":\"x\",\"emit\":\"metadata\"}".into())],
                dep_info: Some(DEP.into()),
            },
            5,
        )
        .unwrap();
        c.store(&k, &e).unwrap();
        (k, e)
    }

    #[test]
    fn an_entry_round_trips_and_is_found_only_with_the_right_key_and_secret() {
        let d = tempfile::tempdir().unwrap();
        let c = store(d.path());
        let s = spec();
        let (k, e) = put(
            &c,
            &s,
            &[("foo-1.rlib", b"rlib-bytes"), ("foo-1.d", b"dep")],
        );
        assert_eq!(c.lookup(&k).unwrap(), e);
        assert_eq!(
            c.load(&e).unwrap(),
            vec![b"rlib-bytes".to_vec(), b"dep".to_vec()]
        );
        // Another key: no entry.
        let mut other = spec();
        other.target = "x86_64-unknown-linux-gnu".into();
        assert!(c.lookup(&cache_key(&other)).is_none());
        // A node of another farm (another secret) cannot read the entry.
        let foreign = Ccache::open(d.path(), &[8; 32], 1).unwrap();
        assert!(foreign.lookup(&k).is_none());
        assert!(
            !foreign.entry_path(&k).exists(),
            "an unreadable entry is dropped"
        );
    }

    #[test]
    fn a_tampered_blob_or_entry_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let c = store(d.path());
        let (k, e) = put(&c, &spec(), &[("foo-1.rlib", b"rlib-bytes")]);
        // Flip a byte of the blob on disk.
        let blob = walkdir::WalkDir::new(root_dir(d.path()).join("blobs"))
            .into_iter()
            .filter_map(|x| x.ok())
            .find(|x| x.file_type().is_file())
            .unwrap()
            .into_path();
        let mut raw = std::fs::read(&blob).unwrap();
        let n = raw.len();
        raw[n - 1] ^= 1;
        std::fs::write(&blob, raw).unwrap();
        assert!(c.load(&e).is_err(), "tampered blob must not load");
        // Flip a byte of the entry: it is not found (and removed).
        let (k2, _) = put(
            &c,
            &{
                let mut s = spec();
                s.crate_name = "bar".into();
                s
            },
            &[("bar-1.rlib", b"zzz")],
        );
        let p = c.entry_path(&k2);
        let mut raw = std::fs::read(&p).unwrap();
        let n = raw.len();
        raw[n - 1] ^= 1;
        std::fs::write(&p, raw).unwrap();
        assert!(c.lookup(&k2).is_none());
        // A missing blob makes the entry unusable too.
        std::fs::remove_file(&blob).unwrap();
        assert!(c.lookup(&k).is_none());
    }

    #[test]
    fn disk_usage_counts_and_clear_removes() {
        let d = tempfile::tempdir().unwrap();
        let c = store(d.path());
        put(&c, &spec(), &[("foo-1.rlib", b"abc")]);
        let u = disk_usage(d.path());
        assert_eq!((u.entries, u.blobs), (1, 1));
        assert!(u.bytes > 0);
        assert_eq!(clear(d.path()).unwrap().entries, 1);
        assert_eq!(disk_usage(d.path()), DiskUsage::default());
    }

    #[test]
    fn local_outputs_are_the_files_of_this_unit_only() {
        let d = tempfile::tempdir().unwrap();
        let deps = d.path().join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        for n in [
            "libfoo-aa.rlib",
            "libfoo-aa.rmeta",
            "foo-aa.d",
            "libfoo-bb.rlib",
            "foo-bb.d",
            "libfoobar-aa.rlib",
            "other.txt",
        ] {
            std::fs::write(deps.join(n), n).unwrap();
        }
        let mut s = spec();
        s.out_dir = ClientPath::from_local(&deps, Path::new(".")).unwrap();
        let produces =
            vec![ClientPath::from_local(&deps.join("libfoo-aa.rlib"), Path::new(".")).unwrap()];
        let got = local_output_files(&s, &produces, "-aa").unwrap();
        let names: Vec<String> = got
            .iter()
            .map(|p| p.path.rsplit(['\\', '/']).next().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["foo-aa.d", "libfoo-aa.rlib", "libfoo-aa.rmeta"]);
        // Without the hash suffix the unit cannot be told apart.
        assert!(local_output_files(&s, &produces, "").is_none());
        // A missing announced product, or a missing dep-info, is not cacheable.
        std::fs::remove_file(deps.join("libfoo-aa.rlib")).unwrap();
        assert!(local_output_files(&s, &produces, "-aa").is_none());
        assert!(local_output_files(&s, &[], "-cc").is_none());
    }
}
