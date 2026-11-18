//! CF-19 (Windows): key and token files carry an explicit protected DACL with
//! exactly one entry, for the current user. The test reads the DACL back from
//! the files the real code paths create.

#![cfg(windows)]

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
    EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl, GetTokenInformation,
    SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `ACCESS_ALLOWED_ACE_TYPE` of winnt.h.
const ACCESS_ALLOWED: u8 = 0;

/// What the DACL of `path` looks like: (protected, ACE types and whether each
/// ACE names the current user).
fn read_dacl(path: &Path) -> (bool, Vec<(u8, bool)>) {
    // SAFETY: Win32 queries on buffers owned here; the descriptor returned by
    // GetNamedSecurityInfoW is freed with LocalFree, the token handle closed.
    unsafe {
        let mut token = std::ptr::null_mut();
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        assert_ne!(
            GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len),
            0
        );
        let me = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        let rc = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        );
        assert_eq!(rc, 0, "GetNamedSecurityInfoW({})", path.display());
        let mut control = 0u16;
        let mut revision = 0u32;
        assert_ne!(
            GetSecurityDescriptorControl(sd, &mut control, &mut revision),
            0
        );
        let protected = control & SE_DACL_PROTECTED != 0;
        let mut aces = Vec::new();
        if !dacl.is_null() {
            let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
            assert_ne!(
                GetAclInformation(
                    dacl,
                    (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                    size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                ),
                0
            );
            for i in 0..info.AceCount {
                let mut ace = std::ptr::null_mut();
                assert_ne!(GetAce(dacl, i, &mut ace), 0);
                let header = &*(ace as *const ACCESS_ALLOWED_ACE);
                let sid = (&header.SidStart as *const u32).cast_mut().cast();
                aces.push((
                    header.Header.AceType,
                    header.Header.AceType == ACCESS_ALLOWED && EqualSid(sid, me) != 0,
                ));
            }
        }
        LocalFree(sd.cast());
        CloseHandle(token);
        (protected, aces)
    }
}

fn assert_owner_only(path: &Path) {
    assert!(path.exists(), "{} was not created", path.display());
    let (protected, aces) = read_dacl(path);
    assert!(
        protected,
        "{}: the DACL inherits from the parent directory",
        path.display()
    );
    assert_eq!(
        aces.len(),
        1,
        "{}: expected exactly one ACE, found {aces:?}",
        path.display()
    );
    assert!(
        aces[0].1,
        "{}: the one ACE must allow the current user",
        path.display()
    );
}

#[test]
fn identity_and_token_files_grant_only_the_current_user() {
    let d = tempfile::tempdir().unwrap();
    crabfarm::farm::create(d.path(), "alpha").unwrap();
    for f in [
        "farm/ca.key",
        "node.key",
        "blob.key",
        "node.id",
        "node.crt",
        "farm/ca.crt",
    ] {
        assert_owner_only(&d.path().join(f));
    }
    // The IPC token (and the port file written next to it).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (_listener, _token) = rt.block_on(crabfarm::ipc::bind(d.path())).unwrap();
    assert_owner_only(&d.path().join("ipc.token"));
    // Invite hashes.
    let mut invites = crabfarm::farm::Invites::load(d.path()).unwrap();
    invites.issue(d.path()).unwrap();
    assert_owner_only(&d.path().join("farm").join("invites.toml"));
}

/// CF-25: the DACL is there when the file is created, before anything is
/// written to it and without a later "restrict" step: the file is checked while
/// the handle that created it is still open and empty.
#[test]
fn a_private_file_is_restricted_at_creation() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("fresh.key");
    let f = crabfarm::secfile::create_new_private(&p).unwrap();
    assert_eq!(f.metadata().unwrap().len(), 0, "nothing written yet");
    assert_owner_only(&p);
    drop(f);
    // `CREATE_NEW`: a second creation of the same name is refused.
    assert!(crabfarm::secfile::create_new_private(&p).is_err());
}

#[test]
fn rewriting_a_key_keeps_the_restriction() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("k.key");
    crabfarm::secfile::write_private(&p, b"one").unwrap();
    crabfarm::secfile::write_private(&p, b"two").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"two");
    assert_owner_only(&p);
}
