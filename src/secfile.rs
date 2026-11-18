//! Files in the data directory that other users of the machine must not read:
//! keys, the IPC token, invite hashes (DESIGN section 3).
//!
//! Unix: mode 0600 from creation. Windows (CF-19): an explicit, protected DACL
//! that grants only the current user (inheritance is cut), instead of relying on
//! the ACL the profile directory happens to carry. The DACL is set on the
//! temporary file while it is still empty, so the secret is never in a file
//! that others could open, and it travels with the file through the rename.

use anyhow::Context;
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
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    // Before the first byte is written. A failure is fatal: a key file that
    // could not be restricted must not be created.
    #[cfg(windows)]
    if let Err(e) = restrict_to_current_user(&tmp) {
        drop(f);
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context(format!("restrict {}", tmp.display())));
    }
    f.write_all(data)
        .with_context(|| format!("write {}", tmp.display()))?;
    f.sync_all().ok();
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Replace the security descriptor's DACL of `path` with one that has exactly
/// one entry, full access for the current user, and does not inherit.
#[cfg(windows)]
pub fn restrict_to_current_user(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, DACL_SECURITY_INFORMATION,
        GetLengthSid, GetTokenInformation, InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: plain Win32 calls. Every buffer below is owned by this function,
    // sized by the API itself (`GetTokenInformation`, `GetLengthSid`), and
    // outlives the calls that read it; the token handle is closed on every path.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            anyhow::bail!("OpenProcessToken: {}", std::io::Error::last_os_error());
        }
        let result = (|| {
            let mut len = 0u32;
            // The first call only reports the size.
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            if len == 0 {
                anyhow::bail!("GetTokenInformation: {}", std::io::Error::last_os_error());
            }
            // u64 elements keep the buffer aligned for TOKEN_USER.
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
                anyhow::bail!("GetTokenInformation: {}", std::io::Error::last_os_error());
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
                anyhow::bail!("InitializeAcl: {}", std::io::Error::last_os_error());
            }
            if AddAccessAllowedAce(acl, ACL_REVISION, FILE_ALL_ACCESS, sid) == 0 {
                anyhow::bail!("AddAccessAllowedAce: {}", std::io::Error::last_os_error());
            }
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let rc = SetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null(),
            );
            if rc != 0 {
                anyhow::bail!(
                    "SetNamedSecurityInfoW: {}",
                    std::io::Error::from_raw_os_error(rc as i32)
                );
            }
            Ok(())
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
}
