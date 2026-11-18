//! `crabfarm service install|uninstall|status`: explicit opt-in autostart of
//! the node (`crabfarmd`) for the current user, never with admin rights.
//!
//! - Windows: HKCU `...\Run` value `crabfarm`.
//! - macOS: a LaunchAgent plist, loaded with `launchctl bootstrap gui/<uid>`.
//! - Linux: a systemd user unit, `systemctl --user enable --now`.
//!
//! The logic runs against the [`Host`] trait; tests use an in-memory fake for
//! all three systems, [`RealHost`] is the only code that touches the machine.

use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const RUN_VALUE_NAME: &str = "crabfarm";
pub const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const LAUNCHD_LABEL: &str = "dev.renner.crabfarm";
pub const SYSTEMD_UNIT: &str = "crabfarm.service";
/// Every child process (launchctl, systemctl) gets this long.
pub const CHILD_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        }
    }
}

/// Everything the file names and commands depend on.
#[derive(Clone, Debug)]
pub struct Target {
    pub os: Os,
    pub home: PathBuf,
    pub uid: u32,
    /// Absolute path of `crabfarmd`.
    pub daemon: PathBuf,
    pub data_dir: PathBuf,
}

impl Target {
    pub fn plist_path(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"))
    }

    pub fn unit_path(&self) -> PathBuf {
        self.home.join(".config/systemd/user").join(SYSTEMD_UNIT)
    }
}

/// The Run value: the quoted path, so a space in the profile path is safe.
pub fn run_value(daemon: &Path) -> String {
    format!("\"{}\"", daemon.display())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// LaunchAgent: start at login, restart only after a crash (a clean exit,
/// including "another node is already running", stays down).
pub fn plist(daemon: &Path, data_dir: &Path) -> String {
    let log = data_dir.join("logs").join("launchd.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{daemon}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        daemon = xml_escape(&daemon.display().to_string()),
        log = xml_escape(&log.display().to_string()),
    )
}

/// systemd user unit. `%` is special in unit files and doubled.
pub fn systemd_unit(daemon: &Path) -> String {
    let exe = daemon
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!(
        "[Unit]\n\
         Description=crabfarm node (distributed rustc farm)\n\
         \n\
         [Service]\n\
         ExecStart=\"{exe}\"\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

pub struct CmdResult {
    pub ok: bool,
}

/// The machine, as far as the service commands touch it.
pub trait Host {
    fn run_key_get(&self) -> anyhow::Result<Option<String>>;
    fn run_key_set(&mut self, value: &str) -> anyhow::Result<()>;
    fn run_key_delete(&mut self) -> anyhow::Result<bool>;
    fn write_file(&mut self, path: &Path, content: &str) -> anyhow::Result<()>;
    fn remove_file(&mut self, path: &Path) -> anyhow::Result<bool>;
    fn read_file(&self, path: &Path) -> Option<String>;
    /// Run a program with a timeout; stdio is discarded.
    fn run(&mut self, program: &str, args: &[String]) -> anyhow::Result<CmdResult>;
    /// Start `crabfarmd` as the user's own detached process.
    fn start_detached(&mut self, daemon: &Path) -> anyhow::Result<()>;
    fn node_running(&self) -> bool;
}

fn s(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Turn autostart on. Returns lines for the user.
pub fn install(host: &mut dyn Host, t: &Target) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    match t.os {
        Os::Windows => {
            host.run_key_set(&run_value(&t.daemon))?;
            out.push("Autostart on: crabfarmd starts when you sign in.".into());
            // The Run key only acts at the next sign-in.
            if host.node_running() {
                out.push("The node is already running.".into());
            } else {
                host.start_detached(&t.daemon)?;
                out.push("Started the node now.".into());
            }
        }
        Os::MacOs => {
            let plist_path = t.plist_path();
            host.write_file(&plist_path, &plist(&t.daemon, &t.data_dir))?;
            let domain = format!("gui/{}", t.uid);
            // Re-installing: unload an older copy first; "not loaded" is fine.
            let _ = host.run(
                "launchctl",
                &s(&["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]),
            );
            let r = host.run(
                "launchctl",
                &s(&["bootstrap", &domain, &plist_path.display().to_string()]),
            )?;
            if !r.ok {
                bail!(
                    "`launchctl bootstrap {domain}` failed; the plist is at {}",
                    plist_path.display()
                );
            }
            out.push("Autostart on: a LaunchAgent starts the node at login (and now).".into());
        }
        Os::Linux => {
            host.write_file(&t.unit_path(), &systemd_unit(&t.daemon))?;
            let r = host.run("systemctl", &s(&["--user", "daemon-reload"]))?;
            if !r.ok {
                bail!(
                    "`systemctl --user daemon-reload` failed (is a systemd user session running?)"
                );
            }
            let r = host.run(
                "systemctl",
                &s(&["--user", "enable", "--now", SYSTEMD_UNIT]),
            )?;
            if !r.ok {
                bail!("`systemctl --user enable --now {SYSTEMD_UNIT}` failed");
            }
            out.push(
                "Autostart on: the systemd user service starts the node at login (and now).".into(),
            );
            out.push(
                "To keep it running when you are logged out: `loginctl enable-linger $USER`."
                    .into(),
            );
        }
    }
    Ok(out)
}

/// Turn autostart off. The running node is left alone (`crabfarm node --stop`).
pub fn uninstall(host: &mut dyn Host, t: &Target) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    match t.os {
        Os::Windows => match host.run_key_get()? {
            None => out.push("Autostart was not on.".to_string()),
            // Another install's entry is not ours to remove (CF-27).
            Some(v) if v != run_value(&t.daemon) => out.push(
                "Autostart points at another crabfarm install; it was left alone. \
                 Run `crabfarm service uninstall` from that install to turn it off."
                    .to_string(),
            ),
            Some(_) => {
                host.run_key_delete()?;
                out.push("Autostart off.".to_string());
            }
        },
        Os::MacOs => {
            let _ = host.run(
                "launchctl",
                &s(&["bootout", &format!("gui/{}/{LAUNCHD_LABEL}", t.uid)]),
            );
            let was = host.remove_file(&t.plist_path())?;
            out.push(if was {
                "Autostart off.".to_string()
            } else {
                "Autostart was not on.".to_string()
            });
        }
        Os::Linux => {
            let _ = host.run(
                "systemctl",
                &s(&["--user", "disable", "--now", SYSTEMD_UNIT]),
            );
            let was = host.remove_file(&t.unit_path())?;
            let _ = host.run("systemctl", &s(&["--user", "daemon-reload"]));
            out.push(if was {
                "Autostart off.".to_string()
            } else {
                "Autostart was not on.".to_string()
            });
        }
    }
    Ok(out)
}

pub struct Status {
    pub autostart: bool,
    /// The entry exists but points at a different crabfarmd than this one.
    pub stale: bool,
    pub node_running: bool,
}

pub fn status(host: &dyn Host, t: &Target) -> anyhow::Result<Status> {
    let (autostart, stale) = match t.os {
        Os::Windows => match host.run_key_get()? {
            Some(v) => (true, v != run_value(&t.daemon)),
            None => (false, false),
        },
        Os::MacOs => match host.read_file(&t.plist_path()) {
            Some(c) => (true, c != plist(&t.daemon, &t.data_dir)),
            None => (false, false),
        },
        Os::Linux => match host.read_file(&t.unit_path()) {
            Some(c) => (true, c != systemd_unit(&t.daemon)),
            None => (false, false),
        },
    };
    Ok(Status {
        autostart,
        stale,
        node_running: host.node_running(),
    })
}

/// After `farm create` / `farm join`: a node that autostart launched before the
/// machine was in a farm has already exited. launchd and systemd restart it on
/// their own (a failed start counts as a crash); the Windows Run key does not,
/// so start it here. Returns a line for the user when it did.
pub fn start_after_setup(host: &mut dyn Host, t: &Target) -> anyhow::Result<Option<String>> {
    // Only for the install the Run key points at: a crabfarm that merely
    // runs from somewhere else (a build tree, the tests) starts nothing.
    let ours = host
        .run_key_get()?
        .is_some_and(|v| v == run_value(&t.daemon));
    if t.os != Os::Windows || !ours || host.node_running() {
        return Ok(None);
    }
    host.start_detached(&t.daemon)?;
    Ok(Some("Started the node (autostart is on).".into()))
}

/// Whether autostart (the Run key, the LaunchAgent, the systemd unit) belongs
/// to the data directory in use. A custom `CRABFARM_HOME` is another node
/// (a test, a second node on one machine): the autostart entry starts the
/// default one, so nothing is read or started for it (CF-27).
pub fn autostart_covers_data_dir(custom_home: Option<&std::ffi::OsStr>) -> bool {
    custom_home.is_none_or(|h| h.is_empty())
}

/// The `crabfarmd` next to this executable.
pub fn daemon_path(exe: &Path) -> anyhow::Result<PathBuf> {
    let name = if cfg!(windows) {
        "crabfarmd.exe"
    } else {
        "crabfarmd"
    };
    let p = exe
        .parent()
        .context("crabfarm has no parent directory")?
        .join(name);
    if !p.is_file() {
        bail!(
            "{} not found next to {}; reinstall crabfarm",
            name,
            exe.display()
        );
    }
    Ok(p)
}

pub fn current_target() -> anyhow::Result<Target> {
    let exe = std::env::current_exe().context("cannot find my own executable")?;
    let home = directories::BaseDirs::new()
        .context("no home directory")?
        .home_dir()
        .to_path_buf();
    #[cfg(unix)]
    // SAFETY: getuid has no failure mode and no arguments.
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0;
    Ok(Target {
        os: Os::current(),
        home,
        uid,
        daemon: daemon_path(&exe)?,
        data_dir: crate::config::data_dir()?,
    })
}

/// Start `crabfarmd` with no console and NO inherited handles. `std`'s
/// `Command` always passes `bInheritHandles = TRUE`, so any inheritable handle
/// of the caller (the output pipe of an installer or a CI step that waits for
/// end-of-output) would stay open inside the node for as long as it runs. Here
/// the child gets no handles at all (`bInheritHandles = FALSE`, no standard
/// handles; the node logs to a file) and a neutral working directory, so it
/// does not pin the caller's folder either.
#[cfg(windows)]
fn spawn_detached_windows(daemon: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let wide = |p: &std::ffi::OsStr| -> Vec<u16> { p.encode_wide().collect() };
    let exe = wide(daemon.as_os_str());
    anyhow::ensure!(
        !exe.contains(&0) && !exe.contains(&(b'"' as u16)),
        "unusable path for {}",
        daemon.display()
    );
    let app: Vec<u16> = exe.iter().copied().chain(std::iter::once(0)).collect();
    // The quoted path is the whole command line.
    let mut cmdline: Vec<u16> = std::iter::once(b'"' as u16)
        .chain(exe.iter().copied())
        .chain([b'"' as u16, 0])
        .collect();
    let cwd: Vec<u16> = wide(std::env::temp_dir().as_os_str())
        .into_iter()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: zeroed STARTUPINFOW/PROCESS_INFORMATION are the documented
    // initial state; every pointer passed is valid and NUL-terminated for the
    // duration of the call.
    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            app.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // bInheritHandles = FALSE
            DETACHED_PROCESS
                | CREATE_NEW_PROCESS_GROUP
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT,
            std::ptr::null(),
            cwd.as_ptr(),
            &si,
            &mut pi,
        );
        if ok == 0 {
            anyhow::bail!(
                "start {}: {}",
                daemon.display(),
                std::io::Error::last_os_error()
            );
        }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    Ok(())
}

pub struct RealHost {
    data_dir: PathBuf,
}

impl RealHost {
    pub fn new(data_dir: PathBuf) -> RealHost {
        RealHost { data_dir }
    }
}

impl Host for RealHost {
    fn run_key_get(&self) -> anyhow::Result<Option<String>> {
        #[cfg(windows)]
        {
            Ok(crate::winreg::read_str(RUN_SUBKEY, RUN_VALUE_NAME)?.map(|v| v.value))
        }
        #[cfg(not(windows))]
        bail!("the Run key exists on Windows only")
    }

    fn run_key_set(&mut self, value: &str) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            crate::winreg::write_str(
                RUN_SUBKEY,
                RUN_VALUE_NAME,
                &crate::winreg::StrValue {
                    value: value.to_string(),
                    expand: false,
                },
            )
        }
        #[cfg(not(windows))]
        {
            let _ = value;
            bail!("the Run key exists on Windows only")
        }
    }

    fn run_key_delete(&mut self) -> anyhow::Result<bool> {
        #[cfg(windows)]
        {
            crate::winreg::delete_value(RUN_SUBKEY, RUN_VALUE_NAME)
        }
        #[cfg(not(windows))]
        bail!("the Run key exists on Windows only")
    }

    fn write_file(&mut self, path: &Path, content: &str) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        std::fs::write(path, content).with_context(|| format!("write {}", path.display()))
    }

    fn remove_file(&mut self, path: &Path) -> anyhow::Result<bool> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
        }
    }

    fn read_file(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn run(&mut self, program: &str, args: &[String]) -> anyhow::Result<CmdResult> {
        use std::process::Stdio;
        let mut cmd = std::process::Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::proctree::no_window(&mut cmd);
        let mut child = cmd.spawn().with_context(|| format!("start {program}"))?;
        let deadline = std::time::Instant::now() + CHILD_TIMEOUT;
        loop {
            if let Some(st) = child.try_wait()? {
                return Ok(CmdResult { ok: st.success() });
            }
            if std::time::Instant::now() >= deadline {
                // Our own child only.
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "{program} did not finish within {} s",
                    CHILD_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn start_detached(&mut self, daemon: &Path) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            spawn_detached_windows(daemon)
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::process::CommandExt;
            use std::process::Stdio;
            let mut cmd = std::process::Command::new(daemon);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            cmd.process_group(0);
            // The node is the user's process from here on; nothing is recorded.
            let _child = cmd
                .spawn()
                .with_context(|| format!("start {}", daemon.display()))?;
            Ok(())
        }
    }

    fn node_running(&self) -> bool {
        crate::daemon::node_running(&self.data_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Fake {
        run_key: Option<String>,
        files: BTreeMap<PathBuf, String>,
        commands: Vec<String>,
        started: Vec<PathBuf>,
        running: bool,
        fail_command: Option<&'static str>,
    }

    impl Host for Fake {
        fn run_key_get(&self) -> anyhow::Result<Option<String>> {
            Ok(self.run_key.clone())
        }
        fn run_key_set(&mut self, v: &str) -> anyhow::Result<()> {
            self.run_key = Some(v.into());
            Ok(())
        }
        fn run_key_delete(&mut self) -> anyhow::Result<bool> {
            Ok(self.run_key.take().is_some())
        }
        fn write_file(&mut self, p: &Path, c: &str) -> anyhow::Result<()> {
            self.files.insert(p.into(), c.into());
            Ok(())
        }
        fn remove_file(&mut self, p: &Path) -> anyhow::Result<bool> {
            Ok(self.files.remove(p).is_some())
        }
        fn read_file(&self, p: &Path) -> Option<String> {
            self.files.get(p).cloned()
        }
        fn run(&mut self, program: &str, args: &[String]) -> anyhow::Result<CmdResult> {
            let line = format!("{program} {}", args.join(" "));
            let ok = !self.fail_command.is_some_and(|f| line.contains(f));
            self.commands.push(line);
            Ok(CmdResult { ok })
        }
        fn start_detached(&mut self, d: &Path) -> anyhow::Result<()> {
            self.started.push(d.into());
            self.running = true;
            Ok(())
        }
        fn node_running(&self) -> bool {
            self.running
        }
    }

    fn target(os: Os) -> Target {
        Target {
            os,
            home: PathBuf::from("/home/u"),
            uid: 501,
            daemon: PathBuf::from("/opt/cf/crabfarmd"),
            data_dir: PathBuf::from("/data/crabfarm"),
        }
    }

    #[test]
    fn windows_run_value_is_the_quoted_daemon_path() {
        let mut t = target(Os::Windows);
        t.daemon = PathBuf::from(r"C:\Users\A B\AppData\Local\Programs\crabfarm\crabfarmd.exe");
        let mut h = Fake::default();
        install(&mut h, &t).unwrap();
        assert_eq!(
            h.run_key.as_deref(),
            Some(r#""C:\Users\A B\AppData\Local\Programs\crabfarm\crabfarmd.exe""#)
        );
    }

    #[test]
    fn windows_install_starts_the_node_once_and_uninstall_removes_the_key() {
        let t = target(Os::Windows);
        let mut h = Fake::default();
        install(&mut h, &t).unwrap();
        assert_eq!(h.started, vec![t.daemon.clone()]);
        // A second install does not start a second node.
        install(&mut h, &t).unwrap();
        assert_eq!(h.started.len(), 1);
        assert!(status(&h, &t).unwrap().autostart);
        uninstall(&mut h, &t).unwrap();
        assert!(h.run_key.is_none());
        assert!(!status(&h, &t).unwrap().autostart);
        // Nothing to remove the second time is not an error.
        uninstall(&mut h, &t).unwrap();
    }

    /// CF-27: `service uninstall` removes only the Run value that points at
    /// THIS install, and says so when it is another one.
    #[test]
    fn windows_uninstall_leaves_another_installs_entry_alone() {
        let t = target(Os::Windows);
        let mut other = target(Os::Windows);
        other.daemon = PathBuf::from(r"D:\elsewhere\crabfarmd.exe");
        let mut h = Fake {
            run_key: Some(run_value(&other.daemon)),
            ..Default::default()
        };
        let lines = uninstall(&mut h, &t).unwrap();
        assert_eq!(h.run_key, Some(run_value(&other.daemon)));
        assert!(lines[0].contains("another crabfarm install"), "{lines:?}");
        // Its own entry still goes.
        h.run_key = Some(run_value(&t.daemon));
        assert_eq!(uninstall(&mut h, &t).unwrap(), vec!["Autostart off."]);
        assert!(h.run_key.is_none());
    }

    /// The detached start works with a real (harmless, short-lived) program
    /// and returns without waiting for it.
    #[cfg(windows)]
    #[test]
    fn the_detached_start_launches_a_process_without_inheriting_handles() {
        let root = std::env::var("SystemRoot").expect("SystemRoot");
        let exe = PathBuf::from(root).join(r"System32\hostname.exe");
        assert!(exe.is_file());
        spawn_detached_windows(&exe).expect("start");
        assert!(spawn_detached_windows(Path::new(r"C:\no\such\crabfarmd.exe")).is_err());
    }

    #[test]
    fn a_custom_data_directory_has_no_autostart() {
        use std::ffi::OsStr;
        assert!(autostart_covers_data_dir(None));
        assert!(autostart_covers_data_dir(Some(OsStr::new(""))));
        assert!(!autostart_covers_data_dir(Some(OsStr::new(
            "/tmp/second-node"
        ))));
    }

    #[test]
    fn macos_plist_runs_at_load_and_restarts_only_after_a_crash() {
        let p = plist(Path::new("/Users/me/.local/bin/crabfarmd"), Path::new("/d"));
        assert!(p.contains("<key>Label</key>\n    <string>dev.renner.crabfarm</string>"));
        assert!(p.contains("<string>/Users/me/.local/bin/crabfarmd</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(p.contains("<key>SuccessfulExit</key>\n        <false/>"));
        // A path with markup characters cannot break out of the string.
        let p = plist(Path::new("/a&b/<x>"), Path::new("/d"));
        assert!(p.contains("/a&amp;b/&lt;x&gt;"));
    }

    #[test]
    fn macos_install_bootstraps_into_the_gui_domain() {
        let t = target(Os::MacOs);
        let mut h = Fake::default();
        install(&mut h, &t).unwrap();
        let plist_path = "/home/u/Library/LaunchAgents/dev.renner.crabfarm.plist";
        assert!(h.files.contains_key(Path::new(plist_path)));
        assert_eq!(
            h.commands,
            vec![
                "launchctl bootout gui/501/dev.renner.crabfarm".to_string(),
                format!("launchctl bootstrap gui/501 {}", t.plist_path().display()),
            ]
        );
        h.commands.clear();
        uninstall(&mut h, &t).unwrap();
        assert_eq!(
            h.commands,
            vec!["launchctl bootout gui/501/dev.renner.crabfarm"]
        );
        assert!(h.files.is_empty());
    }

    #[test]
    fn macos_bootstrap_failure_is_reported() {
        let mut h = Fake {
            fail_command: Some("bootstrap"),
            ..Default::default()
        };
        assert!(install(&mut h, &target(Os::MacOs)).is_err());
    }

    #[test]
    fn linux_unit_restarts_on_failure_and_installs_for_the_default_target() {
        let u = systemd_unit(Path::new("/home/u/.local/bin/crabfarmd"));
        assert!(u.contains("ExecStart=\"/home/u/.local/bin/crabfarmd\"\n"));
        assert!(u.contains("Restart=on-failure\n"));
        assert!(u.contains("WantedBy=default.target\n"));
        assert!(systemd_unit(Path::new("/a%b")).contains("/a%%b"));
    }

    #[test]
    fn linux_install_and_uninstall_use_systemctl_user() {
        let t = target(Os::Linux);
        let mut h = Fake::default();
        install(&mut h, &t).unwrap();
        assert!(
            h.files
                .contains_key(Path::new("/home/u/.config/systemd/user/crabfarm.service"))
        );
        assert_eq!(
            h.commands,
            vec![
                "systemctl --user daemon-reload",
                "systemctl --user enable --now crabfarm.service"
            ]
        );
        h.commands.clear();
        uninstall(&mut h, &t).unwrap();
        assert_eq!(
            h.commands,
            vec![
                "systemctl --user disable --now crabfarm.service",
                "systemctl --user daemon-reload"
            ]
        );
        assert!(h.files.is_empty());
    }

    #[test]
    fn a_node_that_left_for_lack_of_a_farm_is_started_after_setup_on_windows() {
        let t = target(Os::Windows);
        let mut h = Fake::default();
        // Autostart off: the user did not ask for a background node.
        assert!(start_after_setup(&mut h, &t).unwrap().is_none());
        install(&mut h, &t).unwrap();
        h.running = false; // it exited: no farm yet
        h.started.clear();
        assert!(start_after_setup(&mut h, &t).unwrap().is_some());
        assert_eq!(h.started.len(), 1);
        // Autostart points at another install: not ours to start.
        let mut other = t.clone();
        other.daemon = PathBuf::from("/elsewhere/crabfarmd");
        h.running = false;
        assert!(start_after_setup(&mut h, &other).unwrap().is_none());
        h.running = true;
        // Already running: nothing to do. Other systems restart by themselves.
        assert!(start_after_setup(&mut h, &t).unwrap().is_none());
        assert!(
            start_after_setup(&mut Fake::default(), &target(Os::Linux))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn status_flags_an_entry_for_another_install() {
        let t = target(Os::Linux);
        let mut h = Fake::default();
        install(&mut h, &t).unwrap();
        assert!(!status(&h, &t).unwrap().stale);
        let mut moved = t.clone();
        moved.daemon = PathBuf::from("/elsewhere/crabfarmd");
        assert!(status(&h, &moved).unwrap().stale);
    }
}
