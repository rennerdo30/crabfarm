//! The few registry operations `path` and `service` need, on HKEY_CURRENT_USER
//! only (no admin rights). Windows only; the callers hide it behind traits so
//! their logic is tested against in-memory fakes and never touches a real hive.

use anyhow::bail;
use std::ptr::null;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ,
    REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A string value and whether it is REG_EXPAND_SZ (else REG_SZ).
pub struct StrValue {
    pub value: String,
    pub expand: bool,
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful open and is closed once.
        unsafe { RegCloseKey(self.0) };
    }
}

fn open(subkey: &str, access: u32, create: bool) -> anyhow::Result<Option<Key>> {
    let sub = wide(subkey);
    let mut h: HKEY = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated wide string and out pointer.
    let rc = unsafe {
        if create {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                sub.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                access,
                null(),
                &mut h,
                std::ptr::null_mut(),
            )
        } else {
            RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut h)
        }
    };
    match rc {
        ERROR_SUCCESS => Ok(Some(Key(h))),
        ERROR_FILE_NOT_FOUND => Ok(None),
        e => bail!("open HKCU\\{subkey} failed (error {e})"),
    }
}

/// Read a string value of `HKCU\<subkey>` exactly as stored (no expansion).
pub fn read_str(subkey: &str, name: &str) -> anyhow::Result<Option<StrValue>> {
    let Some(key) = open(subkey, KEY_QUERY_VALUE, false)? else {
        return Ok(None);
    };
    let n = wide(name);
    let mut ty = 0u32;
    let mut size = 0u32;
    // SAFETY: size query with a null buffer.
    let rc = unsafe {
        RegQueryValueExW(
            key.0,
            n.as_ptr(),
            null(),
            &mut ty,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != ERROR_SUCCESS {
        bail!("read HKCU\\{subkey}\\{name} failed (error {rc})");
    }
    if ty != REG_SZ && ty != REG_EXPAND_SZ {
        bail!("HKCU\\{subkey}\\{name} is not a string value; not touching it");
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
    let mut size2 = (buf.len() * 2) as u32;
    // SAFETY: the buffer is at least `size2` bytes long.
    let rc = unsafe {
        RegQueryValueExW(
            key.0,
            n.as_ptr(),
            null(),
            &mut ty,
            buf.as_mut_ptr().cast(),
            &mut size2,
        )
    };
    if rc != ERROR_SUCCESS {
        bail!("read HKCU\\{subkey}\\{name} failed (error {rc})");
    }
    let chars = (size2 as usize / 2).min(buf.len());
    let end = buf[..chars].iter().position(|c| *c == 0).unwrap_or(chars);
    Ok(Some(StrValue {
        value: String::from_utf16_lossy(&buf[..end]),
        expand: ty == REG_EXPAND_SZ,
    }))
}

/// Write a string value, creating the key when needed.
pub fn write_str(subkey: &str, name: &str, v: &StrValue) -> anyhow::Result<()> {
    let Some(key) = open(subkey, KEY_SET_VALUE, true)? else {
        bail!("cannot create HKCU\\{subkey}");
    };
    let n = wide(name);
    let data = wide(&v.value);
    let ty = if v.expand { REG_EXPAND_SZ } else { REG_SZ };
    // SAFETY: `data` outlives the call; the size counts the terminating NUL.
    let rc = unsafe {
        RegSetValueExW(
            key.0,
            n.as_ptr(),
            0,
            ty,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    if rc != ERROR_SUCCESS {
        bail!("write HKCU\\{subkey}\\{name} failed (error {rc})");
    }
    Ok(())
}

/// Delete a value; `Ok(false)` when it did not exist.
pub fn delete_value(subkey: &str, name: &str) -> anyhow::Result<bool> {
    let Some(key) = open(subkey, KEY_SET_VALUE, false)? else {
        return Ok(false);
    };
    let n = wide(name);
    // SAFETY: valid handle and NUL-terminated name.
    match unsafe { RegDeleteValueW(key.0, n.as_ptr()) } {
        ERROR_SUCCESS => Ok(true),
        ERROR_FILE_NOT_FOUND => Ok(false),
        e => bail!("delete HKCU\\{subkey}\\{name} failed (error {e})"),
    }
}

/// Tell running programs (Explorer, new terminals) that the environment
/// changed. Bounded: a hung window cannot hold us longer than 3 s in total.
pub fn broadcast_environment_change() {
    let param = wide("Environment");
    let mut result = 0usize;
    // SAFETY: `param` lives through the call; the timeout bounds each window.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST as HWND,
            WM_SETTINGCHANGE,
            0,
            param.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            3000,
            &mut result,
        );
    }
}
