//! `crabfarm update` (CF-37): install the newest published GitHub release over
//! the install this executable belongs to.
//!
//! - Only `https://` is used, for every hop (the API, the download and its
//!   redirects); certificates are checked with rustls against the Mozilla
//!   roots. A download URL must be exactly the release download URL of this
//!   repository for the asset's name; anything else is refused.
//! - Every download is checked against the `SHA256SUMS.txt` of the same
//!   release before it is used; a file not listed there is refused.
//! - Windows installs made by the setup run the new setup silently, with the
//!   PATH and autostart choices kept as they are now. macOS and Linux installs
//!   made by `install.sh` run the release's `install.sh` on the downloaded
//!   archive, into the same folder. Any other install (cargo install, an
//!   unpacked zip, a build tree) is told what to do and nothing changes.

use anyhow::{Context, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

pub const REPO: &str = "rennerdo30/crabfarm";
const API_RELEASES: &str = "https://api.github.com/repos/rennerdo30/crabfarm/releases?per_page=30";
const DOWNLOAD_PREFIX: &str = "https://github.com/rennerdo30/crabfarm/releases/download/";
const RELEASES_PAGE: &str = "https://github.com/rennerdo30/crabfarm/releases";
const MAX_JSON: u64 = 8 << 20;
const MAX_SUMS: u64 = 1 << 20;
const MAX_ASSET: u64 = 256 << 20;
/// The whole install step (the setup stops the node first: up to 40 s).
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

// ------------------------------------------------------------------ versions

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Text(String),
}

impl Ord for Ident {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Ident::Num(a), Ident::Num(b)) => a.cmp(b),
            (Ident::Num(_), Ident::Text(_)) => Ordering::Less,
            (Ident::Text(_), Ident::Num(_)) => Ordering::Greater,
            (Ident::Text(a), Ident::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A semantic version, ordered by the SemVer 2.0 precedence rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<Ident>,
    text: String,
}

impl Version {
    /// `1.2.3` or `1.2.3-rc.1` (a leading `v` is accepted, build metadata
    /// after `+` is ignored). Anything else is `None`.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let nums: Vec<&str> = core.split('.').collect();
        if nums.len() != 3 {
            return None;
        }
        let mut c = [0u64; 3];
        for (i, n) in nums.iter().enumerate() {
            if n.is_empty()
                || !n.bytes().all(|b| b.is_ascii_digit())
                || (n.len() > 1 && n.starts_with('0'))
            {
                return None;
            }
            c[i] = n.parse().ok()?;
        }
        let mut idents = Vec::new();
        if let Some(p) = pre {
            for part in p.split('.') {
                if part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return None;
                }
                let numeric = part.bytes().all(|b| b.is_ascii_digit());
                if numeric && part.len() > 1 && part.starts_with('0') {
                    return None;
                }
                idents.push(if numeric {
                    Ident::Num(part.parse().ok()?)
                } else {
                    Ident::Text(part.to_string())
                });
            }
        }
        Some(Version {
            core: c,
            pre: idents,
            text: s.to_string(),
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the package version is a semantic version")
}

// ------------------------------------------------------------------ releases

#[derive(Clone, Debug, Deserialize)]
pub struct ApiAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApiRelease {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<ApiAsset>,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub assets: Vec<ApiAsset>,
}

/// The newest published release: never a draft, a prerelease only with `pre`,
/// and only tags of the form `v<semver>`.
pub fn pick_release(list: &[ApiRelease], pre: bool) -> Option<Release> {
    list.iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let v = Version::parse(r.tag_name.strip_prefix('v')?)?;
            // A tag with `-` is published as a prerelease (release.yml); trust
            // neither flag alone.
            if (r.prerelease || v.is_prerelease()) && !pre {
                return None;
            }
            Some(Release {
                version: v,
                tag: r.tag_name.clone(),
                assets: r.assets.clone(),
            })
        })
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// Asset names crabfarm releases use: letters, digits, `.`, `-`, `_`.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// The asset `name` of `r`, if its download URL is exactly this repository's
/// release download URL for it (fail closed: no other host, path or query).
pub fn asset<'a>(r: &'a Release, name: &str) -> anyhow::Result<&'a ApiAsset> {
    let a = r
        .assets
        .iter()
        .find(|a| a.name == name)
        .with_context(|| format!("release {} has no file {name}", r.tag))?;
    let want = format!("{DOWNLOAD_PREFIX}{}/{name}", r.tag);
    if !plain_name(name) || !plain_name(&r.tag) || a.browser_download_url != want {
        bail!(
            "release {} lists {name} at an unexpected address ({}); refusing to download it",
            r.tag,
            a.browser_download_url
        );
    }
    if a.size > MAX_ASSET {
        bail!("{name} is {} bytes, more than crabfarm downloads", a.size);
    }
    Ok(a)
}

/// `sha256sum` output: `<64 hex>  <name>` (or ` *<name>`) per line.
pub fn parse_sums(text: &str) -> HashMap<String, [u8; 32]> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some((hex, rest)) = line.split_once(' ') else {
            continue;
        };
        let name = rest
            .trim_start_matches(' ')
            .trim_start_matches('*')
            .trim_end();
        let Ok(bytes) = data_encoding::HEXLOWER_PERMISSIVE.decode(hex.as_bytes()) else {
            continue;
        };
        let Ok(hash) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            continue;
        };
        if plain_name(name) {
            out.insert(name.to_string(), hash);
        }
    }
    out
}

/// Fail unless `data` is exactly what SHA256SUMS lists for `name`.
pub fn verify(sums: &HashMap<String, [u8; 32]>, name: &str, data: &[u8]) -> anyhow::Result<()> {
    let want = sums
        .get(name)
        .with_context(|| format!("{name} is not listed in SHA256SUMS.txt; refusing to use it"))?;
    let got: [u8; 32] = Sha256::digest(data).into();
    if &got != want {
        bail!("checksum mismatch for {name}; the download is not used");
    }
    Ok(())
}

/// The release archive name for this build (`install.sh` platforms).
pub fn host_target() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else {
        None
    }
}

// ------------------------------------------------------------------ installs

/// How this executable was installed, which decides how it is updated.
#[derive(Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// The Windows setup (its uninstall entry names this folder).
    WindowsSetup(PathBuf),
    /// `install.sh`: its folder (`~/.local/bin`, or `CRABFARM_BIN_DIR` when
    /// that is set now), with crabfarm, crabfarmd and cargo-crabfarm in it.
    Script(PathBuf),
    /// `cargo install` (`$CARGO_HOME/bin`).
    Cargo,
    /// Anything else: an unpacked zip, a build tree.
    Other(PathBuf),
}

fn same_dir(a: &Path, b: &Path, windows: bool) -> bool {
    let n = |p: &Path| {
        let s = p.to_string_lossy();
        let s = s.trim_end_matches(['\\', '/']);
        if windows {
            s.replace('/', "\\").to_lowercase()
        } else {
            s.to_string()
        }
    };
    n(a) == n(b)
}

pub fn install_kind(
    exe_dir: &Path,
    windows: bool,
    setup_location: Option<&Path>,
    cargo_home: Option<&Path>,
    script_dirs: &[PathBuf],
    has_siblings: bool,
) -> InstallKind {
    if windows && setup_location.is_some_and(|l| same_dir(l, exe_dir, true)) {
        return InstallKind::WindowsSetup(exe_dir.to_path_buf());
    }
    if cargo_home.is_some_and(|h| same_dir(&h.join("bin"), exe_dir, windows)) {
        return InstallKind::Cargo;
    }
    // Only the folders install.sh installs into: a build tree has the same
    // three programs side by side and must never be overwritten.
    if !windows && has_siblings && script_dirs.iter().any(|d| same_dir(d, exe_dir, false)) {
        return InstallKind::Script(exe_dir.to_path_buf());
    }
    InstallKind::Other(exe_dir.to_path_buf())
}

#[cfg(windows)]
pub fn setup_location() -> Option<PathBuf> {
    let v = crate::winreg::read_str(
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\crabfarm",
        "InstallLocation",
    )
    .ok()??;
    Some(PathBuf::from(v.value))
}

#[cfg(not(windows))]
pub fn setup_location() -> Option<PathBuf> {
    None
}

pub fn this_install() -> anyhow::Result<InstallKind> {
    let exe = std::env::current_exe().context("cannot find my own executable")?;
    let dir = exe.parent().context("crabfarm has no parent directory")?;
    let suffix = std::env::consts::EXE_SUFFIX;
    let siblings = ["crabfarmd", "cargo-crabfarm"]
        .iter()
        .all(|n| dir.join(format!("{n}{suffix}")).is_file());
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".cargo")));
    let mut script_dirs: Vec<PathBuf> = home.iter().map(|h| h.join(".local/bin")).collect();
    if let Some(d) = std::env::var_os("CRABFARM_BIN_DIR").filter(|d| !d.is_empty()) {
        script_dirs.push(PathBuf::from(d));
    }
    Ok(install_kind(
        dir,
        cfg!(windows),
        setup_location().as_deref(),
        cargo_home.as_deref(),
        &script_dirs,
        siblings,
    ))
}

// ------------------------------------------------------------------ network

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_global(Some(Duration::from_secs(600)))
        .user_agent(format!("crabfarm/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn get(agent: &ureq::Agent, url: &str, accept: &str, limit: u64) -> anyhow::Result<Vec<u8>> {
    let mut resp = agent
        .get(url)
        .header("Accept", accept)
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(403 | 429) => anyhow::anyhow!(
                "GitHub refused the request ({e}); its limit for anonymous requests is 60 per hour, try again later"
            ),
            e => anyhow::anyhow!("{url}: {e}"),
        })?;
    resp.body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .with_context(|| format!("read {url}"))
}

pub fn fetch_releases(agent: &ureq::Agent) -> anyhow::Result<Vec<ApiRelease>> {
    let body = get(agent, API_RELEASES, "application/vnd.github+json", MAX_JSON)?;
    serde_json::from_slice(&body).context("GitHub sent a release list crabfarm cannot read")
}

fn download(
    agent: &ureq::Agent,
    r: &Release,
    sums: &HashMap<String, [u8; 32]>,
    name: &str,
    dir: &Path,
) -> anyhow::Result<PathBuf> {
    let a = asset(r, name)?;
    println!("Downloading {name} ...");
    let data = get(
        agent,
        &a.browser_download_url,
        "application/octet-stream",
        MAX_ASSET,
    )?;
    verify(sums, name, &data)?;
    let path = dir.join(name);
    std::fs::write(&path, &data).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Run `cmd` to its end, or kill it (this child only) after `timeout`.
fn run_bounded(mut cmd: Command, timeout: Duration) -> anyhow::Result<ExitStatus> {
    let mut child = cmd.spawn().context("start the installer")?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(st);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the installer did not finish within {} s and was stopped",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ------------------------------------------------------------------ command

pub struct Options {
    pub check: bool,
    pub pre: bool,
    pub force: bool,
}

pub fn run(opts: Options) -> anyhow::Result<i32> {
    let current = current_version();
    let agent = agent();
    let list = fetch_releases(&agent)?;
    let Some(rel) = pick_release(&list, opts.pre) else {
        if !opts.pre && pick_release(&list, true).is_some() {
            println!(
                "No stable crabfarm release yet; `crabfarm update --pre` takes prereleases too."
            );
        } else {
            println!("No published crabfarm release on GitHub yet ({RELEASES_PAGE}).");
        }
        return Ok(0);
    };
    let newer = rel.version > current;
    if !newer && !opts.force {
        println!(
            "crabfarm {current} is up to date (newest {}release: {}).",
            if opts.pre { "" } else { "stable " },
            rel.version
        );
        return Ok(0);
    }
    if opts.check {
        if newer {
            println!(
                "Update available: {current} -> {}. Run `crabfarm update`{}.",
                rel.version,
                if opts.pre { " --pre" } else { "" }
            );
        }
        return Ok(0);
    }
    let kind = this_install()?;
    let dir = match &kind {
        InstallKind::WindowsSetup(d) | InstallKind::Script(d) => d.clone(),
        InstallKind::Cargo => {
            println!(
                "This crabfarm was installed with cargo. Update it with:\n  cargo install crabfarm --locked --version {}",
                rel.version
            );
            return Ok(1);
        }
        InstallKind::Other(d) => {
            println!(
                "{} is not a folder the crabfarm setup or install.sh installed, so `crabfarm update` changes nothing here.\n\
                 Download crabfarm {} from {RELEASES_PAGE}/tag/{} instead.",
                d.display(),
                rel.version,
                rel.tag
            );
            return Ok(1);
        }
    };
    println!(
        "Updating crabfarm {current} -> {} in {}",
        rel.version,
        dir.display()
    );

    let data_dir = crate::config::data_dir()?;
    let work = data_dir
        .join("updates")
        .join(format!("{}-{:08x}", rel.tag, rand::random::<u32>()));
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let result = install(&agent, &rel, &kind, &dir, &data_dir, &work);
    let _ = std::fs::remove_dir_all(&work);
    result?;
    println!(
        "crabfarm {} is installed. Update every machine of the farm: nodes that speak another protocol version cannot work together.",
        rel.version
    );
    Ok(0)
}

fn install(
    agent: &ureq::Agent,
    rel: &Release,
    kind: &InstallKind,
    dir: &Path,
    data_dir: &Path,
    work: &Path,
) -> anyhow::Result<()> {
    let sums_raw = get(
        agent,
        &asset(rel, "SHA256SUMS.txt")?.browser_download_url,
        "application/octet-stream",
        MAX_SUMS,
    )?;
    let sums_text = String::from_utf8(sums_raw).context("SHA256SUMS.txt is not text")?;
    let sums = parse_sums(&sums_text);
    let was_running = crate::daemon::node_running(data_dir);
    let daemon = dir.join(format!("crabfarmd{}", std::env::consts::EXE_SUFFIX));
    match kind {
        InstallKind::WindowsSetup(_) => {
            let setup = download(
                agent,
                rel,
                &sums,
                &format!("crabfarm-{}-setup.exe", rel.version),
                work,
            )?;
            let mut cmd = Command::new(&setup);
            cmd.arg("/S");
            for flag in setup_flags(dir) {
                cmd.arg(flag);
            }
            println!(
                "Running the setup (no windows; it stops the node while it replaces the files) ..."
            );
            let st = run_bounded(cmd, INSTALL_TIMEOUT)?;
            if !st.success() {
                bail!(
                    "the setup failed ({st}). Usually a crabfarm program was still in use: close cargo and rust-analyzer, then run `crabfarm update` again. \
                     The setup's details are in its log only when run by hand: {}",
                    setup.display()
                );
            }
        }
        InstallKind::Script(_) => {
            let target = host_target().context("no release archive is built for this platform")?;
            let script = download(agent, rel, &sums, "install.sh", work)?;
            let archive = download(
                agent,
                rel,
                &sums,
                &format!("crabfarm-{}-{target}.tar.gz", rel.version),
                work,
            )?;
            // install.sh checks the archive again against the sums next to it.
            std::fs::write(work.join("SHA256SUMS.txt"), &sums_text)?;
            if was_running {
                crate::daemon::stop_node(data_dir, crate::daemon::STOP_TIMEOUT)?;
            }
            let mut cmd = Command::new("sh");
            cmd.arg(&script).arg(&archive).env("CRABFARM_BIN_DIR", dir);
            for v in [
                "CRABFARM_SKIP_VERIFY",
                "CRABFARM_BASE_URL",
                "CRABFARM_TARGET",
                "CRABFARM_VERSION",
            ] {
                cmd.env_remove(v);
            }
            let st = run_bounded(cmd, INSTALL_TIMEOUT)?;
            if !st.success() {
                bail!("install.sh failed ({st})");
            }
        }
        InstallKind::Cargo | InstallKind::Other(_) => unreachable!("handled by run"),
    }
    // Autostart (when on) starts the new node itself; otherwise start it when
    // it ran before the update.
    if was_running {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::daemon::node_running(data_dir) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
        }
        if !crate::daemon::node_running(data_dir) {
            use crate::service::Host;
            crate::service::RealHost::new(data_dir.to_path_buf()).start_detached(&daemon)?;
            println!("Started the node again.");
        }
    }
    Ok(())
}

/// Keep what the user chose at the first install: a PATH entry and autostart
/// are only (re)made when they exist now.
#[cfg(windows)]
fn setup_flags(dir: &Path) -> Vec<&'static str> {
    let mut flags = Vec::new();
    let on_path = crate::pathenv::contains(&crate::pathenv::RegistryEnv, &dir.to_string_lossy())
        .unwrap_or(true);
    if !on_path {
        flags.push("/NOPATH");
    }
    let daemon = dir.join("crabfarmd.exe");
    let autostart =
        crate::winreg::read_str(crate::service::RUN_SUBKEY, crate::service::RUN_VALUE_NAME)
            .ok()
            .flatten()
            .is_some_and(|v| v.value == crate::service::run_value(&daemon));
    if !autostart {
        flags.push("/NOAUTOSTART");
    }
    flags
}

#[cfg(not(windows))]
fn setup_flags(_dir: &Path) -> Vec<&'static str> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn versions_follow_semver_precedence() {
        let order = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0-rc.10",
            "1.0.0",
            "1.0.1",
            "1.2.0",
            "10.0.0",
        ];
        for w in order.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("1.2.3+build.5").cmp(&v("1.2.3")), Ordering::Equal);
        for bad in [
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.x",
            "1.2.3-",
            "1.2.3-a..b",
            "",
            "1.2.3-ä",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    fn rel(tag: &str, draft: bool, pre: bool) -> ApiRelease {
        ApiRelease {
            tag_name: tag.into(),
            draft,
            prerelease: pre,
            assets: vec![],
        }
    }

    #[test]
    fn the_newest_published_release_wins_and_prereleases_need_pre() {
        let list = vec![
            rel("v1.0.0", false, false),
            rel("v1.1.0-rc.1", false, true),
            rel("v1.2.0", true, false),
            rel("v1.0.1-rc.2", false, false), // a `-` tag counts as a prerelease
            rel("nightly", false, false),
            rel("v0.9.0", false, false),
        ];
        assert_eq!(pick_release(&list, false).unwrap().tag, "v1.0.0");
        assert_eq!(pick_release(&list, true).unwrap().tag, "v1.1.0-rc.1");
        assert!(pick_release(&[rel("v2.0.0", true, false)], true).is_none());
    }

    #[test]
    fn only_this_repositorys_download_url_is_accepted() {
        let mk = |url: &str| Release {
            version: v("1.0.0"),
            tag: "v1.0.0".into(),
            assets: vec![ApiAsset {
                name: "crabfarm-1.0.0-setup.exe".into(),
                browser_download_url: url.into(),
                size: 10,
            }],
        };
        let good = "https://github.com/rennerdo30/crabfarm/releases/download/v1.0.0/crabfarm-1.0.0-setup.exe";
        assert!(asset(&mk(good), "crabfarm-1.0.0-setup.exe").is_ok());
        for bad in [
            "http://github.com/rennerdo30/crabfarm/releases/download/v1.0.0/crabfarm-1.0.0-setup.exe",
            "https://github.com/someone/crabfarm/releases/download/v1.0.0/crabfarm-1.0.0-setup.exe",
            "https://github.com.evil.example/rennerdo30/crabfarm/releases/download/v1.0.0/crabfarm-1.0.0-setup.exe",
            "https://github.com/rennerdo30/crabfarm/releases/download/v1.0.0/crabfarm-1.0.0-setup.exe?x=1",
            "https://github.com/rennerdo30/crabfarm/releases/download/v0.9.0/crabfarm-1.0.0-setup.exe",
        ] {
            assert!(
                asset(&mk(bad), "crabfarm-1.0.0-setup.exe").is_err(),
                "{bad}"
            );
        }
        assert!(asset(&mk(good), "missing.exe").is_err());
    }

    #[test]
    fn downloads_must_match_their_listed_checksum() {
        let data = b"setup bytes";
        let hex = data_encoding::HEXLOWER.encode(&Sha256::digest(data));
        let sums = parse_sums(&format!(
            "{hex}  crabfarm-1.0.0-setup.exe\n{hex} *install.sh\nnot a line\n{}  ../evil\n",
            "0".repeat(64)
        ));
        assert_eq!(sums.len(), 2, "{sums:?}");
        assert!(verify(&sums, "crabfarm-1.0.0-setup.exe", data).is_ok());
        assert!(verify(&sums, "install.sh", data).is_ok());
        assert!(verify(&sums, "crabfarm-1.0.0-setup.exe", b"other bytes").is_err());
        assert!(verify(&sums, "unlisted.tar.gz", data).is_err());
    }

    #[test]
    fn the_install_kind_follows_where_the_executable_lives() {
        let setup = Path::new(r"C:\Users\u\AppData\Local\Programs\crabfarm");
        let home = Path::new(r"C:\Users\u\.cargo");
        let local = [PathBuf::from("/home/u/.local/bin")];
        assert_eq!(
            install_kind(
                Path::new(r"c:\users\u\appdata\local\programs\crabfarm\"),
                true,
                Some(setup),
                Some(home),
                &[],
                true
            ),
            InstallKind::WindowsSetup(PathBuf::from(
                r"c:\users\u\appdata\local\programs\crabfarm\"
            ))
        );
        assert_eq!(
            install_kind(
                Path::new(r"C:\Users\u\.cargo\bin"),
                true,
                Some(setup),
                Some(home),
                &[],
                true
            ),
            InstallKind::Cargo
        );
        assert!(matches!(
            install_kind(
                Path::new(r"E:\tools\crabfarm"),
                true,
                Some(setup),
                Some(home),
                &[],
                true
            ),
            InstallKind::Other(_)
        ));
        assert!(matches!(
            install_kind(
                Path::new("/home/u/.local/bin"),
                false,
                None,
                Some(Path::new("/home/u/.cargo")),
                &local,
                true
            ),
            InstallKind::Script(_)
        ));
        // A build tree has all three programs too, and is never overwritten.
        assert!(matches!(
            install_kind(
                Path::new("/home/u/src/crabfarm/target/release"),
                false,
                None,
                None,
                &local,
                true
            ),
            InstallKind::Other(_)
        ));
    }
}
