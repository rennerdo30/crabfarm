//! Dependency graph of produced libraries (DESIGN section 8, item 3).
//!
//! rustc needs the metadata of indirect dependencies too (found through
//! `-L dependency=`), so a remote job must ship the transitive closure of its
//! `--extern` files. The node learns that closure by recording, for every
//! rustc invocation that goes through the wrapper, which library files it
//! produced and which `--extern` files it read.
//!
//! An entry is *pending* from `begin` (the invocation was submitted) until
//! `finish` (it ended and the files were stamped). Pending entries exist so a
//! pipelined dependent, which starts as soon as the metadata of its
//! dependency is announced, can already find the closure; they expire.

use crate::proto::ClientPath;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A pending entry older than this is treated as unknown (its producer died).
const PENDING_TTL_SECS: u64 = 30 * 60;
/// Bound for the persisted map so a long-lived node cannot grow it forever.
const MAX_ENTRIES: usize = 200_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    size: u64,
    mtime_ns: i128,
    externs: Vec<ClientPath>,
    /// Unix seconds of `begin`; set while the producing invocation runs.
    pending_since: Option<u64>,
}

/// Why a closure could not be computed.
#[derive(Debug, PartialEq, Eq)]
pub enum Unknown {
    /// Not produced through a wrapper (built before crabfarm was used).
    NotRecorded(ClientPath),
    /// Produced by us, but the file changed on disk since.
    Changed(ClientPath),
}

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unknown::NotRecorded(p) => write!(f, "dependency {} was not built through crabfarm", p.path),
            Unknown::Changed(p) => write!(f, "dependency {} changed since it was recorded", p.path),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct DepGraph {
    entries: HashMap<ClientPath, Entry>,
    #[serde(skip)]
    dirty: bool,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// (size, mtime in ns since the epoch) of a local file.
fn stamp(path: &Path) -> Option<(u64, i128)> {
    let m = std::fs::metadata(path).ok()?;
    if !m.is_file() {
        return None;
    }
    let ns = match m.modified().ok()?.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    Some((m.len(), ns))
}

/// Proc-macro libraries are dynamic libraries; rlib and rmeta are not.
pub fn is_proc_macro_lib(p: &ClientPath) -> bool {
    let lower = p.path.to_ascii_lowercase();
    lower.ends_with(".dll") || lower.ends_with(".so") || lower.ends_with(".dylib")
}

impl DepGraph {
    /// A missing file is an empty graph; a corrupt one too (it is only a cache).
    pub fn load(path: &Path) -> DepGraph {
        std::fs::read(path).ok().and_then(|b| postcard::from_bytes::<DepGraph>(&b).ok()).unwrap_or_default()
    }

    /// Persist when something changed. Pending entries are not written.
    pub fn save(&mut self, path: &Path) -> anyhow::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let snapshot = DepGraph {
            entries: self.entries.iter().filter(|(_, e)| e.pending_since.is_none()).map(|(k, v)| (k.clone(), v.clone())).collect(),
            dirty: false,
        };
        let bytes = postcard::to_stdvec(&snapshot)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        self.dirty = false;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// An invocation was submitted: its products will have these externs.
    pub fn begin(&mut self, produces: &[ClientPath], externs: &[ClientPath]) {
        self.begin_at(produces, externs, now_secs());
    }

    fn begin_at(&mut self, produces: &[ClientPath], externs: &[ClientPath], now: u64) {
        if self.entries.len() > MAX_ENTRIES {
            self.entries.clear();
        }
        for p in produces {
            self.entries.insert(p.clone(), Entry { size: 0, mtime_ns: 0, externs: externs.to_vec(), pending_since: Some(now) });
        }
    }

    /// The invocation ended. On success the files are stamped; otherwise (or
    /// when a file is missing) the pending entry is dropped.
    pub fn finish(&mut self, produces: &[ClientPath], ok: bool) {
        for p in produces {
            let stamped = if ok { stamp(&p.to_local()) } else { None };
            match (self.entries.get_mut(p), stamped) {
                (Some(e), Some((size, mtime_ns))) => {
                    e.size = size;
                    e.mtime_ns = mtime_ns;
                    e.pending_since = None;
                    self.dirty = true;
                }
                (Some(_), None) => {
                    self.entries.remove(p);
                    self.dirty = true;
                }
                (None, _) => {}
            }
        }
    }

    /// Record a finished invocation in one step (remote results, tests).
    pub fn record(&mut self, produces: &[ClientPath], externs: &[ClientPath]) {
        self.begin(produces, externs);
        self.finish(produces, true);
    }

    /// The `--extern` files plus everything they were built from, transitively.
    /// Every file on the way must be known and unchanged.
    pub fn closure(&self, externs: &[ClientPath]) -> Result<Vec<ClientPath>, Unknown> {
        self.closure_at(externs, now_secs())
    }

    fn closure_at(&self, externs: &[ClientPath], now: u64) -> Result<Vec<ClientPath>, Unknown> {
        let mut seen: HashSet<ClientPath> = HashSet::new();
        let mut order: Vec<ClientPath> = Vec::new();
        let mut stack: Vec<ClientPath> = externs.iter().rev().cloned().collect();
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            let e = self.entries.get(&p).ok_or_else(|| Unknown::NotRecorded(p.clone()))?;
            match e.pending_since {
                Some(t) if now.saturating_sub(t) <= PENDING_TTL_SECS => {}
                Some(_) => return Err(Unknown::NotRecorded(p)),
                None => {
                    if stamp(&p.to_local()) != Some((e.size, e.mtime_ns)) {
                        return Err(Unknown::Changed(p));
                    }
                }
            }
            for x in e.externs.iter().rev() {
                stack.push(x.clone());
            }
            order.push(p);
        }
        Ok(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(p: &Path) -> ClientPath {
        ClientPath::from_local(p, Path::new(".")).expect("absolute test path")
    }

    fn make(dir: &Path, name: &str, body: &str) -> ClientPath {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        cp(&p)
    }

    #[test]
    fn closure_is_transitive_and_deduplicated() {
        let d = tempfile::tempdir().unwrap();
        let a = make(d.path(), "liba.rlib", "a");
        let b = make(d.path(), "libb.rlib", "b");
        let c = make(d.path(), "libc.rlib", "c");
        let mut g = DepGraph::default();
        g.record(std::slice::from_ref(&a), &[]);
        g.record(std::slice::from_ref(&b), std::slice::from_ref(&a));
        g.record(std::slice::from_ref(&c), &[a.clone(), b.clone()]);
        let got = g.closure(std::slice::from_ref(&c)).unwrap();
        assert_eq!(got.len(), 3);
        for p in [&a, &b, &c] {
            assert!(got.contains(p));
        }
    }

    #[test]
    fn unknown_file_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let a = make(d.path(), "liba.rlib", "a");
        let g = DepGraph::default();
        assert_eq!(g.closure(std::slice::from_ref(&a)), Err(Unknown::NotRecorded(a)));
    }

    #[test]
    fn unknown_indirect_dependency_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let a = make(d.path(), "liba.rlib", "a");
        let b = make(d.path(), "libb.rlib", "b");
        let mut g = DepGraph::default();
        // b was built from a, but a itself was never seen.
        g.record(std::slice::from_ref(&b), std::slice::from_ref(&a));
        assert_eq!(g.closure(std::slice::from_ref(&b)), Err(Unknown::NotRecorded(a)));
    }

    #[test]
    fn changed_file_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let a = make(d.path(), "liba.rlib", "a");
        let mut g = DepGraph::default();
        g.record(std::slice::from_ref(&a), &[]);
        assert!(g.closure(std::slice::from_ref(&a)).is_ok());
        // Different size, so the check does not depend on mtime granularity.
        std::fs::write(a.to_local(), "aaaa").unwrap();
        assert_eq!(g.closure(std::slice::from_ref(&a)), Err(Unknown::Changed(a)));
    }

    #[test]
    fn pending_entry_counts_until_it_expires() {
        let d = tempfile::tempdir().unwrap();
        let a = make(d.path(), "liba.rmeta", "a");
        let mut g = DepGraph::default();
        g.begin_at(std::slice::from_ref(&a), &[], 1000);
        assert!(g.closure_at(std::slice::from_ref(&a), 1100).is_ok());
        assert_eq!(g.closure_at(std::slice::from_ref(&a), 1000 + PENDING_TTL_SECS + 1), Err(Unknown::NotRecorded(a)));
    }

    #[test]
    fn failed_invocation_drops_its_entry() {
        let d = tempfile::tempdir().unwrap();
        let a = make(d.path(), "liba.rlib", "a");
        let mut g = DepGraph::default();
        g.begin(std::slice::from_ref(&a), &[]);
        g.finish(std::slice::from_ref(&a), false);
        assert!(g.closure(std::slice::from_ref(&a)).is_err());
    }

    #[test]
    fn persists_stamped_entries_only() {
        let d = tempfile::tempdir().unwrap();
        let a = make(d.path(), "liba.rlib", "a");
        let b = make(d.path(), "libb.rlib", "b");
        let mut g = DepGraph::default();
        g.record(std::slice::from_ref(&a), &[]);
        g.begin(std::slice::from_ref(&b), &[]);
        let file = d.path().join("depgraph.bin");
        g.save(&file).unwrap();
        let back = DepGraph::load(&file);
        assert!(back.closure(std::slice::from_ref(&a)).is_ok());
        assert!(back.closure(std::slice::from_ref(&b)).is_err());
    }

    #[test]
    fn proc_macro_libraries_are_recognised() {
        let p = |s: &str| ClientPath { style: crate::proto::PathStyle::Posix, path: s.into() };
        assert!(is_proc_macro_lib(&p("/t/deps/derive_mac-1.dll")));
        assert!(is_proc_macro_lib(&p("/t/deps/libderive_mac-1.so")));
        assert!(!is_proc_macro_lib(&p("/t/deps/libcore_lib-1.rlib")));
        assert!(!is_proc_macro_lib(&p("/t/deps/libcore_lib-1.rmeta")));
    }
}
