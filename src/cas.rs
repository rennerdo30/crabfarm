//! Package C. Encrypted content-addressed blob store (DESIGN sections 3 and 8).
//!
//! Layout: `<dir>/<owner node id hex>/<2 hex>/<64 hex>`. File content is
//! `b"CFB1"` + 24-byte nonce + XChaCha20-Poly1305 ciphertext (key = the
//! submitter's blob key, AAD = the 32 hash bytes). The owner directory keeps
//! blobs of different submitters apart, so a blob is only ever served to jobs
//! of the node whose key encrypted it. LRU eviction by file mtime above
//! `max_bytes`.

use crate::proto::{BlobHash, CHUNK_SIZE, NodeId};
use anyhow::{Context, bail};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

const MAGIC: &[u8; 4] = b"CFB1";
const NONCE_LEN: usize = 24;
/// Largest blob the `Assembler` accepts: the DESIGN section 8 cap of 256 MiB.
pub const MAX_BLOB_BYTES: u64 = 256 * 1024 * 1024;
/// Concurrent assemblers (blobs in flight) and bytes held by them, per node
/// (per process: one node runs per process).
pub const MAX_ASSEMBLERS: usize = 64;
pub const MAX_ASSEMBLER_BYTES: u64 = 1024 * 1024 * 1024;
/// Temp files older than this are leftovers of a crash and are deleted by `evict`.
const STALE_TMP: Duration = Duration::from_secs(24 * 3600);

pub struct BlobStore {
    pub dir: PathBuf,
    pub max_bytes: u64,
    pins: Mutex<HashMap<PathBuf, Pin>>,
}

/// A blob some running job still needs.
#[derive(Default)]
struct Pin {
    count: usize,
    /// `remove` was called while pinned: delete when the last pin goes.
    remove_when_free: bool,
}

/// Keeps the blobs of a running job in the store: `remove` (the `no_retain`
/// cleanup of another job) and `evict` leave them alone until the guard is
/// dropped (CF-10, from CF-14).
pub struct PinGuard<'a> {
    store: &'a BlobStore,
    paths: Vec<PathBuf>,
}

impl Drop for PinGuard<'_> {
    fn drop(&mut self) {
        let mut pins = self.store.pins.lock().unwrap_or_else(|e| e.into_inner());
        for p in &self.paths {
            let Some(pin) = pins.get_mut(p) else { continue };
            pin.count = pin.count.saturating_sub(1);
            if pin.count == 0 {
                let remove = pin.remove_when_free;
                pins.remove(p);
                if remove {
                    let _ = fs::remove_file(p);
                }
            }
        }
    }
}

impl BlobStore {
    pub fn open(dir: &Path, max_bytes: u64) -> anyhow::Result<BlobStore> {
        fs::create_dir_all(dir).with_context(|| format!("create blob store {}", dir.display()))?;
        Ok(BlobStore {
            dir: dir.to_path_buf(),
            max_bytes,
            pins: Mutex::default(),
        })
    }

    /// Pin the blobs of `owner` with these hashes (present or not yet received)
    /// until the guard is dropped. Pins nest: a blob stays while any guard holds it.
    pub fn pin(&self, owner: NodeId, hashes: impl IntoIterator<Item = BlobHash>) -> PinGuard<'_> {
        let mut paths: Vec<PathBuf> = hashes
            .into_iter()
            .map(|h| self.path_of(owner, &h))
            .collect();
        paths.sort();
        paths.dedup();
        let mut pins = self.pins.lock().unwrap_or_else(|e| e.into_inner());
        for p in &paths {
            pins.entry(p.clone()).or_default().count += 1;
        }
        drop(pins);
        PinGuard { store: self, paths }
    }

    /// True when a running job holds this blob.
    fn pinned(&self, path: &Path) -> bool {
        self.pins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(path)
    }

    fn path_of(&self, owner: NodeId, hash: &BlobHash) -> PathBuf {
        let hex = hash.to_hex();
        self.dir.join(owner.to_hex()).join(&hex[..2]).join(hex)
    }

    /// True if a blob of this hash, written by `owner`, is present.
    pub fn has(&self, owner: NodeId, hash: &BlobHash) -> bool {
        self.path_of(owner, hash).is_file()
    }

    /// Verify `BlobHash::of(plaintext) == hash`, encrypt and store atomically
    /// (temp file + rename). Touches mtime for LRU.
    pub fn put(
        &self,
        owner: NodeId,
        key: &[u8; 32],
        hash: &BlobHash,
        plaintext: &[u8],
    ) -> anyhow::Result<()> {
        if BlobHash::of(plaintext) != *hash {
            bail!("blob hash mismatch on put ({hash:?})");
        }
        let path = self.path_of(owner, hash);
        let parent = path.parent().context("blob path has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

        let cipher = XChaCha20Poly1305::new(key.into());
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce[..]);
        let ct = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad: &hash.0,
                },
            )
            .map_err(|_| anyhow::anyhow!("encrypt blob {hash:?}"))?;

        // Temp file in the same directory so the rename stays on one volume
        // and a reader never sees a partial blob.
        let mut tag = [0u8; 8];
        rand::fill(&mut tag[..]);
        let tag: String = tag.iter().map(|b| format!("{b:02x}")).collect();
        let tmp = parent.join(format!(".tmp-{tag}"));
        let write = (|| -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            f.write_all(MAGIC)?;
            f.write_all(&nonce)?;
            f.write_all(&ct)?;
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write {
            let _ = fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("write blob {hash:?}"));
        }
        if let Err(e) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("store blob {hash:?}"));
        }
        touch(&path);
        Ok(())
    }

    /// Decrypt and return the plaintext; error if missing, wrong owner or
    /// authentication fails. Touches mtime.
    pub fn get(&self, owner: NodeId, key: &[u8; 32], hash: &BlobHash) -> anyhow::Result<Vec<u8>> {
        let path = self.path_of(owner, hash);
        let raw = fs::read(&path).with_context(|| format!("read blob {hash:?}"))?;
        if raw.len() < MAGIC.len() + NONCE_LEN || &raw[..MAGIC.len()] != MAGIC {
            bail!("blob {hash:?} has a bad header");
        }
        let (nonce, ct) = raw[MAGIC.len()..].split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().context("nonce length")?;
        let cipher = XChaCha20Poly1305::new(key.into());
        let plain = cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ct,
                    aad: &hash.0,
                },
            )
            .map_err(|_| {
                anyhow::anyhow!("blob {hash:?} failed authentication (wrong key or tampered)")
            })?;
        // The AEAD already binds the hash as AAD; this also catches a blob that
        // was stored under a hash it does not have.
        if BlobHash::of(&plain) != *hash {
            bail!("blob {hash:?} plaintext hash mismatch");
        }
        touch(&path);
        Ok(plain)
    }

    /// Delete a blob. A pinned blob is deleted when its last pin is released.
    pub fn remove(&self, owner: NodeId, hash: &BlobHash) -> anyhow::Result<()> {
        let path = self.path_of(owner, hash);
        {
            let mut pins = self.pins.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pin) = pins.get_mut(&path) {
                pin.remove_when_free = true;
                return Ok(());
            }
        }
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove blob {hash:?}")),
        }
    }

    /// Delete least recently used blobs until total size <= max_bytes.
    /// Returns the number of bytes freed.
    pub fn evict(&self) -> anyhow::Result<u64> {
        let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        let now = SystemTime::now();
        for entry in walkdir::WalkDir::new(&self.dir)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(md) = entry.metadata() else { continue };
            let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if entry.file_name().to_string_lossy().starts_with(".tmp-") {
                if now
                    .duration_since(mtime)
                    .map(|d| d > STALE_TMP)
                    .unwrap_or(false)
                {
                    let _ = fs::remove_file(entry.path());
                }
                continue;
            }
            total += md.len();
            // A running job still reads this one.
            if self.pinned(entry.path()) {
                continue;
            }
            files.push((mtime, md.len(), entry.into_path()));
        }
        files.sort_by_key(|(mtime, _, _)| *mtime);
        let mut freed = 0u64;
        for (_, len, path) in files {
            if total <= self.max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total -= len;
                freed += len;
            }
        }
        Ok(freed)
    }
}

/// Mark a blob as recently used. Failure only degrades LRU order, so it is ignored.
fn touch(path: &Path) {
    if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// Split plaintext into zstd-compressed `CHUNK_SIZE` chunks for `BlobChunk`
/// frames: returns (offset, last, compressed data). An empty blob is one empty
/// last chunk.
pub fn chunks(plaintext: &[u8]) -> Vec<(u64, bool, Vec<u8>)> {
    if plaintext.is_empty() {
        return vec![(0, true, compress(&[]))];
    }
    let n = plaintext.len().div_ceil(CHUNK_SIZE);
    plaintext
        .chunks(CHUNK_SIZE)
        .enumerate()
        .map(|(i, c)| ((i * CHUNK_SIZE) as u64, i + 1 == n, compress(c)))
        .collect()
}

fn compress(data: &[u8]) -> Vec<u8> {
    // Compressing in-memory input does not fail in practice; fall back to a
    // cheaper level rather than panicking.
    zstd::bulk::compress(data, 3)
        .unwrap_or_else(|_| zstd::bulk::compress(data, 1).unwrap_or_default())
}

/// Node-wide limit on blobs being reassembled. A peer opens an assembler by
/// sending a first chunk, so without it one connection could hold unbounded
/// memory by starting many blobs (or many connections could add up).
pub struct AssemblyBudget {
    max_blobs: usize,
    max_bytes: u64,
    blobs: AtomicUsize,
    bytes: AtomicU64,
}

impl AssemblyBudget {
    pub const fn new(max_blobs: usize, max_bytes: u64) -> AssemblyBudget {
        AssemblyBudget {
            max_blobs,
            max_bytes,
            blobs: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    fn acquire_blob(&self) -> bool {
        self.blobs
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max_blobs).then_some(n + 1)
            })
            .is_ok()
    }

    fn acquire_bytes(&self, n: u64) -> bool {
        self.bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |b| {
                b.checked_add(n).filter(|t| *t <= self.max_bytes)
            })
            .is_ok()
    }

    /// Blobs and bytes currently held (tests and diagnostics).
    pub fn in_flight(&self) -> (usize, u64) {
        (
            self.blobs.load(Ordering::Acquire),
            self.bytes.load(Ordering::Acquire),
        )
    }
}

static NODE_BUDGET: AssemblyBudget = AssemblyBudget::new(MAX_ASSEMBLERS, MAX_ASSEMBLER_BYTES);

/// Reassembles chunks of one blob; verifies the hash at the end. Every
/// assembler counts against [`AssemblyBudget`] from its first chunk until it
/// finishes or is dropped.
pub struct Assembler {
    hash: Option<BlobHash>,
    buf: Vec<u8>,
    done: bool,
    limit: u64,
    budget: &'static AssemblyBudget,
    /// Registered with the budget (holds one blob slot).
    counted: bool,
    /// Bytes reserved in the budget.
    held: u64,
}

impl Default for Assembler {
    fn default() -> Self {
        Assembler::with_budget(&NODE_BUDGET)
    }
}

impl Drop for Assembler {
    fn drop(&mut self) {
        self.release();
    }
}

impl Assembler {
    /// An assembler drawing on `budget` instead of the node-wide one.
    pub fn with_budget(budget: &'static AssemblyBudget) -> Assembler {
        Assembler {
            hash: None,
            buf: Vec::new(),
            done: false,
            limit: MAX_BLOB_BYTES,
            budget,
            counted: false,
            held: 0,
        }
    }

    fn release(&mut self) {
        if self.held > 0 {
            self.budget.bytes.fetch_sub(self.held, Ordering::AcqRel);
            self.held = 0;
        }
        if self.counted {
            self.budget.blobs.fetch_sub(1, Ordering::AcqRel);
            self.counted = false;
        }
    }

    /// Feed one chunk; returns the full plaintext when `last` and verified.
    /// Chunks must arrive in order (`offset` equals the bytes received so far).
    pub fn push(
        &mut self,
        hash: &BlobHash,
        offset: u64,
        last: bool,
        data: &[u8],
    ) -> anyhow::Result<Option<Vec<u8>>> {
        if self.done {
            bail!("chunk after the last chunk of {hash:?}");
        }
        match &self.hash {
            Some(h) if h != hash => bail!("chunk for {hash:?} inside the transfer of {h:?}"),
            Some(_) => {}
            None => self.hash = Some(*hash),
        }
        if offset != self.buf.len() as u64 {
            bail!(
                "out-of-order chunk for {hash:?}: offset {offset}, expected {}",
                self.buf.len()
            );
        }
        // Bounded decompression: a chunk can never legitimately exceed CHUNK_SIZE.
        let plain = zstd::bulk::decompress(data, CHUNK_SIZE).context("decompress chunk")?;
        // `chunks()` cuts every blob at CHUNK_SIZE, so only the last chunk may be
        // shorter; many tiny chunks would otherwise be a cheap way to inflate
        // frame and bookkeeping overhead.
        if !last && plain.len() != CHUNK_SIZE {
            bail!("short non-final chunk for {hash:?} ({} bytes)", plain.len());
        }
        if self.buf.len() as u64 + plain.len() as u64 > self.limit {
            bail!("blob {hash:?} exceeds {} bytes", self.limit);
        }
        if !self.counted {
            if !self.budget.acquire_blob() {
                bail!("too many blobs in flight on this node");
            }
            self.counted = true;
        }
        if !self.budget.acquire_bytes(plain.len() as u64) {
            bail!("too many blob bytes in flight on this node");
        }
        self.held += plain.len() as u64;
        self.buf.extend_from_slice(&plain);
        if !last {
            return Ok(None);
        }
        self.done = true;
        let out = std::mem::take(&mut self.buf);
        // The caller owns the plaintext now; the slot is free for the next blob.
        self.release();
        if BlobHash::of(&out) != *hash {
            bail!("blob hash mismatch after assembly ({hash:?})");
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    fn store(max: u64) -> (tempfile::TempDir, BlobStore) {
        let d = tempfile::tempdir().unwrap();
        let s = BlobStore::open(&d.path().join("cache"), max).unwrap();
        (d, s)
    }

    fn put(s: &BlobStore, o: NodeId, key: &[u8; 32], data: &[u8]) -> BlobHash {
        let h = BlobHash::of(data);
        s.put(o, key, &h, data).unwrap();
        h
    }

    /// CF-10: a blob a running job needs survives `remove` and `evict`.
    #[test]
    fn pinned_blobs_survive_remove_and_evict_until_released() {
        let (_d, s) = store(0);
        let key = [7u8; 32];
        let h = put(&s, owner(1), &key, b"input of a running job");
        let other = put(&s, owner(1), &key, b"unpinned");
        let pin = s.pin(owner(1), [h]);
        let nested = s.pin(owner(1), [h]);
        s.remove(owner(1), &h).unwrap();
        s.evict().unwrap();
        assert!(s.has(owner(1), &h), "pinned blob was deleted");
        assert!(
            !s.has(owner(1), &other),
            "evict must still free unpinned blobs"
        );
        drop(pin);
        assert!(s.has(owner(1), &h), "still held by the second pin");
        drop(nested);
        assert!(
            !s.has(owner(1), &h),
            "the requested removal happens when the last pin goes"
        );
        // Pinning also covers a blob that has not arrived yet; without a removal request it stays.
        let later = BlobHash::of(b"arrives later");
        let g = s.pin(owner(1), [later]);
        s.put(owner(1), &key, &later, b"arrives later").unwrap();
        s.evict().unwrap();
        assert!(s.has(owner(1), &later));
        drop(g);
        s.evict().unwrap();
        assert!(!s.has(owner(1), &later), "unpinned again, evicted");
    }

    #[test]
    fn round_trip_and_layout() {
        let (_d, s) = store(u64::MAX);
        let key = [7u8; 32];
        let h = put(&s, owner(1), &key, b"hello blob");
        assert!(s.has(owner(1), &h));
        assert_eq!(s.get(owner(1), &key, &h).unwrap(), b"hello blob");
        let hex = h.to_hex();
        let p = s.dir.join(owner(1).to_hex()).join(&hex[..2]).join(&hex);
        let raw = fs::read(p).unwrap();
        assert_eq!(&raw[..4], b"CFB1");
        assert!(
            !raw.windows(10).any(|w| w == b"hello blob"),
            "stored plaintext"
        );
    }

    #[test]
    fn put_rejects_wrong_hash() {
        let (_d, s) = store(u64::MAX);
        let h = BlobHash::of(b"a");
        assert!(s.put(owner(1), &[1; 32], &h, b"b").is_err());
        assert!(!s.has(owner(1), &h));
    }

    #[test]
    fn wrong_key_fails() {
        let (_d, s) = store(u64::MAX);
        let h = put(&s, owner(1), &[1; 32], b"secret");
        assert!(s.get(owner(1), &[2; 32], &h).is_err());
    }

    #[test]
    fn wrong_owner_fails() {
        let (_d, s) = store(u64::MAX);
        let h = put(&s, owner(1), &[1; 32], b"secret");
        assert!(!s.has(owner(2), &h));
        assert!(s.get(owner(2), &[1; 32], &h).is_err());
    }

    #[test]
    fn tampered_file_fails() {
        let (_d, s) = store(u64::MAX);
        let key = [3u8; 32];
        let h = put(&s, owner(1), &key, b"some content here");
        let p = s.path_of(owner(1), &h);
        let mut raw = fs::read(&p).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        fs::write(&p, &raw).unwrap();
        assert!(s.get(owner(1), &key, &h).is_err());
    }

    #[test]
    fn swapped_file_fails() {
        // A valid blob copied under another hash must not authenticate (AAD).
        let (_d, s) = store(u64::MAX);
        let key = [3u8; 32];
        let a = put(&s, owner(1), &key, b"aaa");
        let b = BlobHash::of(b"bbb");
        let dst = s.path_of(owner(1), &b);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::copy(s.path_of(owner(1), &a), &dst).unwrap();
        assert!(s.get(owner(1), &key, &b).is_err());
    }

    #[test]
    fn remove_is_idempotent() {
        let (_d, s) = store(u64::MAX);
        let h = put(&s, owner(1), &[1; 32], b"x");
        s.remove(owner(1), &h).unwrap();
        s.remove(owner(1), &h).unwrap();
        assert!(!s.has(owner(1), &h));
    }

    #[test]
    fn evicts_oldest_first_and_get_refreshes() {
        let (_d, s) = store(u64::MAX);
        let key = [9u8; 32];
        let hashes: Vec<BlobHash> = (0..4u8)
            .map(|i| put(&s, owner(1), &key, &vec![i; 1000]))
            .collect();
        // Explicit mtimes: 0 oldest .. 3 newest, then use blob 0 so it becomes newest.
        let base = SystemTime::now() - Duration::from_secs(1000);
        for (i, h) in hashes.iter().enumerate() {
            let f = fs::OpenOptions::new()
                .write(true)
                .open(s.path_of(owner(1), h))
                .unwrap();
            f.set_modified(base + Duration::from_secs(i as u64 * 10))
                .unwrap();
        }
        s.get(owner(1), &key, &hashes[0]).unwrap();
        let one = fs::metadata(s.path_of(owner(1), &hashes[0])).unwrap().len();
        let small = BlobStore {
            dir: s.dir.clone(),
            max_bytes: one * 2,
            pins: Mutex::default(),
        };
        let freed = small.evict().unwrap();
        assert_eq!(freed, one * 2);
        assert!(!s.has(owner(1), &hashes[1]));
        assert!(!s.has(owner(1), &hashes[2]));
        assert!(s.has(owner(1), &hashes[0]));
        assert!(s.has(owner(1), &hashes[3]));
    }

    #[test]
    fn chunk_round_trip_large() {
        let data: Vec<u8> = (0..(2 * CHUNK_SIZE + 12345))
            .map(|i| (i % 251) as u8 ^ (i / 7) as u8)
            .collect();
        let h = BlobHash::of(&data);
        let cs = chunks(&data);
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[1].0, CHUNK_SIZE as u64);
        assert!(cs[2].1 && !cs[0].1 && !cs[1].1);
        let mut a = Assembler::default();
        let mut out = None;
        for (off, last, d) in &cs {
            out = a.push(&h, *off, *last, d).unwrap();
        }
        assert_eq!(out.unwrap(), data);
    }

    #[test]
    fn empty_blob_round_trip() {
        let h = BlobHash::of(b"");
        let cs = chunks(b"");
        assert_eq!(cs.len(), 1);
        let mut a = Assembler::default();
        assert_eq!(
            a.push(&h, 0, true, &cs[0].2).unwrap().unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn assembler_rejects_out_of_order() {
        let data = vec![5u8; CHUNK_SIZE + 10];
        let h = BlobHash::of(&data);
        let cs = chunks(&data);
        let mut a = Assembler::default();
        assert!(a.push(&h, cs[1].0, cs[1].1, &cs[1].2).is_err());
        let mut a = Assembler::default();
        a.push(&h, 0, false, &cs[0].2).unwrap();
        assert!(a.push(&h, 0, false, &cs[0].2).is_err(), "repeated offset");
    }

    #[test]
    fn assembler_rejects_hash_mismatch_and_garbage() {
        let data = vec![1u8; 100];
        let cs = chunks(&data);
        let mut a = Assembler::default();
        assert!(a.push(&BlobHash::of(b"other"), 0, true, &cs[0].2).is_err());
        let mut a = Assembler::default();
        assert!(a.push(&BlobHash::of(&data), 0, true, b"not zstd").is_err());
    }

    #[test]
    fn assembler_rejects_oversize_chunk_and_total() {
        let big = vec![0u8; CHUNK_SIZE + 1];
        let comp = zstd::bulk::compress(&big, 3).unwrap();
        let mut a = Assembler::default();
        assert!(
            a.push(&BlobHash::of(&big), 0, true, &comp).is_err(),
            "chunk above CHUNK_SIZE"
        );

        // Same guard as the 256 MiB cap, with a small limit so the test is cheap.
        let data = vec![3u8; 3 * CHUNK_SIZE];
        let h = BlobHash::of(&data);
        let comp = zstd::bulk::compress(&data[..CHUNK_SIZE], 3).unwrap();
        let mut a = Assembler::default();
        a.limit = 2 * CHUNK_SIZE as u64 + 10;
        assert!(a.push(&h, 0, false, &comp).unwrap().is_none());
        assert!(
            a.push(&h, CHUNK_SIZE as u64, false, &comp)
                .unwrap()
                .is_none()
        );
        assert!(
            a.push(&h, 2 * CHUNK_SIZE as u64, false, &comp).is_err(),
            "total above limit"
        );
    }

    #[test]
    fn blob_limit_is_the_design_cap() {
        assert_eq!(MAX_BLOB_BYTES, 256 * 1024 * 1024);
    }

    #[test]
    fn assembler_rejects_short_non_final_chunk() {
        let data = vec![4u8; 2 * CHUNK_SIZE];
        let h = BlobHash::of(&data);
        let short = zstd::bulk::compress(&data[..1000], 3).unwrap();
        let mut a = Assembler::default();
        assert!(
            a.push(&h, 0, false, &short).is_err(),
            "short chunk that is not the last"
        );
        // A short final chunk is normal.
        let tail = vec![4u8; 1000];
        let mut a = Assembler::default();
        assert!(
            a.push(&BlobHash::of(&tail), 0, true, &short)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn assembly_budget_caps_blobs_and_bytes_and_releases() {
        static BLOBS: AssemblyBudget = AssemblyBudget::new(2, u64::MAX);
        let chunk = |n: u8| {
            let d = vec![n; CHUNK_SIZE];
            (BlobHash::of(&d), zstd::bulk::compress(&d, 1).unwrap())
        };
        let (h1, c1) = chunk(1);
        let (h2, c2) = chunk(2);
        let (h3, c3) = chunk(3);
        let (mut a, mut b, mut c) = (
            Assembler::with_budget(&BLOBS),
            Assembler::with_budget(&BLOBS),
            Assembler::with_budget(&BLOBS),
        );
        a.push(&h1, 0, false, &c1).unwrap();
        b.push(&h2, 0, false, &c2).unwrap();
        assert!(
            c.push(&h3, 0, false, &c3).is_err(),
            "third concurrent blob refused"
        );
        assert_eq!(BLOBS.in_flight().0, 2);
        drop(a);
        assert!(c.push(&h3, 0, false, &c3).is_ok(), "slot freed by drop");
        drop((b, c));
        assert_eq!(BLOBS.in_flight(), (0, 0));

        // Bytes: room for one and a half chunks.
        static BYTES: AssemblyBudget =
            AssemblyBudget::new(10, CHUNK_SIZE as u64 + CHUNK_SIZE as u64 / 2);
        let (mut a, mut b) = (
            Assembler::with_budget(&BYTES),
            Assembler::with_budget(&BYTES),
        );
        a.push(&h1, 0, false, &c1).unwrap();
        assert!(
            b.push(&h2, 0, false, &c2).is_err(),
            "second chunk exceeds the byte budget"
        );
        // A finished blob returns its bytes and its slot.
        let small = vec![9u8; 10];
        let mut f = Assembler::with_budget(&BYTES);
        f.push(
            &BlobHash::of(&small),
            0,
            true,
            &zstd::bulk::compress(&small, 1).unwrap(),
        )
        .unwrap()
        .unwrap();
        drop((a, b, f));
        assert_eq!(BYTES.in_flight(), (0, 0));
    }

    #[test]
    fn assembler_rejects_mixed_hashes_and_chunks_after_last() {
        let data = vec![2u8; CHUNK_SIZE + 1];
        let h = BlobHash::of(&data);
        let cs = chunks(&data);
        let mut a = Assembler::default();
        a.push(&h, 0, false, &cs[0].2).unwrap();
        assert!(
            a.push(&BlobHash::of(b"zz"), cs[1].0, true, &cs[1].2)
                .is_err()
        );
        let mut a = Assembler::default();
        a.push(&h, 0, false, &cs[0].2).unwrap();
        a.push(&h, cs[1].0, true, &cs[1].2).unwrap();
        assert!(a.push(&h, data.len() as u64, true, &cs[1].2).is_err());
    }
}
