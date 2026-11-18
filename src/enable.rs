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

/// Longest time `cargo locate-project` may take before the fallback is used.
const LOCATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Directory of the workspace that contains `start`, as cargo itself sees it
/// (`cargo locate-project --workspace`). `None` when cargo is missing, fails
/// or does not answer within the timeout.
fn locate_with_cargo(start: &Path) -> Option<PathBuf> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut child = Command::new(cargo)
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .current_dir(start)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + LOCATE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => {
                // Our own child: kill and reap it, nothing else.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let manifest = PathBuf::from(out.trim());
    let dir = manifest.parent()?;
    // Only a manifest at or above `start` is a root for it.
    let dir = dir.to_path_buf();
    (manifest.is_file() && start.ancestors().any(|a| same_dir(a, &dir))).then_some(dir)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

fn manifest_has_workspace(manifest: &Path) -> bool {
    std::fs::read_to_string(manifest)
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .is_some_and(|t| t.contains_key("workspace"))
}

/// Without cargo: the topmost directory at or above the nearest `Cargo.toml`
/// whose manifest has a `[workspace]`, else that nearest directory.
pub fn find_project_root_fallback(start: &Path) -> Option<PathBuf> {
    let nearest = start.ancestors().find(|d| d.join("Cargo.toml").is_file())?;
    let top_workspace = nearest
        .ancestors()
        .filter(|d| manifest_has_workspace(&d.join("Cargo.toml")))
        .last();
    Some(top_workspace.unwrap_or(nearest).to_path_buf())
}

/// The directory `enable` writes `.cargo/config.toml` in: the WORKSPACE root,
/// because cargo reads config files from the directory it runs in upwards and
/// a member's own `.cargo` is ignored when building from the workspace root.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    locate_with_cargo(start).or_else(|| find_project_root_fallback(start))
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

/// The file to write: a symlinked `config.toml` is followed, so the link
/// stays a link and the real file changes.
fn resolve_link(path: &Path) -> anyhow::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path)
            .with_context(|| format!("{} is a symlink that cannot be followed", path.display())),
        _ => Ok(path.to_path_buf()),
    }
}

fn write_if_changed(path: &Path, doc: &DocumentMut) -> anyhow::Result<()> {
    use std::io::Write;
    let target = resolve_link(path)?;
    let existing = std::fs::read_to_string(&target).ok();
    let mut new = doc.to_string();
    // Lines crabfarm adds end in `\n`; a file that uses CRLF keeps CRLF.
    if existing.as_deref().is_some_and(|t| t.contains("\r\n")) {
        new = new.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    if existing.as_deref() == Some(new.as_str()) {
        return Ok(());
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // Replace in one step so an editor or cargo never sees half a file. The
    // temp name is unique, so two runs (or a leftover) never collide.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = target.with_file_name(format!(
        ".{name}.crabfarm-{}-{nanos}.tmp",
        std::process::id()
    ));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    let written = f.write_all(new.as_bytes()).and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("write {}", tmp.display()));
    }
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::Error::new(e).context(format!("replace {}", target.display()))
    })
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

/// Comment on the `[env]` line when crabfarm took the wrapper from OUTSIDE the
/// file (a parent directory or `CARGO_HOME`): `disable` then removes the entry
/// instead of putting a wrapper into the file that was never there.
const INHERITED_MARKER: &str = " # crabfarm enable (inherited)";

/// The inner wrapper as stored in `[env]` (plain string or `{ value = .. }`),
/// and whether crabfarm took it from outside the file.
fn env_inner(env: &dyn toml_edit::TableLike) -> Option<(String, bool)> {
    let item = env.get(INNER_WRAPPER_ENV)?;
    let inherited = item
        .as_value()
        .and_then(|v| v.decor().suffix())
        .and_then(|s| s.as_str())
        .is_some_and(|s| s.contains("crabfarm enable (inherited)"));
    if let Some(s) = item.as_str() {
        return Some((s.to_string(), inherited));
    }
    item.as_table_like()?
        .get("value")?
        .as_str()
        .map(|s| (s.to_string(), inherited))
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

/// What crabfarm remembers on the `jobs` line: the user's earlier value and
/// the user's own trailing comment (kept, so a comment never gets lost).
#[derive(Debug, Default, PartialEq, Eq, Clone)]
struct JobsMark {
    was: Option<i64>,
    comment: Option<String>,
}

fn jobs_marker(item: &Item) -> Option<JobsMark> {
    let suffix = item.as_value()?.decor().suffix()?.as_str()?;
    let rest = suffix.trim().strip_prefix("# crabfarm enable")?;
    let (head, comment) = match rest.split_once(" | ") {
        Some((h, c)) => (h, Some(c.trim().to_string()).filter(|c| !c.is_empty())),
        None => (rest, None),
    };
    let head = head.trim();
    if head.is_empty() {
        return Some(JobsMark { was: None, comment });
    }
    let was = head.strip_prefix("(was ")?.strip_suffix(')')?;
    Some(JobsMark {
        was: was.trim().parse().ok(),
        comment,
    })
}

/// The user's own trailing comment of a value that crabfarm did not write.
fn user_comment(item: &Item) -> Option<String> {
    let suffix = item.as_value()?.decor().suffix()?.as_str()?.trim();
    (suffix.starts_with('#') && !suffix.starts_with("# crabfarm enable"))
        .then(|| suffix.to_string())
}

fn mark_suffix(mark: &JobsMark) -> String {
    let mut s = String::from(JOBS_MARKER);
    if let Some(w) = mark.was {
        s.push_str(&format!(" (was {w})"));
    }
    if let Some(c) = &mark.comment {
        s.push_str(&format!(" | {c}"));
    }
    s
}

/// `RUSTC_WRAPPER` / `CARGO_BUILD_RUSTC_WRAPPER` when set in the environment:
/// cargo lets them override `build.rustc-wrapper` from any config file, so
/// `enable` cannot take effect while one is set (a file cannot fix that).
/// Returns the variable's name (never its value).
pub fn env_wrapper_override(
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<&'static str> {
    ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"]
        .into_iter()
        .find(|n| env(n).is_some())
}

/// Write the wrapper (and `jobs`) into `file`. `jobs`: `Some(n)` was asked
/// for explicitly and replaces the file's value (remembering it); `None`
/// keeps a value that is already there, else writes `default_jobs`.
///
/// `outer_wrapper` is the wrapper cargo would use for this scope WITHOUT the
/// file (from a parent directory or `CARGO_HOME`, found with cargo's own
/// precedence, never crabfarm). It is chained as the inner wrapper, because a
/// `build.rustc-wrapper` in `file` would otherwise replace it silently.
pub fn enable_file(
    file: &Path,
    exe: &Path,
    explicit_jobs: Option<u32>,
    default_jobs: u32,
    outer_wrapper: Option<&str>,
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

    let outer = outer_wrapper.filter(|w| !w.is_empty() && !is_crabfarm(w, Some(exe)));
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
            // An empty value switches a wrapper from further out off. It is
            // remembered (and restored) but is never a program to chain to.
            env.insert(INNER_WRAPPER_ENV, env_item_for(&other));
            if !other.is_empty() {
                inner = Some(other);
            }
        }
        None => {
            if let Some(o) = outer {
                let env = table_mut(&mut doc, "env", file)?;
                // A value the user put there already is theirs.
                if !env.contains_key(INNER_WRAPPER_ENV) {
                    let mut v = Value::from(o);
                    v.decor_mut().set_suffix(INHERITED_MARKER);
                    env.insert(INNER_WRAPPER_ENV, Item::Value(v));
                    inner = Some(o.to_string());
                }
            }
        }
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
                // Remember what the user had (value and comment); keep an
                // older memory on re-runs.
                let mark = match (&ours, &current) {
                    (Some(prev), _) => prev.clone(),
                    (None, c) => JobsMark {
                        was: cur,
                        comment: c.as_ref().and_then(user_comment),
                    },
                };
                let mut v = Value::from(i64::from(n));
                v.decor_mut().set_suffix(mark_suffix(&mark));
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
    disable_file_if(file, None)
}

/// True when the configured wrapper is exactly `exe` (the same file, or the
/// same path ignoring case and slash direction on Windows).
fn wrapper_is_exe(wrapper: &str, exe: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(wrapper), std::fs::canonicalize(exe)) {
        return a == b;
    }
    let norm = |p: &str| {
        let p = p.replace('\\', "/");
        if cfg!(windows) {
            p.to_ascii_lowercase()
        } else {
            p
        }
    };
    norm(wrapper) == norm(&exe.to_string_lossy())
}

/// Like [`disable_file`]; with `only_exe` the file is changed only when its
/// wrapper IS that executable (the uninstaller must not touch a global entry
/// that belongs to another crabfarm install).
pub fn disable_file_if(file: &Path, only_exe: Option<&Path>) -> anyhow::Result<DisableReport> {
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
        .is_some_and(|w| match only_exe {
            Some(exe) => wrapper_is_exe(w, exe),
            None => is_crabfarm(w, None),
        });
    if !wrapper_is_ours {
        return Ok(report);
    }
    report.was_enabled = true;

    // The wrapper that was in place before, if enable moved one into [env]
    // (or recorded one it inherited from outside the file).
    let stored = doc
        .get("env")
        .and_then(Item::as_table_like)
        .and_then(env_inner);
    let restore = stored.as_ref().filter(|(_, inh)| !inh).map(|(w, _)| w);
    {
        let build = table_mut(&mut doc, "build", file)?;
        match restore {
            Some(w) => set_keep_decor(build, "rustc-wrapper", Value::from(w.clone())),
            None => {
                build.remove("rustc-wrapper");
            }
        }
        match build.get("jobs").and_then(jobs_marker) {
            Some(JobsMark {
                was: Some(was),
                comment,
            }) => {
                let mut v = Value::from(was);
                // Back to the user's line, with their own comment only.
                v.decor_mut().clear();
                v.decor_mut().set_prefix(" ");
                if let Some(c) = comment {
                    v.decor_mut().set_suffix(format!(" {c}"));
                }
                build.insert("jobs", Item::Value(v));
            }
            Some(JobsMark { was: None, .. }) => {
                build.remove("jobs");
            }
            None => {}
        }
        if build.is_empty() {
            doc.remove("build");
        }
    }
    if stored.is_some()
        && let Some(env) = doc.get_mut("env").and_then(Item::as_table_like_mut)
    {
        env.remove(INNER_WRAPPER_ENV);
        if env.is_empty() {
            doc.remove("env");
        }
    }
    report.restored = restore.filter(|w| !w.is_empty()).cloned();
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
        let r = enable_file(&f, &exe(), None, 24, None).unwrap();
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
        enable_file(&f, &exe(), Some(16), 0, None).unwrap();
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
        let r = enable_file(&f, &exe(), Some(32), 0, None).unwrap();
        assert_eq!(r.inner.as_deref(), Some("sccache"));
        let t = parsed(&f);
        assert_eq!(t["build"]["rustc-wrapper"].as_str(), Some(EXE));
        assert_eq!(t["env"][INNER_WRAPPER_ENV].as_str(), Some("sccache"));
        assert_eq!(t["build"]["jobs"].as_integer(), Some(32));
        // Enabling again changes nothing and never chains crabfarm to itself.
        let again = std::fs::read_to_string(&f).unwrap();
        enable_file(&f, &exe(), Some(32), 0, None).unwrap();
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
        enable_file(&f, &exe(), Some(4), 0, None).unwrap();
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
        let r = enable_file(&f, &exe(), None, 40, None).unwrap();
        assert_eq!(r.jobs, Some((8, false)));
        disable_file(&f).unwrap();
        assert_eq!(parsed(&f)["build"]["jobs"].as_integer(), Some(8));
        // --jobs: replaced, remembered, restored.
        enable_file(&f, &exe(), Some(64), 40, None).unwrap();
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
        assert!(enable_file(&f, &exe(), None, 8, None).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), original);
    }

    #[test]
    fn invalid_toml_is_not_touched() {
        let d = tempfile::tempdir().unwrap();
        let f = cfg_in(d.path(), "[build\n");
        assert!(enable_file(&f, &exe(), None, 8, None).is_err());
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

    /// A workspace with a member below it, valid enough for cargo itself.
    fn make_workspace(root: &Path) -> PathBuf {
        let member = root.join("crates/foo");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/foo\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"foo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(member.join("src/lib.rs"), "").unwrap();
        member
    }

    /// CF-27 F1: from a member the config goes to the workspace root, which is
    /// where cargo looks when it builds from there.
    #[test]
    fn enable_in_a_workspace_member_targets_the_workspace_root() {
        let d = tempfile::tempdir().unwrap();
        let member = make_workspace(d.path());
        let f = target_config(false, &member, None).unwrap();
        // cargo reports the real path (macOS temp dirs sit behind the `/var`
        // symlink), so compare the resolved directories.
        let real = |p: &Path| p.canonicalize().unwrap();
        assert_eq!(f.file_name().unwrap(), "config.toml");
        assert_eq!(f.parent().unwrap().file_name().unwrap(), ".cargo");
        assert_eq!(real(f.parent().unwrap().parent().unwrap()), real(d.path()));
        // The no-cargo fallback agrees.
        assert_eq!(
            find_project_root_fallback(&member).unwrap(),
            d.path().to_path_buf()
        );
    }

    #[test]
    fn the_fallback_takes_the_topmost_workspace_manifest() {
        let d = tempfile::tempdir().unwrap();
        let member = make_workspace(d.path());
        // A plain package without a workspace stays its own root.
        let lone = d.path().join("lone");
        std::fs::create_dir_all(&lone).unwrap();
        std::fs::write(lone.join("Cargo.toml"), "[package]\nname = \"l\"\n").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("Cargo.toml"),
            "[package]\nname = \"o\"\n",
        )
        .unwrap();
        assert_eq!(
            find_project_root_fallback(outside.path()).unwrap(),
            outside.path().to_path_buf()
        );
        assert_eq!(
            find_project_root_fallback(&member.join("src")).unwrap(),
            d.path()
        );
    }

    /// CF-27 F2: a wrapper from OUTSIDE the target file is chained, not lost.
    #[test]
    fn an_outer_wrapper_is_chained_and_disable_does_not_leave_it_behind() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".cargo/config.toml");
        let r = enable_file(&f, &exe(), Some(8), 0, Some("sccache")).unwrap();
        assert_eq!(r.inner.as_deref(), Some("sccache"));
        let t = parsed(&f);
        assert_eq!(t["build"]["rustc-wrapper"].as_str(), Some(EXE));
        assert_eq!(t["env"][INNER_WRAPPER_ENV].as_str(), Some("sccache"));
        // Same result a second time, nothing doubled.
        let first = std::fs::read_to_string(&f).unwrap();
        enable_file(&f, &exe(), Some(8), 0, Some("sccache")).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), first);
        // The wrapper was never in this file, so disable must not write it.
        let r = disable_file(&f).unwrap();
        assert!(r.was_enabled);
        assert_eq!(r.restored, None);
        assert_eq!(std::fs::read_to_string(&f).unwrap().trim(), "");
    }

    #[test]
    fn crabfarm_or_an_empty_outer_wrapper_is_never_chained() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".cargo/config.toml");
        let me = exe().to_string_lossy().into_owned();
        for outer in [Some(me.as_str()), Some(""), None] {
            let r = enable_file(&f, &exe(), Some(2), 0, outer).unwrap();
            assert_eq!(r.inner, None);
            assert!(!parsed(&f).contains_key("env"));
            disable_file(&f).unwrap();
        }
    }

    #[test]
    fn a_wrapper_in_the_environment_is_reported_by_name_only() {
        let none = |_: &str| None::<std::ffi::OsString>;
        assert_eq!(env_wrapper_override(&none), None);
        let a = |n: &str| (n == "RUSTC_WRAPPER").then(|| "sccache".into());
        assert_eq!(env_wrapper_override(&a), Some("RUSTC_WRAPPER"));
        let b = |n: &str| (n == "CARGO_BUILD_RUSTC_WRAPPER").then(std::ffi::OsString::new);
        assert_eq!(env_wrapper_override(&b), Some("CARGO_BUILD_RUSTC_WRAPPER"));
    }

    #[test]
    fn an_empty_original_wrapper_is_restored_and_never_used_as_inner() {
        let d = tempfile::tempdir().unwrap();
        let original = "[build]\nrustc-wrapper = \"\"\n";
        let f = cfg_in(d.path(), original);
        let r = enable_file(&f, &exe(), Some(4), 0, Some("sccache")).unwrap();
        assert_eq!(r.inner, None);
        let r = disable_file(&f).unwrap();
        assert_eq!(r.restored, None);
        assert!(r.was_enabled);
        assert_eq!(parsed(&f)["build"]["rustc-wrapper"].as_str(), Some(""));
        assert!(!parsed(&f).contains_key("env"));
    }

    #[test]
    fn this_install_only_leaves_another_installs_entry_alone() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".cargo/config.toml");
        let other = if cfg!(windows) {
            PathBuf::from(r"C:\other\crabfarm.exe")
        } else {
            PathBuf::from("/other/crabfarm")
        };
        enable_file(&f, &other, Some(4), 0, None).unwrap();
        let before = std::fs::read_to_string(&f).unwrap();
        let r = disable_file_if(&f, Some(&exe())).unwrap();
        assert!(!r.was_enabled);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), before);
        // And its own entry (even with another slash style) is removed.
        let r = disable_file_if(&f, Some(&other)).unwrap();
        assert!(r.was_enabled);
        assert_eq!(std::fs::read_to_string(&f).unwrap().trim(), "");
    }

    #[test]
    fn crlf_files_stay_crlf_and_come_back_byte_for_byte() {
        let d = tempfile::tempdir().unwrap();
        let original = "# c\r\n[build]\r\ntarget-dir = \"out\"\r\n";
        let f = cfg_in(d.path(), original);
        enable_file(&f, &exe(), Some(4), 0, None).unwrap();
        let during = std::fs::read_to_string(&f).unwrap();
        assert!(!during.replace("\r\n", "").contains('\n'), "{during:?}");
        disable_file(&f).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), original);
    }

    #[test]
    fn the_comment_on_the_jobs_line_survives_enable_and_disable() {
        let d = tempfile::tempdir().unwrap();
        let original = "[build]\njobs = 8 # my choice\n";
        let f = cfg_in(d.path(), original);
        enable_file(&f, &exe(), Some(32), 0, None).unwrap();
        assert!(std::fs::read_to_string(&f).unwrap().contains("# my choice"));
        enable_file(&f, &exe(), Some(48), 0, None).unwrap();
        disable_file(&f).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), original);
    }

    #[test]
    fn no_temp_file_is_left_next_to_the_config() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".cargo/config.toml");
        enable_file(&f, &exe(), Some(4), 0, None).unwrap();
        disable_file(&f).unwrap();
        let names: Vec<_> = std::fs::read_dir(f.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["config.toml"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_written_through_and_stays_a_link() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("shared.toml");
        std::fs::write(&real, "[net]\nretry = 2\n").unwrap();
        let f = d.path().join(".cargo/config.toml");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &f).unwrap();
        enable_file(&f, &exe(), Some(4), 0, None).unwrap();
        assert!(
            std::fs::symlink_metadata(&f)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(std::fs::read_to_string(&real).unwrap().contains(EXE));
    }
}
