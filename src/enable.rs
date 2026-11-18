//! `crabfarm enable` / `disable`: make plain `cargo build` use the farm by
//! writing `build.rustc-wrapper` (and `build.jobs`) into a cargo config file.
//!
//! The file is edited with `toml_edit`, so comments and layout survive. A
//! wrapper that was configured before (sccache) is kept as the inner wrapper
//! in `[env] CRABFARM_INNER_WRAPPER`: cargo applies `[env]` to the wrapper
//! process too (verified with cargo 1.99.1, CF-22), and the wrapper already
//! chains that variable for its local runs (CF-19). `disable` puts the
//! original value back exactly.

use crate::cargo_cmd::{INNER_WRAPPER_ENV, MAX_JOBS, is_crabfarm};
use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value, value};

/// Trailing comment on the `jobs` line that crabfarm wrote.
const JOBS_MARKER: &str = " # crabfarm enable";

/// Nearest directory at or above `start` with a `Cargo.toml`.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

/// `<dir>/config.toml`, or the legacy `<dir>/config` when only that exists.
pub fn config_file_in(cargo_dir: &Path) -> PathBuf {
    let new = cargo_dir.join("config.toml");
    let old = cargo_dir.join("config");
    if !new.exists() && old.is_file() {
        old
    } else {
        new
    }
}

/// The config file `enable`/`disable` act on: the project's `.cargo` or, with
/// `global`, `<cargo_home>/config.toml`.
pub fn target_config(
    global: bool,
    cwd: &Path,
    cargo_home: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    if global {
        let home = cargo_home.context("cannot tell CARGO_HOME; set it and try again")?;
        return Ok(config_file_in(home));
    }
    let root = find_project_root(cwd).with_context(|| {
        format!(
            "{} is not inside a Cargo project (no Cargo.toml here or above); \
             run this in a project, or use --global",
            cwd.display()
        )
    })?;
    Ok(config_file_in(&root.join(".cargo")))
}

/// Jobs when the user gave none: local slots plus the free remote slots of
/// the running node, else twice the local slots; capped.
pub fn default_jobs(
    local_slots: u32,
    peers: Option<&[(crate::proto::PeerInfo, Option<crate::proto::Status>)]>,
) -> u32 {
    match peers {
        Some(p) => crate::cargo_cmd::farm_jobs(local_slots, p),
        None => local_slots.saturating_mul(2).clamp(1, MAX_JOBS),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct EnableReport {
    pub file: PathBuf,
    /// The `jobs` now in the file, and whether crabfarm set it (false: the
    /// user's own value was kept).
    pub jobs: Option<(i64, bool)>,
    /// The wrapper that was moved to the inner position.
    pub inner: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DisableReport {
    pub file: PathBuf,
    /// crabfarm was the wrapper and is gone now.
    pub was_enabled: bool,
    /// The wrapper that took crabfarm's place again.
    pub restored: Option<String>,
}

fn read_doc(path: &Path) -> anyhow::Result<DocumentMut> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    text.parse::<DocumentMut>()
        .with_context(|| format!("{} is not valid TOML; not touching it", path.display()))
}

fn write_if_changed(path: &Path, doc: &DocumentMut) -> anyhow::Result<()> {
    let new = doc.to_string();
    if std::fs::read_to_string(path).ok().as_deref() == Some(new.as_str()) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // Replace in one step so an editor or cargo never sees half a file.
    let tmp = path.with_extension("crabfarm-tmp");
    std::fs::write(&tmp, new).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

fn table_mut<'a>(
    doc: &'a mut DocumentMut,
    name: &str,
    file: &Path,
) -> anyhow::Result<&'a mut dyn toml_edit::TableLike> {
    let item = doc.entry(name).or_insert(Item::Table(Table::new()));
    item.as_table_like_mut()
        .with_context(|| format!("`{name}` in {} is not a table", file.display()))
}

fn str_of(item: &Item) -> Option<&str> {
    item.as_str()
}

/// The inner wrapper as stored in `[env]` (plain string or `{ value = .. }`).
fn env_inner(env: &dyn toml_edit::TableLike) -> Option<String> {
    let item = env.get(INNER_WRAPPER_ENV)?;
    if let Some(s) = item.as_str() {
        return Some(s.to_string());
    }
    item.as_table_like()?
        .get("value")?
        .as_str()
        .map(str::to_string)
}

/// `Programs/x` style values are relative to the folder above `.cargo`; the
/// `relative` flag makes cargo resolve them the same way for `[env]`.
fn env_item_for(wrapper: &str) -> Item {
    if wrapper.contains(['/', '\\']) && !Path::new(wrapper).is_absolute() {
        let mut t = InlineTable::new();
        t.insert("value", wrapper.into());
        t.insert("relative", true.into());
        Item::Value(Value::InlineTable(t))
    } else {
        value(wrapper)
    }
}

/// Replace a value, keeping the comment that trailed the old one.
fn set_keep_decor(table: &mut dyn toml_edit::TableLike, key: &str, mut new: Value) {
    if let Some(old) = table.get(key).and_then(Item::as_value) {
        *new.decor_mut() = old.decor().clone();
    }
    table.insert(key, Item::Value(new));
}

fn jobs_marker(item: &Item) -> Option<Option<i64>> {
    let suffix = item.as_value()?.decor().suffix()?.as_str()?;
    let rest = suffix.trim().strip_prefix("# crabfarm enable")?.trim();
    if rest.is_empty() {
        return Some(None);
    }
    let was = rest.strip_prefix("(was ")?.strip_suffix(')')?;
    Some(was.trim().parse().ok())
}

/// Write the wrapper (and `jobs`) into `file`. `jobs`: `Some(n)` was asked
/// for explicitly and replaces the file's value (remembering it); `None`
/// keeps a value that is already there, else writes `default_jobs`.
pub fn enable_file(
    file: &Path,
    exe: &Path,
    explicit_jobs: Option<u32>,
    default_jobs: u32,
) -> anyhow::Result<EnableReport> {
    anyhow::ensure!(
        exe.is_absolute(),
        "the crabfarm executable path must be absolute"
    );
    let exe_str = exe.to_string_lossy().into_owned();
    let mut doc = read_doc(file)?;
    let mut inner = None;

    let existing = {
        let build = table_mut(&mut doc, "build", file)?;
        build.get("rustc-wrapper").map(|i| match str_of(i) {
            Some(s) => Ok(s.to_string()),
            None => Err(anyhow::anyhow!(
                "build.rustc-wrapper in {} is not a string; not touching it",
                file.display()
            )),
        })
    }
    .transpose()?;

    match existing {
        // Already crabfarm (maybe another install dir): refresh the path only.
        Some(w) if is_crabfarm(&w, Some(exe)) => {}
        Some(other) => {
            let env = table_mut(&mut doc, "env", file)?;
            if env.contains_key(INNER_WRAPPER_ENV) {
                bail!(
                    "{} already has [env] {INNER_WRAPPER_ENV} next to a different build.rustc-wrapper; \
                     resolve that by hand first",
                    file.display()
                );
            }
            env.insert(INNER_WRAPPER_ENV, env_item_for(&other));
            inner = Some(other);
        }
        None => {}
    }
    {
        let build = table_mut(&mut doc, "build", file)?;
        set_keep_decor(build, "rustc-wrapper", Value::from(exe_str));
    }

    let jobs = {
        let build = table_mut(&mut doc, "build", file)?;
        let current = build.get("jobs").cloned();
        let current_int = current.as_ref().and_then(|i| i.as_integer());
        let ours = current.as_ref().and_then(jobs_marker);
        match (explicit_jobs, current_int) {
            (Some(n), cur) => {
                // Remember what the user had; keep an older memory on re-runs.
                let was = match (&ours, cur) {
                    (Some(prev), _) => *prev,
                    (None, c) => c,
                };
                let mut v = Value::from(i64::from(n));
                let suffix = match was {
                    Some(w) => format!("{JOBS_MARKER} (was {w})"),
                    None => JOBS_MARKER.to_string(),
                };
                v.decor_mut().set_suffix(suffix);
                build.insert("jobs", Item::Value(v));
                Some((i64::from(n), true))
            }
            (None, Some(cur)) => Some((cur, ours.is_some())),
            (None, None) => {
                let mut v = Value::from(i64::from(default_jobs));
                v.decor_mut().set_suffix(JOBS_MARKER);
                build.insert("jobs", Item::Value(v));
                Some((i64::from(default_jobs), true))
            }
        }
    };

    write_if_changed(file, &doc)?;
    Ok(EnableReport {
        file: file.to_path_buf(),
        jobs,
        inner,
    })
}

/// Undo `enable_file`: give the earlier wrapper and `jobs` back, or remove
/// what crabfarm added. A file crabfarm is not in is left alone.
pub fn disable_file(file: &Path) -> anyhow::Result<DisableReport> {
    let mut report = DisableReport {
        file: file.to_path_buf(),
        was_enabled: false,
        restored: None,
    };
    if !file.exists() {
        return Ok(report);
    }
    let mut doc = read_doc(file)?;
    let wrapper_is_ours = doc
        .get("build")
        .and_then(Item::as_table_like)
        .and_then(|b| b.get("rustc-wrapper"))
        .and_then(Item::as_str)
        .is_some_and(|w| is_crabfarm(w, None));
    if !wrapper_is_ours {
        return Ok(report);
    }
    report.was_enabled = true;

    // The wrapper that was in place before, if enable moved one into [env].
    let inner = doc
        .get("env")
        .and_then(Item::as_table_like)
        .and_then(env_inner);
    {
        let build = table_mut(&mut doc, "build", file)?;
        match &inner {
            Some(w) => set_keep_decor(build, "rustc-wrapper", Value::from(w.clone())),
            None => {
                build.remove("rustc-wrapper");
            }
        }
        match build.get("jobs").and_then(jobs_marker) {
            Some(Some(was)) => {
                let mut v = Value::from(was);
                // Back to the user's line without our comment.
                v.decor_mut().clear();
                v.decor_mut().set_prefix(" ");
                build.insert("jobs", Item::Value(v));
            }
            Some(None) => {
                build.remove("jobs");
            }
            None => {}
        }
        if build.is_empty() {
            doc.remove("build");
        }
    }
    if inner.is_some()
        && let Some(env) = doc.get_mut("env").and_then(Item::as_table_like_mut)
    {
        env.remove(INNER_WRAPPER_ENV);
        if env.is_empty() {
            doc.remove("env");
        }
    }
    report.restored = inner;
    write_if_changed(file, &doc)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = if cfg!(windows) {
        r"C:\Users\a\AppData\Local\Programs\crabfarm\crabfarm.exe"
    } else {
        "/home/a/.local/bin/crabfarm"
    };

    fn exe() -> PathBuf {
        PathBuf::from(EXE)
    }

    fn cfg_in(dir: &Path, text: &str) -> PathBuf {
        let f = dir.join(".cargo/config.toml");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, text).unwrap();
        f
    }

    fn parsed(f: &Path) -> toml::Table {
        toml::from_str(&std::fs::read_to_string(f).unwrap()).unwrap()
    }

    #[test]
    fn enable_creates_the_file_and_disable_removes_everything() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".cargo/config.toml");
        let r = enable_file(&f, &exe(), None, 24).unwrap();
        assert_eq!(r.jobs, Some((24, true)));
        let t = parsed(&f);
        assert_eq!(t["build"]["rustc-wrapper"].as_str(), Some(EXE));
        assert_eq!(t["build"]["jobs"].as_integer(), Some(24));
        let r = disable_file(&f).unwrap();
        assert!(r.was_enabled);
        assert_eq!(std::fs::read_to_string(&f).unwrap().trim(), "");
    }

    #[test]
    fn round_trip_keeps_comments_and_other_settings_byte_for_byte() {
        let d = tempfile::tempdir().unwrap();
        let original = "# my project\n[build]\n# keep this\ntarget-dir = \"out\"   # trailing\n\n[net]\nretry = 2\n\n[alias]\nb = \"build\" # short\n";
        let f = cfg_in(d.path(), original);
        enable_file(&f, &exe(), Some(16), 0).unwrap();
        let during = std::fs::read_to_string(&f).unwrap();
        for keep in [
            "# my project",
            "# keep this",
            "# trailing",
            "# short",
            "retry = 2",
        ] {
            assert!(during.contains(keep), "lost `{keep}` in:\n{during}");
        }
        assert!(during.contains(EXE));
        disable_file(&f).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), original);
    }

    #[test]
    fn existing_sccache_becomes_the_inner_wrapper_and_comes_back_exactly() {
        let d = tempfile::tempdir().unwrap();
        let original = "[build]\nrustc-wrapper = \"sccache\" # shared cache\njobs = 8\n";
        let f = cfg_in(d.path(), original);
        let r = enable_file(&f, &exe(), Some(32), 0).unwrap();
        assert_eq!(r.inner.as_deref(), Some("sccache"));
        let t = parsed(&f);
        assert_eq!(t["build"]["rustc-wrapper"].as_str(), Some(EXE));
        assert_eq!(t["env"][INNER_WRAPPER_ENV].as_str(), Some("sccache"));
        assert_eq!(t["build"]["jobs"].as_integer(), Some(32));
        // Enabling again changes nothing and never chains crabfarm to itself.
        let again = std::fs::read_to_string(&f).unwrap();
        enable_file(&f, &exe(), Some(32), 0).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), again);
        assert_ne!(
            parsed(&f)["env"][INNER_WRAPPER_ENV].as_str(),
            Some(EXE),
            "crabfarm must never be its own inner wrapper"
        );
        let r = disable_file(&f).unwrap();
        assert_eq!(r.restored.as_deref(), Some("sccache"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), original);
    }

    #[test]
    fn a_relative_inner_wrapper_keeps_its_meaning_in_env() {
        let d = tempfile::tempdir().unwrap();
        let original = "[build]\nrustc-wrapper = \"tools/wrap.sh\"\n";
        let f = cfg_in(d.path(), original);
        enable_file(&f, &exe(), Some(4), 0).unwrap();
        let during = std::fs::read_to_string(&f).unwrap();
        assert!(during.contains("relative = true"), "{during}");
        disable_file(&f).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), original);
    }

    #[test]
    fn a_users_jobs_survive_unless_asked_and_come_back() {
        let d = tempfile::tempdir().unwrap();
        let f = cfg_in(d.path(), "[build]\njobs = 8\n");
        // No --jobs: the user's value stays and disable leaves it.
        let r = enable_file(&f, &exe(), None, 40).unwrap();
        assert_eq!(r.jobs, Some((8, false)));
        disable_file(&f).unwrap();
        assert_eq!(parsed(&f)["build"]["jobs"].as_integer(), Some(8));
        // --jobs: replaced, remembered, restored.
        enable_file(&f, &exe(), Some(64), 40).unwrap();
        assert_eq!(parsed(&f)["build"]["jobs"].as_integer(), Some(64));
        disable_file(&f).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "[build]\njobs = 8\n");
    }

    #[test]
    fn disable_leaves_a_file_without_crabfarm_alone() {
        let d = tempfile::tempdir().unwrap();
        let original = "[build]\nrustc-wrapper = \"sccache\"\njobs = 3\n";
        let f = cfg_in(d.path(), original);
        let r = disable_file(&f).unwrap();
        assert!(!r.was_enabled);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), original);
        // And a missing file is not an error or created.
        let missing = d.path().join("nope/config.toml");
        assert!(!disable_file(&missing).unwrap().was_enabled);
        assert!(!missing.exists());
    }

    #[test]
    fn a_conflicting_env_entry_is_refused_without_changes() {
        let d = tempfile::tempdir().unwrap();
        let original =
            "[build]\nrustc-wrapper = \"sccache\"\n[env]\nCRABFARM_INNER_WRAPPER = \"other\"\n";
        let f = cfg_in(d.path(), original);
        assert!(enable_file(&f, &exe(), None, 8).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), original);
    }

    #[test]
    fn invalid_toml_is_not_touched() {
        let d = tempfile::tempdir().unwrap();
        let f = cfg_in(d.path(), "[build\n");
        assert!(enable_file(&f, &exe(), None, 8).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "[build\n");
    }

    #[test]
    fn project_lookup_goes_up_and_refuses_outside_a_project() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("p");
        let deep = proj.join("src/a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(proj.join("Cargo.toml"), "[package]\n").unwrap();
        let f = target_config(false, &deep, None).unwrap();
        assert_eq!(f, proj.join(".cargo").join("config.toml"));
        // A directory with no Cargo.toml above it (the tempdir is below any
        // real project): refused with the hint.
        let lone = d.path().join("lone");
        std::fs::create_dir_all(&lone).unwrap();
        let e = target_config(false, &lone, None).unwrap_err().to_string();
        assert!(e.contains("--global"), "{e}");
    }

    #[test]
    fn global_targets_cargo_home_and_prefers_the_legacy_file_only_if_alone() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        assert_eq!(
            target_config(true, Path::new("/nowhere"), Some(home)).unwrap(),
            home.join("config.toml")
        );
        std::fs::write(home.join("config"), "").unwrap();
        assert_eq!(config_file_in(home), home.join("config"));
        std::fs::write(home.join("config.toml"), "").unwrap();
        assert_eq!(config_file_in(home), home.join("config.toml"));
    }

    #[test]
    fn default_jobs_uses_the_farm_or_twice_the_local_slots() {
        assert_eq!(default_jobs(16, None), 32);
        assert_eq!(default_jobs(100, None), MAX_JOBS);
        assert_eq!(default_jobs(16, Some(&[])), 16);
    }
}
