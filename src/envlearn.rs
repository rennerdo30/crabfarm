//! Environment dependencies a crate was seen to have (CF-18, DESIGN section 9).
//!
//! When a crate reads an environment variable that the farm does not forward
//! (`env!`, `option_env!`, `include!(env!(..))`), a peer builds it without that
//! value and the result is wrong. The wrapper finds such names in the `.d` file
//! that its OWN local rustc wrote (never in one a peer returned) and records
//! them here, per crate, in `<data>/env_forward.toml`. A recorded name is never
//! forwarded because of that: the file only remembers that this crate must
//! build locally while the variable is set, so the crate does not bounce
//! between a remote run and a local rerun on every build. A name travels to a
//! peer only when the user lists it in `forward_env`.
//!
//! The file lives in a directory other programs may write to, so everything
//! read from it is capped and sanitized, and it is replaced atomically under a
//! lock (several wrappers run at once).

use anyhow::Context;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Crates remembered at most; a new crate is not recorded beyond this.
pub const MAX_CRATES: usize = 4096;
/// Names remembered per crate.
pub const MAX_NAMES_PER_CRATE: usize = 64;
/// Longest variable name kept (and longest crate name used as a key).
pub const MAX_NAME_LEN: usize = 128;
/// Larger files are ignored: nothing here is that big.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

type Table = BTreeMap<String, Vec<String>>;

fn file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("env_forward.toml")
}

/// A usable environment variable name: no `=`, no NUL or control characters,
/// not empty, not longer than [`MAX_NAME_LEN`].
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= MAX_NAME_LEN && !n.contains('=') && !n.chars().any(char::is_control)
}

/// Parse and sanitize file contents: wrong shapes and entries over the caps
/// are dropped, never trusted.
fn parse(text: &str) -> Table {
    let raw: Table = toml::from_str(text).unwrap_or_default();
    let mut out = Table::new();
    for (krate, names) in raw {
        if out.len() >= MAX_CRATES {
            break;
        }
        if !valid_name(&krate) {
            continue;
        }
        let mut kept: Vec<String> = Vec::new();
        for n in names {
            if kept.len() >= MAX_NAMES_PER_CRATE {
                break;
            }
            if valid_name(&n) && !kept.contains(&n) {
                kept.push(n);
            }
        }
        if !kept.is_empty() {
            out.insert(krate, kept);
        }
    }
    out
}

fn load(data_dir: &Path) -> Table {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(file_path(data_dir)) else {
        return Table::new();
    };
    let mut text = String::new();
    if file
        .take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .is_err()
        || text.len() as u64 > MAX_FILE_BYTES
    {
        return Table::new();
    }
    parse(&text)
}

/// Names recorded for `crate_name`.
pub fn recorded(data_dir: &Path, crate_name: &str) -> Vec<String> {
    load(data_dir).remove(crate_name).unwrap_or_default()
}

/// Snapshot of all recorded crate -> variable-name entries (names only, never values),
/// capped and sanitized by [`load`].
pub fn summary(data_dir: &Path) -> BTreeMap<String, Vec<String>> {
    load(data_dir)
}

/// Exclusive lock on `env_forward.lock`, released on drop.
struct Lock(std::fs::File);

impl Lock {
    fn acquire(data_dir: &Path) -> anyhow::Result<Lock> {
        let path = data_dir.join("env_forward.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Lock(file)),
                Err(std::fs::TryLockError::WouldBlock) => {
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "timed out waiting for env_forward.lock"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e).context("lock"),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Record `names` for `crate_name`. Invalid names and anything beyond the caps
/// are not recorded. The file is replaced atomically (temporary file in the
/// same directory, then rename).
pub fn remember(data_dir: &Path, crate_name: &str, names: &[String]) -> anyhow::Result<()> {
    if !valid_name(crate_name) {
        return Ok(());
    }
    let _lock = Lock::acquire(data_dir)?;
    let mut table = load(data_dir);
    if !table.contains_key(crate_name) && table.len() >= MAX_CRATES {
        return Ok(());
    }
    let entry = table.entry(crate_name.to_string()).or_default();
    for n in names {
        if entry.len() >= MAX_NAMES_PER_CRATE {
            break;
        }
        if valid_name(n) && !entry.contains(n) {
            entry.push(n.clone());
        }
    }
    if entry.is_empty() {
        table.remove(crate_name);
    }
    let text = toml::to_string(&table).context("encode env_forward.toml")?;
    let mut tag = [0u8; 6];
    rand::fill(&mut tag[..]);
    let tmp = data_dir.join(format!(
        "env_forward.{}.tmp",
        data_encoding::HEXLOWER.encode(&tag)
    ));
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, file_path(data_dir)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("replace env_forward.toml");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn names_are_recorded_per_crate_and_deduplicated() {
        let d = tempfile::tempdir().unwrap();
        remember(d.path(), "a", &names(&["X", "Y"])).unwrap();
        remember(d.path(), "a", &names(&["Y", "Z"])).unwrap();
        remember(d.path(), "b", &names(&["Q"])).unwrap();
        assert_eq!(recorded(d.path(), "a"), names(&["X", "Y", "Z"]));
        assert_eq!(recorded(d.path(), "b"), names(&["Q"]));
        assert!(recorded(d.path(), "c").is_empty());
        // No temporary file is left behind.
        let left: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn entry_count_and_name_length_are_capped() {
        let d = tempfile::tempdir().unwrap();
        let many: Vec<String> = (0..MAX_NAMES_PER_CRATE + 10)
            .map(|i| format!("V{i}"))
            .collect();
        remember(d.path(), "a", &many).unwrap();
        assert_eq!(recorded(d.path(), "a").len(), MAX_NAMES_PER_CRATE);
        remember(d.path(), "b", &["L".repeat(MAX_NAME_LEN + 1)]).unwrap();
        assert!(recorded(d.path(), "b").is_empty(), "over-long name kept");
        remember(d.path(), "c", &names(&["A=B", "", "bad\nname"])).unwrap();
        assert!(recorded(d.path(), "c").is_empty(), "invalid name kept");
    }

    #[test]
    fn a_hand_edited_file_over_the_caps_is_cut_down_when_read() {
        let d = tempfile::tempdir().unwrap();
        let mut text = String::new();
        for i in 0..MAX_CRATES + 50 {
            text.push_str(&format!("c{i:05} = [\"V\"]\n"));
        }
        text.push_str(&format!("long = [\"{}\"]\n", "N".repeat(MAX_NAME_LEN + 1)));
        std::fs::write(file_path(d.path()), text).unwrap();
        assert!(load(d.path()).len() <= MAX_CRATES);
        assert!(recorded(d.path(), "long").is_empty());
        // A crate beyond the cap is not added to a full table.
        std::fs::write(file_path(d.path()), {
            let mut t = String::new();
            for i in 0..MAX_CRATES {
                t.push_str(&format!("c{i:05} = [\"V\"]\n"));
            }
            t
        })
        .unwrap();
        remember(d.path(), "zzz-new", &names(&["X"])).unwrap();
        assert!(recorded(d.path(), "zzz-new").is_empty());
    }

    #[test]
    fn a_huge_file_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            file_path(d.path()),
            "a = [\"X\"]\n".to_string() + &"# pad\n".repeat(300_000),
        )
        .unwrap();
        assert!(recorded(d.path(), "a").is_empty());
    }

    #[test]
    fn concurrent_writers_do_not_lose_names() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || remember(&p, "a", &[format!("V{i}")]).unwrap())
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(recorded(&path, "a").len(), 8);
    }
}
