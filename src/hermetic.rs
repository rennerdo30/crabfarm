//! Crates proven hermetic (DESIGN section 16, CF-28).
//!
//! A proc macro or any build-time code can read a file that is neither in the
//! dep-info nor in the package directory (sqlx's offline data in the workspace
//! root, say), so a result of a LOCAL rustc run may be stale the next time the
//! cache key matches. A result of a REMOTE run cannot: the peer's sandbox held
//! only the files that were shipped. So a local result is stored only for a
//! crate that has already built successfully on a peer: its identity is
//! "proven hermetic". When a remote run of such a crate fails and the local
//! rerun succeeds, it needs something outside the sandbox and the proof is
//! revoked.
//!
//! The proofs are keyed by [`crate::ccache::crate_identity`], kept in
//! `<data>/hermetic.txt` (one `<hex> <unix seconds>` line each), bounded, and
//! replaced atomically. The file is in a directory other programs may write
//! to, so everything read from it is capped and sanitized.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Identities remembered at most; the least recently proven are dropped first.
pub const MAX_IDENTITIES: usize = 8192;
/// Larger files are ignored: nothing here is that big.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// A proof that is already this fresh is not rewritten on every remote success.
const REFRESH_AFTER_SECS: u64 = 24 * 60 * 60;

pub struct Hermetic {
    path: PathBuf,
    ids: BTreeMap<String, u64>,
}

fn valid_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn parse(text: &str) -> BTreeMap<String, u64> {
    let mut ids = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(id), Some(secs)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let (true, Ok(secs)) = (valid_id(id), secs.parse::<u64>()) {
            ids.insert(id.to_string(), secs);
        }
    }
    trim(&mut ids);
    ids
}

/// Keep the [`MAX_IDENTITIES`] most recently proven.
fn trim(ids: &mut BTreeMap<String, u64>) {
    if ids.len() <= MAX_IDENTITIES {
        return;
    }
    let mut by_age: Vec<(u64, String)> = ids.iter().map(|(k, v)| (*v, k.clone())).collect();
    by_age.sort();
    for (_, k) in by_age.into_iter().take(ids.len() - MAX_IDENTITIES) {
        ids.remove(&k);
    }
}

impl Hermetic {
    /// Load `<data>/hermetic.txt`; nothing is proven when it is missing or damaged.
    pub fn load(data_dir: &Path) -> Hermetic {
        use std::io::Read;
        let path = data_dir.join("hermetic.txt");
        let mut text = String::new();
        let ok = std::fs::File::open(&path)
            .map(|f| f.take(MAX_FILE_BYTES + 1).read_to_string(&mut text))
            .is_ok_and(|r| r.is_ok())
            && text.len() as u64 <= MAX_FILE_BYTES;
        Hermetic {
            path,
            ids: if ok { parse(&text) } else { BTreeMap::new() },
        }
    }

    pub fn is_proven(&self, id: &str) -> bool {
        self.ids.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Record a successful remote run of `id`. Returns whether the file changed
    /// (a persistence error is returned; the proof then holds for this run only).
    pub fn prove(&mut self, id: &str, now: u64) -> anyhow::Result<bool> {
        if !valid_id(id) {
            return Ok(false);
        }
        match self.ids.get(id) {
            Some(t) if now.saturating_sub(*t) < REFRESH_AFTER_SECS => return Ok(false),
            _ => {}
        }
        self.ids.insert(id.to_string(), now);
        trim(&mut self.ids);
        self.save()?;
        Ok(true)
    }

    /// Withdraw the proof: a remote run failed and the local rerun worked.
    pub fn revoke(&mut self, id: &str) -> anyhow::Result<bool> {
        if self.ids.remove(id).is_none() {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> anyhow::Result<()> {
        use anyhow::Context;
        let mut text = String::new();
        for (id, secs) in &self.ids {
            text.push_str(id);
            text.push(' ');
            text.push_str(&secs.to_string());
            text.push('\n');
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replace {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> String {
        format!("{n:064x}")
    }

    #[test]
    fn a_proof_survives_a_restart_and_a_revocation_too() {
        let d = tempfile::tempdir().unwrap();
        let mut h = Hermetic::load(d.path());
        assert!(!h.is_proven(&id(1)));
        assert!(h.prove(&id(1), 100).unwrap());
        assert!(h.prove(&id(2), 100).unwrap());
        assert!(Hermetic::load(d.path()).is_proven(&id(1)));
        assert!(h.revoke(&id(1)).unwrap());
        assert!(!h.revoke(&id(1)).unwrap(), "already gone");
        let again = Hermetic::load(d.path());
        assert!(!again.is_proven(&id(1)) && again.is_proven(&id(2)));
    }

    #[test]
    fn a_fresh_proof_is_not_rewritten() {
        let d = tempfile::tempdir().unwrap();
        let mut h = Hermetic::load(d.path());
        assert!(h.prove(&id(1), 100).unwrap());
        assert!(!h.prove(&id(1), 200).unwrap());
        assert!(h.prove(&id(1), 100 + REFRESH_AFTER_SECS).unwrap());
    }

    #[test]
    fn the_set_is_bounded_and_drops_the_oldest() {
        let d = tempfile::tempdir().unwrap();
        let mut h = Hermetic::load(d.path());
        for n in 0..(MAX_IDENTITIES as u32 + 3) {
            h.ids.insert(id(n), 1000 + u64::from(n));
        }
        trim(&mut h.ids);
        assert_eq!(h.len(), MAX_IDENTITIES);
        assert!(!h.is_proven(&id(0)) && !h.is_proven(&id(2)));
        assert!(h.is_proven(&id(3)));
    }

    #[test]
    fn a_damaged_or_hostile_file_proves_nothing_it_should_not() {
        let d = tempfile::tempdir().unwrap();
        let good = id(7);
        let text = format!("garbage\n{good} 5\nNOTHEX 1\n{} x\n{}\n", id(8), id(9));
        std::fs::write(d.path().join("hermetic.txt"), text).unwrap();
        let h = Hermetic::load(d.path());
        assert!(h.is_proven(&good));
        assert_eq!(h.len(), 1);
        std::fs::write(
            d.path().join("hermetic.txt"),
            vec![b'a'; MAX_FILE_BYTES as usize + 1],
        )
        .unwrap();
        assert!(Hermetic::load(d.path()).is_empty(), "too big is ignored");
    }
}
