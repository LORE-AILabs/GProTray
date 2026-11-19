//! Windows login autostart management via HKCU\Software\Microsoft\Windows\CurrentVersion\Run.
//! Requires no administrator privileges.

use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

use crate::win_hid::str_to_utf16_z;

const RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "GProTray";

pub fn current_exe_quoted() -> Result<String, String> {
    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetModuleFileNameW(null_mut(), buf.as_mut_ptr(), buf.len() as u32) };
    if len == 0 || (len as usize) >= buf.len() {
        return Err("実行ファイルパスの取得に失敗しました".into());
    }
    let exe = String::from_utf16_lossy(&buf[..len as usize]);
    Ok(format!("\"{exe}\""))
}

pub fn is_enabled() -> Result<bool, String> {
    let subkey = str_to_utf16_z(RUN_SUBKEY);
    let val_name = str_to_utf16_z(VALUE_NAME);
    let mut hkey = null_mut();
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut hkey,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!("レジストリ参照エラー ({rc})"));
    }

    let mut ty = 0u32;
    let mut size = 0u32;
    let qrc = unsafe {
        RegQueryValueExW(
            hkey,
            val_name.as_ptr(),
            null_mut(),
            &mut ty,
            null_mut(),
            &mut size,
        )
    };
    unsafe { RegCloseKey(hkey) };

    if qrc == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    if qrc != ERROR_SUCCESS {
        return Err(format!("レジストリ値参照エラー ({qrc})"));
    }
    Ok(ty == REG_SZ && size > 2)
}

pub fn set_enabled(enable: bool) -> Result<(), String> {
    let subkey = str_to_utf16_z(RUN_SUBKEY);
    let val_name = str_to_utf16_z(VALUE_NAME);
    let mut hkey = null_mut();
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!("レジストリ書込オープンエラー ({rc})"));
    }

    let op_rc = if enable {
        let cmd = match current_exe_quoted() {
            Ok(c) => str_to_utf16_z(&c),
            Err(e) => {
                unsafe { RegCloseKey(hkey) };
                return Err(e);
            }
        };
        let byte_len = (cmd.len() * 2) as u32;
        unsafe {
            RegSetValueExW(
                hkey,
                val_name.as_ptr(),
                0,
                REG_SZ,
                cmd.as_ptr() as *const u8,
                byte_len,
            )
        }
    } else {
        let drc = unsafe { RegDeleteValueW(hkey, val_name.as_ptr()) };
        if drc == ERROR_FILE_NOT_FOUND {
            ERROR_SUCCESS
        } else {
            drc
        }
    };

    unsafe { RegCloseKey(hkey) };

    if op_rc != ERROR_SUCCESS {
        return Err(format!("自動起動設定の更新に失敗しました ({op_rc})"));
    }
    Ok(())
}
