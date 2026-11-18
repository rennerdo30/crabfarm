//! Package C. Encrypted content-addressed blob store (DESIGN sections 3 and 8).
//!
//! Blobs are stored as XChaCha20-Poly1305(key = submitter blob key, random
//! 24-byte nonce, AAD = blob hash) under `cache/<2 hex>/<64 hex>`. The file
//! records which key owner wrote it (node id) so a blob is only served to jobs
//! of the same submitter. LRU eviction by file mtime above `max_bytes`.

use crate::proto::{BlobHash, NodeId};
use std::path::{Path, PathBuf};

pub struct BlobStore {
    pub dir: PathBuf,
    pub max_bytes: u64,
}

impl BlobStore {
    pub fn open(dir: &Path, max_bytes: u64) -> anyhow::Result<BlobStore> {
        let _ = (dir, max_bytes);
        todo!("package C")
    }

    /// True if a blob of this hash, written by `owner`, is present.
    pub fn has(&self, owner: NodeId, hash: &BlobHash) -> bool {
        let _ = (owner, hash);
        todo!("package C")
    }

    /// Verify `BlobHash::of(plaintext) == hash`, encrypt and store atomically
    /// (temp file + rename). Touches mtime for LRU.
    pub fn put(&self, owner: NodeId, key: &[u8; 32], hash: &BlobHash, plaintext: &[u8]) -> anyhow::Result<()> {
        let _ = (owner, key, hash, plaintext);
        todo!("package C")
    }

    /// Decrypt and return the plaintext; error if missing, wrong owner or
    /// authentication fails. Touches mtime.
    pub fn get(&self, owner: NodeId, key: &[u8; 32], hash: &BlobHash) -> anyhow::Result<Vec<u8>> {
        let _ = (owner, key, hash);
        todo!("package C")
    }

    pub fn remove(&self, owner: NodeId, hash: &BlobHash) -> anyhow::Result<()> {
        let _ = (owner, hash);
        todo!("package C")
    }

    /// Delete least recently used blobs until total size <= max_bytes.
    pub fn evict(&self) -> anyhow::Result<u64> {
        todo!("package C")
    }
}

/// Split plaintext into zstd-compressed `CHUNK_SIZE` chunks for `BlobChunk`
/// frames: returns (offset, last, compressed data).
pub fn chunks(plaintext: &[u8]) -> Vec<(u64, bool, Vec<u8>)> {
    let _ = plaintext;
    todo!("package C")
}

/// Reassembles chunks of one blob; verifies the hash at the end.
#[derive(Default)]
pub struct Assembler {
    // implementation-defined
}

impl Assembler {
    /// Feed one chunk; returns the full plaintext when `last` and verified.
    pub fn push(&mut self, hash: &BlobHash, offset: u64, last: bool, data: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        let _ = (hash, offset, last, data);
        todo!("package C")
    }
}
