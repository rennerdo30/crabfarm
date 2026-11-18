//! Files in the data directory that other users of the machine must not read:
//! keys, the IPC token, invite hashes (DESIGN section 3).
//!
//! The restriction is part of the creation, never applied afterwards (CF-25):
//! Unix creates the file with mode 0600; Windows creates it with `CreateFileW`
//! and a security descriptor that carries an explicit, protected DACL granting
//! only the current user (inheritance is cut). So there is no moment at which a
//! secret, or even an empty file that will hold one, is open to other users, and
//! the DACL travels with the file through the rename.

use anyhow::Context;
use std::fs::File;
use std::path::Path;

/// Write `data` to `path` readable by the current user only. The content goes
/// to a temporary file first, so a crash never leaves a truncated key under the
/// real name and a reader never sees a half-written token.
pub fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut tmp_name = path.file_name().context("file name")?.to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    // A leftover from a crashed run may carry any permissions (or be a link):
    // it is removed, and the file below is created fresh and exclusively, so the
    // mode and the DACL always come from this call.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove stale {}", tmp.display())),
    }
    let mut f = create_new_private(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    if let Err(e) = f.write_all(data) {
        drop(f);
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("write {}", tmp.display()));
    }
    f.sync_all().ok();
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Create `path` (which must not exist) for writing, readable by the current
/// user only from the first instant.
#[cfg(unix)]
pub fn create_new_private(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Create `path` (which must not exist) for writing, with a protected DACL that
/// has exactly one entry, full access for the current user.
#[cfg(windows)]
pub fn create_new_private(path: &Path) -> std::io::Result<File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::{
        InitializeSecurityDescriptor, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    };
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};

    // The DACL lives in this buffer until `CreateFileW` has returned.
    let mut acl_buf = current_user_acl()?;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: plain Win32 calls on memory owned by this function. The security
    // descriptor and the ACL it points to outlive `CreateFileW`, which copies
    // what it needs; the handle returned is owned by the `File` made from it.
    unsafe {
        let mut sd: SECURITY_DESCRIPTOR = std::mem::zeroed();
        let psd = (&mut sd as *mut SECURITY_DESCRIPTOR).cast();
        // SECURITY_DESCRIPTOR_REVISION is 1.
        if InitializeSecurityDescriptor(psd, 1) == 0
            || SetSecurityDescriptorDacl(psd, 1, acl_buf.as_mut_ptr().cast(), 0) == 0
            || SetSecurityDescriptorControl(psd, SE_DACL_PROTECTED, SE_DACL_PROTECTED) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd,
            bInheritHandle: 0,
        };
        let handle = CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            &sa,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(File::from_raw_handle(handle))
    }
}

/// An ACL (in a `u32` buffer, so it is aligned) with exactly one entry: full
/// access for the user of the current process.
#[cfg(windows)]
fn current_user_acl() -> std::io::Result<Vec<u32>> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, GetLengthSid,
        GetTokenInformation, InitializeAcl, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let last = std::io::Error::last_os_error;
    // SAFETY: plain Win32 calls. Every buffer below is owned by this function,
    // sized by the API itself (`GetTokenInformation`, `GetLengthSid`), and
    // outlives the calls that read it; the token handle is closed on every path.
    // The ACL gets a copy of the SID, so the token buffer is not needed after.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(last());
        }
        let result = (|| {
            let mut len = 0u32;
            // The first call only reports the size.
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            if len == 0 {
                return Err(last());
            }
            // u64 elements keep the buffer aligned for TOKEN_USER.
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
                return Err(last());
            }
            let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
            let sid_len = GetLengthSid(sid) as usize;
            // ACL header, one ACE (its SidStart member overlaps the SID), DWORD aligned.
            let acl_size = (size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>()
                + sid_len)
                .next_multiple_of(4);
            let mut acl_buf = vec![0u32; acl_size / 4];
            let acl = acl_buf.as_mut_ptr().cast::<ACL>();
            if InitializeAcl(acl, acl_size as u32, ACL_REVISION) == 0 {
                return Err(last());
            }
            if AddAccessAllowedAce(acl, ACL_REVISION, FILE_ALL_ACCESS, sid) == 0 {
                return Err(last());
            }
            Ok(acl_buf)
        })();
        CloseHandle(token);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_is_replaced_atomically_and_no_temp_file_remains() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("secret.key");
        write_private(&p, b"one").unwrap();
        write_private(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    /// CF-25: a stale temporary file from a crashed run (here with a readable
    /// mode and other content) does not decide the permissions of the new one.
    #[test]
    fn a_stale_temporary_file_is_replaced_not_reused() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("secret.key");
        std::fs::write(d.path().join("secret.key.tmp"), b"stale and world readable").unwrap();
        write_private(&p, b"fresh").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"fresh");
        assert!(!d.path().join("secret.key.tmp").exists());
    }

    #[test]
    fn create_new_private_refuses_an_existing_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        drop(create_new_private(&p).unwrap());
        assert!(create_new_private(&p).is_err());
    }

    /// CF-25 (Unix): the mode is 0600 from the moment the file exists, not
    /// after a later chmod.
    #[cfg(unix)]
    #[test]
    fn unix_files_are_created_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        let f = create_new_private(&p).unwrap();
        let mode = f.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Whatever the umask: write_private ends with the same mode.
        let q = d.path().join("y");
        write_private(&q, b"k").unwrap();
        assert_eq!(
            std::fs::metadata(&q).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
