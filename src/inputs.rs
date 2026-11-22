//! Package A. Collect the input files of a job (DESIGN section 8, items 1, 2
//! and 4; the dependency closure, item 3, is `depgraph`, package D) and hash
//! them with a persistent cache.

use crate::proto::{BlobHash, ClientPath};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

pub const MAX_FILE: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Collected {
    /// (absolute local path, size, executable)
    pub files: Vec<(PathBuf, u64, bool)>,
    pub total: u64,
}

#[derive(Debug)]
pub enum CollectError {
    TooLarge,
    MissingNativeLib(String),
    /// A generated `.rs` file in `OUT_DIR` embeds the submitter's literal
    /// package or `OUT_DIR` path (CF-43, e.g. `khronos_api`), so rustc on a
    /// peer would try to open that unmapped path outside its sandbox.
    EmbeddedLocalPath,
    Io(std::io::Error),
}

impl From<std::io::Error> for CollectError {
    fn from(e: std::io::Error) -> Self {
        CollectError::Io(e)
    }
}

impl From<walkdir::Error> for CollectError {
    fn from(e: walkdir::Error) -> Self {
        CollectError::Io(e.into())
    }
}

impl std::fmt::Display for CollectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CollectError::TooLarge => write!(f, "job inputs exceed the size cap"),
            CollectError::MissingNativeLib(n) => {
                write!(f, "native library `{n}` not found in the search dirs")
            }
            CollectError::EmbeddedLocalPath => {
                write!(f, "generated file in OUT_DIR embeds a local path")
            }
            CollectError::Io(e) => write!(f, "reading job inputs: {e}"),
        }
    }
}

impl std::error::Error for CollectError {}

/// Package dir (skip `target/`, `.git/`, dot-dirs anywhere, dot-files directly
/// in the package dir such as `.env` and `.npmrc`, files > MAX_FILE),
/// `OUT_DIR` contents, and native libs for static `-l` libraries (`name.lib`
/// for msvc targets, `lib<name>.a` otherwise) from `native`/`all` search dirs.
/// Exceeding MAX_TOTAL is `TooLarge`.
///
/// Deliberate choices:
/// - `target` is skipped only directly under the package dir, so a source
///   module directory `src/target/` is kept. A target dir that lives elsewhere
///   inside the package dir is not known here; callers pass the package dir.
/// - A file over MAX_FILE in the package dir is skipped (it is almost surely
///   not a source); in `OUT_DIR` it is `TooLarge`, because the build needs it.
/// - Symlinks are never followed as directories. A symlink to a regular file
///   inside the same tree is recorded under the link's path; every other
///   symlink is skipped.
pub fn collect(
    package_dir: &Path,
    out_dir_env: Option<&Path>,
    search_dirs: &[(String, PathBuf)],
    link_libs: &[(String, String)],
    target_is_msvc: bool,
) -> Result<Collected, CollectError> {
    collect_with_limits(
        package_dir,
        out_dir_env,
        search_dirs,
        link_libs,
        target_is_msvc,
        MAX_FILE,
        MAX_TOTAL,
    )
}

struct Acc {
    files: Vec<(PathBuf, u64, bool)>,
    total: u64,
    seen: HashSet<PathBuf>,
    max_total: u64,
}

impl Acc {
    fn add(&mut self, path: PathBuf, size: u64, exec: bool) -> Result<(), CollectError> {
        if !self.seen.insert(path.clone()) {
            return Ok(());
        }
        self.total = self.total.saturating_add(size);
        if self.total > self.max_total {
            return Err(CollectError::TooLarge);
        }
        self.files.push((path, size, exec));
        Ok(())
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Walk `root`; `is_package` selects the skip rules for the package dir.
fn walk(root: &Path, is_package: bool, max_file: u64, acc: &mut Acc) -> Result<(), CollectError> {
    let canon_root = root.canonicalize()?;
    let walker = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if !is_package || e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            if !e.file_type().is_dir() {
                // A dot-FILE directly in the package dir (`.env`, `.npmrc`,
                // `.netrc`, ...) is where credentials and local settings live;
                // rustc never reads one, so it is not sent to a peer (CF-19).
                return !(e.depth() == 1 && name.starts_with('.'));
            }
            !(name.starts_with('.') || (e.depth() == 1 && name == "target"))
        });
    for entry in walker {
        let entry = entry?;
        let ft = entry.file_type();
        let (meta, path) = if ft.is_symlink() {
            // Only a link to a regular file inside the tree counts.
            let Ok(canon) = entry.path().canonicalize() else {
                continue;
            };
            if !canon.starts_with(&canon_root) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(entry.path()) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            (meta, entry.path().to_path_buf())
        } else if ft.is_file() {
            (entry.metadata()?, entry.path().to_path_buf())
        } else {
            continue;
        };
        if meta.len() > max_file {
            if is_package {
                continue;
            }
            return Err(CollectError::TooLarge);
        }
        acc.add(path, meta.len(), is_executable(&meta))?;
    }
    Ok(())
}

/// Spellings under which a local directory path can appear inside a generated
/// `.rs` file in `OUT_DIR` (raw `\`, forward `/`, backslash `\`, and escaped `\\`).
fn path_needles(dirs: &[&Path]) -> Vec<String> {
    let mut out = Vec::new();
    for dir in dirs {
        let s = dir.to_string_lossy();
        let trimmed = s.trim_end_matches(['\\', '/']);
        if trimmed.len() < 3 {
            continue;
        }
        let mut variants = vec![
            trimmed.to_string(),
            trimmed.replace('\\', "/"),
            trimmed.replace('\\', "\\\\"),
        ];
        if cfg!(windows) && trimmed.contains('/') {
            let bs = trimmed.replace('/', "\\");
            variants.push(bs.replace('\\', "\\\\"));
            variants.push(bs);
        }
        for v in variants {
            let v = if cfg!(windows) {
                v.to_ascii_lowercase()
            } else {
                v
            };
            if !out.contains(&v) {
                out.push(v);
            }
        }
    }
    out
}

/// True when a generated `.rs` file in `OUT_DIR` contains a non-comment line
/// mentioning one of the submitter's local directory paths (CF-43, e.g.
/// `khronos_api` baking `CARGO_MANIFEST_DIR` into `include_bytes!`). Streams
/// line-by-line and fails closed (`true`) on non-UTF-8 `.rs` files.
fn rs_embeds_local_path(path: &Path, needles: &[String]) -> std::io::Result<bool> {
    use std::io::{BufRead, BufReader};
    if needles.is_empty()
        || !path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("rs"))
    {
        return Ok(false);
    }
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            // Non-UTF-8 `.rs` in OUT_DIR: fail closed and run locally.
            return Ok(true);
        };
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if cfg!(windows) {
            let lower = trimmed.to_ascii_lowercase();
            if needles.iter().any(|n| lower.contains(n.as_str())) {
                return Ok(true);
            }
        } else if needles.iter().any(|n| trimmed.contains(n.as_str())) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn collect_with_limits(
    package_dir: &Path,
    out_dir_env: Option<&Path>,
    search_dirs: &[(String, PathBuf)],
    link_libs: &[(String, String)],
    target_is_msvc: bool,
    max_file: u64,
    max_total: u64,
) -> Result<Collected, CollectError> {
    let mut acc = Acc {
        files: Vec::new(),
        total: 0,
        seen: HashSet::new(),
        max_total,
    };
    walk(package_dir, true, max_file, &mut acc)?;
    if let Some(out) = out_dir_env {
        walk(out, false, max_file, &mut acc)?;
        let needles = path_needles(&[package_dir, out]);
        for (path, _, _) in &acc.files {
            if path.starts_with(out) && rs_embeds_local_path(path, &needles)? {
                return Err(CollectError::EmbeddedLocalPath);
            }
        }
    }
    for (kind, name) in link_libs {
        // Only static libraries are read at compile time of an rlib.
        if kind.split(':').next() != Some("static") {
            continue;
        }
        let file = if target_is_msvc {
            format!("{name}.lib")
        } else {
            format!("lib{name}.a")
        };
        let found = search_dirs
            .iter()
            .filter(|(k, _)| k == "native" || k == "all")
            .map(|(_, d)| d.join(&file))
            .find(|p| p.is_file());
        let Some(path) = found else {
            return Err(CollectError::MissingNativeLib(name.clone()));
        };
        let meta = std::fs::metadata(&path)?;
        if meta.len() > max_file {
            return Err(CollectError::TooLarge);
        }
        acc.add(path, meta.len(), is_executable(&meta))?;
    }
    Ok(Collected {
        files: acc.files,
        total: acc.total,
    })
}

/// Hash cache keyed by client path, valid while (size, mtime nanos) match.
/// Persisted by the node (postcard file).
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct HashCache {
    pub entries: std::collections::HashMap<ClientPath, (u64, i128, BlobHash)>,
}

/// An mtime this close to "now" may still change without changing the stamp
/// (coarse file system clocks), so such a hash is not remembered.
const RACY_WINDOW_NANOS: i128 = 2_000_000_000;

fn mtime_nanos(meta: &std::fs::Metadata) -> std::io::Result<i128> {
    Ok(match meta.modified()?.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    })
}

fn now_nanos() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i128)
}

impl HashCache {
    /// BLAKE3 of the file (streaming) unless a matching entry exists.
    /// Returns (hash, size).
    pub fn hash(&mut self, client: &ClientPath, path: &Path) -> std::io::Result<(BlobHash, u64)> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        let (size, mtime) = (meta.len(), mtime_nanos(&meta)?);
        if let Some((s, m, h)) = self.entries.get(client)
            && *s == size
            && *m == mtime
        {
            return Ok((*h, size));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(std::fs::File::open(path)?)?;
        let read = hasher.count();
        let hash = BlobHash(*hasher.finalize().as_bytes());
        // Remember it only if the file did not change underneath us and its
        // stamp is old enough to be trustworthy.
        let after = std::fs::metadata(path)?;
        let stable = after.len() == size && mtime_nanos(&after)? == mtime && read == size;
        if stable && now_nanos() - mtime > RACY_WINDOW_NANOS {
            self.entries.insert(client.clone(), (size, mtime, hash));
        } else {
            self.entries.remove(client);
        }
        Ok((hash, read))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::PathStyle;
    use std::fs;
    use std::time::Duration;

    fn write(p: &Path, data: &[u8]) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(p, data).expect("write");
    }

    fn names(c: &Collected, root: &Path) -> Vec<String> {
        let mut v: Vec<String> = c
            .files
            .iter()
            .map(|(p, _, _)| {
                p.strip_prefix(root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        v.sort();
        v
    }

    fn collect_pkg(root: &Path) -> Collected {
        collect(root, None, &[], &[], true).expect("collect")
    }

    #[test]
    fn package_skips_target_git_and_dot_dirs() {
        let d = tempfile::tempdir().expect("tmp");
        let r = d.path();
        write(&r.join("Cargo.toml"), b"x");
        write(&r.join("src/lib.rs"), b"fn a(){}");
        write(
            &r.join("src/target/mod.rs"),
            b"nested module dir named target is source",
        );
        write(&r.join("target/debug/junk.rlib"), b"junk");
        write(&r.join(".git/HEAD"), b"ref");
        write(&r.join(".hidden/x.rs"), b"x");
        write(&r.join("src/.cache/y.rs"), b"y");
        // CF-19: dot-files at the package root hold credentials and local
        // settings; none of them leaves the machine. Deeper ones are source.
        write(&r.join(".gitignore"), b"x");
        write(&r.join(".env"), b"TOKEN=secret");
        write(&r.join(".npmrc"), b"//registry:_authToken=secret");
        write(&r.join("src/.keep"), b"a nested dot file is kept");
        let c = collect_pkg(r);
        assert_eq!(
            names(&c, r),
            vec!["Cargo.toml", "src/.keep", "src/lib.rs", "src/target/mod.rs"]
        );
        assert_eq!(c.total, c.files.iter().map(|f| f.1).sum::<u64>());
    }

    #[test]
    fn files_over_max_file_are_skipped_in_package_but_fail_in_out_dir() {
        let d = tempfile::tempdir().expect("tmp");
        let pkg = d.path().join("pkg");
        let out = d.path().join("out");
        write(&pkg.join("small.rs"), b"12345");
        write(&pkg.join("big.bin"), &[0u8; 11]);
        let c = collect_with_limits(&pkg, None, &[], &[], true, 10, 1000).expect("collect");
        assert_eq!(names(&c, &pkg), vec!["small.rs"]);
        write(&out.join("gen.rs"), &[0u8; 11]);
        let e = collect_with_limits(&pkg, Some(&out), &[], &[], true, 10, 1000);
        assert!(matches!(e, Err(CollectError::TooLarge)), "{e:?}");
    }

    #[test]
    fn total_cap_is_enforced() {
        let d = tempfile::tempdir().expect("tmp");
        write(&d.path().join("a"), &[0u8; 6]);
        write(&d.path().join("b"), &[0u8; 6]);
        assert!(collect_with_limits(d.path(), None, &[], &[], true, 10, 12).is_ok());
        let e = collect_with_limits(d.path(), None, &[], &[], true, 10, 11);
        assert!(matches!(e, Err(CollectError::TooLarge)), "{e:?}");
    }

    #[test]
    fn out_dir_is_collected_recursively_including_dot_dirs_and_deduped() {
        let d = tempfile::tempdir().expect("tmp");
        let pkg = d.path().join("pkg");
        let out = d.path().join("out");
        write(&pkg.join("lib.rs"), b"x");
        write(&out.join("a/gen.rs"), b"gen");
        write(&out.join(".hidden/h.rs"), b"h");
        let c = collect(&pkg, Some(&out), &[], &[], true).expect("collect");
        assert_eq!(
            names(&c, d.path()),
            vec!["out/.hidden/h.rs", "out/a/gen.rs", "pkg/lib.rs"]
        );
        // OUT_DIR inside the package dir is not listed twice.
        let inner = pkg.join("gen");
        write(&inner.join("g.rs"), b"g");
        let c = collect(&pkg, Some(&inner), &[], &[], true).expect("collect");
        assert_eq!(
            c.files
                .iter()
                .filter(|(p, _, _)| p.ends_with("g.rs"))
                .count(),
            1
        );
        // A missing OUT_DIR is an error, not silently empty.
        assert!(matches!(
            collect(&pkg, Some(&d.path().join("nope")), &[], &[], true),
            Err(CollectError::Io(_))
        ));
    }

    #[test]
    fn native_libs_msvc_and_unix_names_and_search_kinds() {
        let d = tempfile::tempdir().expect("tmp");
        let pkg = d.path().join("pkg");
        write(&pkg.join("lib.rs"), b"x");
        let libs = d.path().join("libs");
        write(&libs.join("foo.lib"), b"msvc");
        write(&libs.join("libbar.a"), b"unix");
        let dirs = vec![("native".to_string(), libs.clone())];
        let wanted = |n: &str| vec![("static".to_string(), n.to_string())];
        let c = collect(&pkg, None, &dirs, &wanted("foo"), true).expect("msvc");
        assert!(c.files.iter().any(|(p, _, _)| p.ends_with("foo.lib")));
        let c = collect(&pkg, None, &dirs, &wanted("bar"), false).expect("unix");
        assert!(c.files.iter().any(|(p, _, _)| p.ends_with("libbar.a")));
        // `all` dirs count, `dependency` dirs do not.
        let all = vec![("all".to_string(), libs.clone())];
        assert!(collect(&pkg, None, &all, &wanted("foo"), true).is_ok());
        let dep = vec![("dependency".to_string(), libs.clone())];
        assert!(
            matches!(collect(&pkg, None, &dep, &wanted("foo"), true), Err(CollectError::MissingNativeLib(n)) if n == "foo")
        );
        // Wrong naming scheme for the target is missing, not found.
        assert!(matches!(
            collect(&pkg, None, &dirs, &wanted("bar"), true),
            Err(CollectError::MissingNativeLib(_))
        ));
        // Modifier kinds still count as static; dylibs are never collected.
        let modded = vec![("static:+bundle".to_string(), "foo".to_string())];
        assert!(collect(&pkg, None, &dirs, &modded, true).is_ok());
        let dyn_libs = vec![("dylib".to_string(), "nonexistent".to_string())];
        assert!(collect(&pkg, None, &dirs, &dyn_libs, true).is_ok());
    }

    #[test]
    fn symlinks_are_not_followed_out_of_the_package() {
        let d = tempfile::tempdir().expect("tmp");
        let pkg = d.path().join("pkg");
        write(&pkg.join("real.rs"), b"real");
        write(&d.path().join("outside.rs"), b"secret");
        write(&d.path().join("outside_dir/x.rs"), b"x");
        let mk_file = |target: &Path, link: &Path| -> bool {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(target, link).is_ok()
            }
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_file(target, link).is_ok()
            }
        };
        let mk_dir = |target: &Path, link: &Path| -> bool {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(target, link).is_ok()
            }
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_dir(target, link).is_ok()
            }
        };
        // Creating symlinks may need a privilege on Windows; without it there is nothing to test.
        if !mk_file(&pkg.join("real.rs"), &pkg.join("inside_link.rs")) {
            eprintln!("symlinks not available here, skipping");
            return;
        }
        assert!(mk_file(
            &d.path().join("outside.rs"),
            &pkg.join("outside_link.rs")
        ));
        assert!(mk_dir(&d.path().join("outside_dir"), &pkg.join("dir_link")));
        assert!(mk_dir(&pkg, &pkg.join("self_loop")));
        let c = collect_pkg(&pkg);
        // A link to a file inside is recorded (under the link path); outside and dir links are not.
        assert_eq!(names(&c, &pkg), vec!["inside_link.rs", "real.rs"]);
    }

    #[test]
    fn hash_cache_matches_blake3_and_reuses_entries() {
        let d = tempfile::tempdir().expect("tmp");
        let f = d.path().join("f.rs");
        write(&f, b"hello world");
        // Make the stamp old enough to be cached.
        let old = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&f)
            .expect("open")
            .set_modified(old)
            .expect("mtime");
        let client = ClientPath::parse(PathStyle::Windows, r"C:\p\f.rs").expect("path");
        let mut cache = HashCache::default();
        let (h, size) = cache.hash(&client, &f).expect("hash");
        assert_eq!(h, BlobHash::of(b"hello world"));
        assert_eq!(size, 11);
        assert_eq!(cache.entries.len(), 1);
        // Poison the entry: a matching (size, mtime) must return the cached hash without reading.
        let (s, m, _) = cache.entries[&client];
        let fake = BlobHash([7; 32]);
        cache.entries.insert(client.clone(), (s, m, fake));
        assert_eq!(cache.hash(&client, &f).expect("hash").0, fake);
        // Same size, new mtime: rehashed.
        fs::File::options()
            .write(true)
            .open(&f)
            .expect("open")
            .set_modified(old + Duration::from_secs(5))
            .expect("mtime");
        assert_eq!(
            cache.hash(&client, &f).expect("hash").0,
            BlobHash::of(b"hello world")
        );
        // Different content and size: rehashed.
        write(&f, b"changed content");
        fs::File::options()
            .write(true)
            .open(&f)
            .expect("open")
            .set_modified(old)
            .expect("mtime");
        let (h, size) = cache.hash(&client, &f).expect("hash");
        assert_eq!((h, size), (BlobHash::of(b"changed content"), 15));
    }

    #[test]
    fn hash_cache_does_not_remember_fresh_files() {
        let d = tempfile::tempdir().expect("tmp");
        let f = d.path().join("fresh.rs");
        write(&f, b"just written");
        let client = ClientPath::parse(PathStyle::Posix, "/p/fresh.rs").expect("path");
        let mut cache = HashCache::default();
        let (h, _) = cache.hash(&client, &f).expect("hash");
        assert_eq!(h, BlobHash::of(b"just written"));
        assert!(
            cache.entries.is_empty(),
            "a file modified a moment ago must not be cached"
        );
    }

    #[test]
    fn hash_cache_errors_and_persistence() {
        let d = tempfile::tempdir().expect("tmp");
        let client = ClientPath::parse(PathStyle::Posix, "/p/x").expect("path");
        let mut cache = HashCache::default();
        assert!(cache.hash(&client, &d.path().join("missing")).is_err());
        assert!(
            cache.hash(&client, d.path()).is_err(),
            "a directory is not hashable"
        );
        // The persisted form (postcard) round-trips, including a pre-1970 mtime.
        cache
            .entries
            .insert(client.clone(), (3, -5, BlobHash([1; 32])));
        let bytes = postcard::to_stdvec(&cache).expect("encode");
        let back: HashCache = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back.entries[&client], (3, -5, BlobHash([1; 32])));
    }

    #[test]
    fn hash_streams_files_larger_than_the_read_buffer() {
        let d = tempfile::tempdir().expect("tmp");
        let f = d.path().join("big.bin");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        write(&f, &data);
        let client = ClientPath::parse(PathStyle::Posix, "/p/big.bin").expect("path");
        let (h, size) = HashCache::default().hash(&client, &f).expect("hash");
        assert_eq!(h, BlobHash::of(&data));
        assert_eq!(size, 300_000);
    }

    /// CF-43: a build script that bakes the submitter's literal package path
    /// into a generated `.rs` file in `OUT_DIR` (like `khronos_api`) cannot
    /// compile inside a peer's sandbox.
    #[test]
    fn out_dir_rs_embedding_package_path_is_refused() {
        let d = tempfile::tempdir().expect("tmp");
        let pkg = d.path().join("khronos_api-3.1.0");
        let out = d.path().join("out");
        write(
            &pkg.join("src/lib.rs"),
            b"include!(concat!(env!(\"OUT_DIR\"), \"/exts.rs\"));",
        );
        write(&pkg.join("api_webgl/ext.xml"), b"<xml/>");
        let embedded = format!(
            "pub const X: &[u8] = include_bytes!(r\"{}\");\n",
            pkg.join("api_webgl/ext.xml").display()
        );
        write(&out.join("exts.rs"), embedded.as_bytes());
        assert!(
            matches!(
                collect(&pkg, Some(&out), &[], &[], true),
                Err(CollectError::EmbeddedLocalPath)
            ),
            "OUT_DIR .rs file with hardcoded package path must not go remote"
        );
        // Escaped backslash and forward-slash forms are caught too.
        let escaped = format!(
            "pub const X: &[u8] = include_bytes!(\"{}\");\n",
            pkg.join("api_webgl/ext.xml")
                .display()
                .to_string()
                .replace('\\', "\\\\")
        );
        write(&out.join("exts.rs"), escaped.as_bytes());
        assert!(matches!(
            collect(&pkg, Some(&out), &[], &[], true),
            Err(CollectError::EmbeddedLocalPath)
        ));
        let slashed = format!(
            "pub const X: &[u8] = include_bytes!(\"{}\");\n",
            pkg.join("api_webgl/ext.xml")
                .display()
                .to_string()
                .replace('\\', "/")
        );
        write(&out.join("exts.rs"), slashed.as_bytes());
        assert!(matches!(
            collect(&pkg, Some(&out), &[], &[], true),
            Err(CollectError::EmbeddedLocalPath)
        ));
        // A comment mentioning the path, or normal generated code, stays remote-eligible.
        let comment = format!(
            "// generated from {}\npub const X: &str = \"ok\";\n",
            pkg.display()
        );
        write(&out.join("exts.rs"), comment.as_bytes());
        assert!(collect(&pkg, Some(&out), &[], &[], true).is_ok());

        // Embedding OUT_DIR itself is caught too.
        let out_embed = format!(
            "pub const DATA: &[u8] = include_bytes!(r\"{}\");\n",
            out.join("blob.bin").display()
        );
        write(&out.join("exts.rs"), out_embed.as_bytes());
        assert!(matches!(
            collect(&pkg, Some(&out), &[], &[], true),
            Err(CollectError::EmbeddedLocalPath)
        ));

        // Non-UTF-8 generated `.rs` fails closed (runs locally).
        write(&out.join("exts.rs"), b"\xff\xfeinvalid utf8");
        assert!(matches!(
            collect(&pkg, Some(&out), &[], &[], true),
            Err(CollectError::EmbeddedLocalPath)
        ));

        // OUT_DIR nested inside package_dir (non-target dir) is still checked.
        let nested_out = pkg.join("gen_out");
        let nested_embed = format!(
            "pub const X: &[u8] = include_bytes!(r\"{}\");\n",
            pkg.join("api_webgl/ext.xml").display()
        );
        write(&nested_out.join("nested.rs"), nested_embed.as_bytes());
        assert!(matches!(
            collect(&pkg, Some(&nested_out), &[], &[], true),
            Err(CollectError::EmbeddedLocalPath)
        ));
    }
}
