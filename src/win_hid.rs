//! Windows native HID device enumeration and overlapped I/O using `windows-sys`.
//! Opens devices with `FILE_SHARE_READ | FILE_SHARE_WRITE` so tools like
//! WirelessBattery can share the same HID interfaces without exclusive lock conflicts.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FlushQueue, HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid,
    HidD_GetPreparsedData, HidD_GetProductString, HidD_SetOutputReport, HidP_GetCaps,
    HIDD_ATTRIBUTES, HIDP_CAPS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};

pub const LOGITECH_VID: u16 = 0x046D;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct HidInterfaceInfo {
    pub path_utf16: Vec<u16>,
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub version: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub in_len: u16,
    pub out_len: u16,
    pub feat_len: u16,
    pub product: String,
}

fn utf16_z_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..end]).to_string_lossy().into_owned()
}

pub fn str_to_utf16_z(s: &str) -> Vec<u16> {
    OsString::from(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Enumerates all present Logitech (`VID 0x046D`) HID interfaces.
pub fn enumerate_logitech_hid() -> Vec<HidInterfaceInfo> {
    let mut out = Vec::new();
    let mut hid_guid = unsafe { std::mem::zeroed() };
    unsafe { HidD_GetHidGuid(&mut hid_guid) };

    let dev_info = unsafe {
        SetupDiGetClassDevsW(
            &hid_guid,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if dev_info == INVALID_HANDLE_VALUE as isize || dev_info == 0 {
        return out;
    }

    let mut if_data: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
    if_data.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;

    let mut index = 0u32;
    loop {
        let ok = unsafe {
            SetupDiEnumDeviceInterfaces(dev_info, null(), &hid_guid, index, &mut if_data)
        };
        if ok == 0 {
            break;
        }
        index += 1;

        let mut required = 0u32;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                dev_info,
                &if_data,
                null_mut(),
                0,
                &mut required,
                null_mut(),
            )
        };
        if required < 8 {
            continue;
        }

        let mut buf = vec![0u8; required as usize];
        let detail_ptr = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        let cb_size = if cfg!(target_pointer_width = "64") {
            8u32
        } else {
            6u32
        };
        unsafe { (*detail_ptr).cbSize = cb_size };

        let ok = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                dev_info,
                &if_data,
                detail_ptr,
                required,
                null_mut(),
                null_mut(),
            )
        };
        if ok == 0 {
            continue;
        }

        // DevicePath begins at byte offset 4 of SP_DEVICE_INTERFACE_DETAIL_DATA_W.
        let path_ptr = unsafe { buf.as_ptr().add(4) as *const u16 };
        let max_u16 = (required as usize - 4) / 2;
        let path_slice = unsafe { std::slice::from_raw_parts(path_ptr, max_u16) };
        let nul_pos = path_slice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(path_slice.len());
        let mut path_utf16 = path_slice[..nul_pos].to_vec();
        path_utf16.push(0);
        let path = utf16_z_to_string(&path_utf16);

        if !path.to_ascii_lowercase().contains("vid_046d") {
            continue;
        }

        let h = unsafe {
            CreateFileW(
                path_utf16.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            continue;
        }

        let mut attr: HIDD_ATTRIBUTES = unsafe { std::mem::zeroed() };
        attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
        let got_attr = unsafe { HidD_GetAttributes(h, &mut attr) } != 0;

        let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
        let mut preparsed: isize = 0;
        if unsafe { HidD_GetPreparsedData(h, &mut preparsed) } != 0 {
            unsafe {
                HidP_GetCaps(preparsed, &mut caps);
                HidD_FreePreparsedData(preparsed);
            }
        }

        let mut prod_buf = [0u16; 128];
        let got_prod = unsafe {
            HidD_GetProductString(
                h,
                prod_buf.as_mut_ptr() as *mut _,
                (prod_buf.len() * 2) as u32,
            )
        } != 0;

        unsafe { CloseHandle(h) };

        if !got_attr || attr.VendorID != LOGITECH_VID {
            continue;
        }

        let product = if got_prod {
            utf16_z_to_string(&prod_buf).trim().to_string()
        } else {
            String::new()
        };

        out.push(HidInterfaceInfo {
            path_utf16,
            path,
            vid: attr.VendorID,
            pid: attr.ProductID,
            version: attr.VersionNumber,
            usage_page: caps.UsagePage,
            usage: caps.Usage,
            in_len: caps.InputReportByteLength,
            out_len: caps.OutputReportByteLength,
            feat_len: caps.FeatureReportByteLength,
            product,
        });
    }

    unsafe { SetupDiDestroyDeviceInfoList(dev_info) };
    out
}

pub struct HidHandle {
    handle: HANDLE,
}

unsafe impl Send for HidHandle {}

impl HidHandle {
    pub fn open_overlapped(path_utf16: &[u16]) -> io::Result<Self> {
        let h = unsafe {
            CreateFileW(
                path_utf16.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return Err(io::Error::last_os_error());
        }
        unsafe { HidD_FlushQueue(h) };
        Ok(Self { handle: h })
    }

    pub fn write_report(&self, packet: &[u8], out_len: usize, timeout_ms: u32) -> io::Result<()> {
        let len = out_len.max(packet.len());
        let mut buf = vec![0u8; len];
        buf[..packet.len()].copy_from_slice(packet);

        let ev = unsafe { CreateEventW(null(), 1, 0, null()) };
        if ev.is_null() {
            return Err(io::Error::last_os_error());
        }

        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = ev;
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                &mut ov,
            )
        };

        let result = if ok != 0 {
            Ok(())
        } else {
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                let wait = unsafe { WaitForSingleObject(ev, timeout_ms) };
                if wait == WAIT_OBJECT_0 {
                    let done =
                        unsafe { GetOverlappedResult(self.handle, &ov, &mut written, 0) };
                    if done != 0 && written as usize == buf.len() {
                        Ok(())
                    } else {
                        Err(io::Error::last_os_error())
                    }
                } else {
                    unsafe {
                        CancelIoEx(self.handle, &ov);
                        GetOverlappedResult(self.handle, &ov, &mut written, 1);
                    }
                    Err(io::Error::new(io::ErrorKind::TimedOut, "HID write timed out"))
                }
            } else {
                Err(io::Error::from_raw_os_error(err as i32))
            }
        };

        unsafe { CloseHandle(ev) };

        if result.is_ok() {
            return Ok(());
        }

        // Fallback to HidD_SetOutputReport if WriteFile was rejected.
        let set_ok = unsafe {
            HidD_SetOutputReport(self.handle, buf.as_mut_ptr() as *mut _, buf.len() as u32)
        };
        if set_ok != 0 {
            Ok(())
        } else {
            result
        }
    }

    /// Reads one HID input report, cancelling immediately if `stop_event` is signaled.
    /// Returns `Ok(None)` when `stop_event` was signaled.
    pub fn read_report_cancellable(
        &self,
        in_len: usize,
        io_event: HANDLE,
        stop_event: HANDLE,
    ) -> io::Result<Option<Vec<u8>>> {
        unsafe { ResetEvent(io_event) };
        let mut buf = vec![0u8; in_len.max(64)];
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = io_event;

        let mut got = 0u32;
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                in_len as u32,
                &mut got,
                &mut ov,
            )
        };

        if ok != 0 {
            buf.truncate(got as usize);
            return Ok(Some(buf));
        }

        let err = unsafe { GetLastError() };
        if err != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(err as i32));
        }

        let handles = [io_event, stop_event];
        let wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        if wait == WAIT_OBJECT_0 {
            let done = unsafe { GetOverlappedResult(self.handle, &ov, &mut got, 0) };
            if done == 0 {
                return Err(io::Error::last_os_error());
            }
            buf.truncate(got as usize);
            Ok(Some(buf))
        } else {
            unsafe {
                CancelIoEx(self.handle, &ov);
                GetOverlappedResult(self.handle, &ov, &mut got, 1);
            }
            Ok(None)
        }
    }
}

impl Drop for HidHandle {
    fn drop(&mut self) {
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// Safe wrapper around a Win32 manual-reset event HANDLE.
pub struct WinEvent {
    handle: HANDLE,
}

unsafe impl Send for WinEvent {}
unsafe impl Sync for WinEvent {}

impl WinEvent {
    pub fn new_manual_reset() -> io::Result<Self> {
        let h = unsafe { CreateEventW(null(), 1, 0, null()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle: h })
    }

    pub fn raw(&self) -> HANDLE {
        self.handle
    }

    pub fn signal(&self) {
        unsafe { windows_sys::Win32::System::Threading::SetEvent(self.handle) };
    }
}

impl Drop for WinEvent {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}
