// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! HKCU registry helpers via Advapi32 — no `reg.exe` child process.
//!
//! Spawning `reg query` every status poll flashed a console and stalled the UI
//! thread on Windows. These calls stay in-process.

#![cfg(windows)]

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

type Hkey = *mut core::ffi::c_void;
type DWORD = u32;
type LONG = i32;

const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as usize as Hkey;
const KEY_READ: DWORD = 0x20019;
const KEY_SET_VALUE: DWORD = 0x0002;
const KEY_CREATE_SUB_KEY: DWORD = 0x0004;
const KEY_QUERY_VALUE: DWORD = 0x0001;
/// Open/create access for writing values under an existing or new subkey.
const KEY_WRITE: DWORD = KEY_SET_VALUE | KEY_CREATE_SUB_KEY;
const ERROR_SUCCESS: LONG = 0;
const ERROR_FILE_NOT_FOUND: LONG = 2;
const REG_OPTION_NON_VOLATILE: DWORD = 0;
const REG_SZ: DWORD = 1;
const REG_DWORD: DWORD = 4;

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(
        h_key: Hkey,
        lp_sub_key: *const u16,
        ul_options: DWORD,
        sam_desired: DWORD,
        phk_result: *mut Hkey,
    ) -> LONG;
    fn RegCreateKeyExW(
        h_key: Hkey,
        lp_sub_key: *const u16,
        reserved: DWORD,
        lp_class: *mut u16,
        dw_options: DWORD,
        sam_desired: DWORD,
        lp_security_attributes: *mut core::ffi::c_void,
        phk_result: *mut Hkey,
        lpdw_disposition: *mut DWORD,
    ) -> LONG;
    fn RegQueryValueExW(
        h_key: Hkey,
        lp_value_name: *const u16,
        lp_reserved: *mut DWORD,
        lp_type: *mut DWORD,
        lp_data: *mut u8,
        lpcb_data: *mut DWORD,
    ) -> LONG;
    fn RegSetValueExW(
        h_key: Hkey,
        lp_value_name: *const u16,
        reserved: DWORD,
        dw_type: DWORD,
        lp_data: *const u8,
        cb_data: DWORD,
    ) -> LONG;
    fn RegDeleteValueW(h_key: Hkey, lp_value_name: *const u16) -> LONG;
    fn RegCloseKey(h_key: Hkey) -> LONG;
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn open_hkcu(subkey: &str, access: DWORD) -> io::Result<Hkey> {
    let sub = wide(subkey);
    let mut hkey: Hkey = ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut hkey) };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(hkey)
}

/// Like `reg add`: create the subkey path if missing, then open for write.
fn create_hkcu(subkey: &str, access: DWORD) -> io::Result<Hkey> {
    let sub = wide(subkey);
    let mut hkey: Hkey = ptr::null_mut();
    let mut disposition: DWORD = 0;
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            access,
            ptr::null_mut(),
            &mut hkey,
            &mut disposition,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(hkey)
}

/// Read a REG_DWORD under HKCU. `None` if missing or wrong type.
pub fn hkcu_get_dword(subkey: &str, value: &str) -> Option<u32> {
    let hkey = open_hkcu(subkey, KEY_READ | KEY_QUERY_VALUE).ok()?;
    let name = wide(value);
    let mut ty: DWORD = 0;
    let mut data: DWORD = 0;
    let mut size: DWORD = std::mem::size_of::<DWORD>() as DWORD;
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            ptr::null_mut(),
            &mut ty,
            &mut data as *mut DWORD as *mut u8,
            &mut size,
        )
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if rc != ERROR_SUCCESS || ty != REG_DWORD {
        return None;
    }
    Some(data)
}

/// True when a REG_SZ value exists under HKCU (any non-empty length, including empty string).
pub fn hkcu_has_value(subkey: &str, value: &str) -> bool {
    let Ok(hkey) = open_hkcu(subkey, KEY_READ | KEY_QUERY_VALUE) else {
        return false;
    };
    let name = wide(value);
    let mut ty: DWORD = 0;
    let mut size: DWORD = 0;
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            ptr::null_mut(),
            &mut ty,
            ptr::null_mut(),
            &mut size,
        )
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    rc == ERROR_SUCCESS
}

pub fn hkcu_set_string(subkey: &str, value: &str, data: &str) -> io::Result<()> {
    // Create-or-open: matches `reg add`, which creates missing Run keys.
    let hkey = create_hkcu(subkey, KEY_WRITE)?;
    let name = wide(value);
    // REG_SZ data is a wide NUL-terminated string; cbData includes the NUL.
    let bytes: Vec<u16> = OsStr::new(data).encode_wide().chain(Some(0)).collect();
    let byte_len = (bytes.len() * 2) as DWORD;
    let rc = unsafe {
        RegSetValueExW(
            hkey,
            name.as_ptr(),
            0,
            REG_SZ,
            bytes.as_ptr() as *const u8,
            byte_len,
        )
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

pub fn hkcu_delete_value(subkey: &str, value: &str) -> io::Result<()> {
    let hkey = match open_hkcu(subkey, KEY_SET_VALUE) {
        Ok(h) => h,
        // Missing key ⇒ already disabled.
        Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => return Ok(()),
        Err(e) => return Err(e),
    };
    let name = wide(value);
    let rc = unsafe { RegDeleteValueW(hkey, name.as_ptr()) };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    Err(io::Error::from_raw_os_error(rc))
}
