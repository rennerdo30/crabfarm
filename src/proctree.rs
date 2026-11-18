//! Ownership of the process tree of one rustc job (CF-9).
//!
//! `Child::kill` only ends rustc. rustc starts helpers of its own (the linker
//! for a proc-macro dylib, `cc`), and on Windows a surviving grandchild keeps
//! its files open, so the sandbox cannot be deleted and the plaintext sources
//! stay on disk. The process is therefore placed under a handle that reaches
//! the whole tree:
//!
//! - Windows: a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Closing
//!   the handle (also when the node itself dies) or `kill` ends every member.
//! - Unix: its own process group, killed as a group.
//!
//! Only the process this node spawned, and what that process started, is ever
//! touched.

use std::process::Command;

/// Prepare `cmd` so its process can be tracked as a tree. Call before spawning.
pub fn configure(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // pgid == pid of the child: a group of its own.
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };

    pub struct Tree(HANDLE);

    // A job handle is usable from any thread.
    unsafe impl Send for Tree {}
    unsafe impl Sync for Tree {}

    impl Tree {
        /// Create a kill-on-close job and put `process` into it.
        pub fn adopt(process: *mut c_void) -> anyhow::Result<Tree> {
            // SAFETY: plain Win32 calls with valid arguments; the handle is
            // owned by `Tree` from here on and closed exactly once in `drop`.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    anyhow::bail!("CreateJobObject failed: {}", std::io::Error::last_os_error());
                }
                let tree = Tree(job);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    anyhow::bail!("SetInformationJobObject failed: {}", std::io::Error::last_os_error());
                }
                if AssignProcessToJobObject(job, process) == 0 {
                    anyhow::bail!("AssignProcessToJobObject failed: {}", std::io::Error::last_os_error());
                }
                Ok(tree)
            }
        }

        pub fn kill(&self) {
            // SAFETY: the handle is valid until `drop`.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }

        pub fn finished(&mut self) {}
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE ends whatever is still in the job.
            // SAFETY: closes the handle this value owns.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    pub struct Tree {
        pgid: i32,
        armed: bool,
    }

    impl Tree {
        pub fn adopt(pid: u32) -> anyhow::Result<Tree> {
            let pgid = i32::try_from(pid).ok().filter(|p| *p > 1).ok_or_else(|| anyhow::anyhow!("invalid child pid"))?;
            Ok(Tree { pgid, armed: true })
        }

        pub fn kill(&self) {
            // SAFETY: signals the process group this node created for its
            // own child (`process_group(0)`); the leader has not been reaped
            // while the tree is armed, so the group id cannot be reused.
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
        }

        /// The child was reaped after a normal exit: its group id may be
        /// reused from now on, so the group is no longer signalled.
        pub fn finished(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            if self.armed {
                self.kill();
            }
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    pub struct Tree;
    impl Tree {
        pub fn adopt(_: u32) -> anyhow::Result<Tree> {
            anyhow::bail!("process trees are not supported on this platform")
        }
        pub fn kill(&self) {}
        pub fn finished(&mut self) {}
    }
}

/// The tracked process tree of one job. Dropping it ends the tree (Windows:
/// always; Unix: unless `finished` was called).
pub struct ProcessTree(imp::Tree);

impl ProcessTree {
    /// Track the tree of a freshly spawned `child` (spawned from a command that
    /// went through [`configure`]). On Windows a helper that rustc started in
    /// the instant before this call is not part of the job; rustc needs far
    /// longer than that to reach its first helper.
    pub fn adopt(child: &tokio::process::Child) -> anyhow::Result<ProcessTree> {
        #[cfg(windows)]
        {
            let handle = child.raw_handle().ok_or_else(|| anyhow::anyhow!("child already exited"))?;
            Ok(ProcessTree(imp::Tree::adopt(handle)?))
        }
        #[cfg(not(windows))]
        {
            let pid = child.id().ok_or_else(|| anyhow::anyhow!("child already exited"))?;
            Ok(ProcessTree(imp::Tree::adopt(pid)?))
        }
    }

    /// End every process of the tree now.
    pub fn kill(&self) {
        self.0.kill();
    }

    /// The root process exited normally and was reaped.
    pub fn finished(&mut self) {
        self.0.finished();
    }
}
