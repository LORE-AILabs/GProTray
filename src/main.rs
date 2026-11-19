#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod editor_ui;
mod hidpp;
mod mouse;
mod onboard;
mod tray;
mod win_hid;

use std::ptr::null;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows_sys::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess};

use mouse::{ConnectionStatus, MouseManager};
use win_hid::str_to_utf16_z;

fn print_memory_stats() {
    unsafe {
        let mut pmc: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut pmc as *mut _ as *mut _,
            pmc.cb,
        ) != 0
        {
            let ws_mb = pmc.WorkingSetSize as f64 / (1024.0 * 1024.0);
            let priv_mb = pmc.PrivateUsage as f64 / (1024.0 * 1024.0);
            println!(
                "Memory: WorkingSet={:.2} MB ({} bytes), PrivateBytes={:.2} MB ({} bytes)",
                ws_mb, pmc.WorkingSetSize, priv_mb, pmc.PrivateUsage
            );
        }
    }
}

fn run_probe() -> i32 {
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    let mut mgr = MouseManager::new(Arc::new(|_| {}));
    let snap = mgr.scan_and_read();

    println!("=== GProTray Read-Only Probe ===");
    println!("Status: {:?}", snap.status);
    println!("Device Name: {}", snap.name);
    if let Some(fw_name) = &snap.firmware_name {
        println!("Firmware Name (0x0005): {fw_name}");
    }
    if snap.receiver_vid != 0 {
        println!(
            "Receiver/USB: VID={:04X} PID={:04X} ({}) dev_idx={:#04x}",
            snap.receiver_vid, snap.receiver_pid, snap.receiver_product, snap.device_index
        );
    }
    if let Some(uid) = &snap.unit_id {
        println!("Unit ID: {uid}");
    }
    if !snap.model_ids.is_empty() {
        let hexes: Vec<String> = snap.model_ids.iter().map(|id| format!("0x{id:04X}")).collect();
        println!("Model IDs: {}", hexes.join(", "));
    }
    if let Some(fw) = &snap.firmware_version {
        println!("Firmware: {fw}");
    }
    if let Some((maj, min)) = snap.protocol {
        println!("HID++ Protocol: {maj}.{min}");
    }
    println!("Features discovered: {}", snap.features.len());
    for f in snap.features.values() {
        println!(
            "  0x{:04X} (idx={:2}, flags=0x{:02X}, ver={})",
            f.id, f.index, f.flags, f.version
        );
    }
    if let Some(dpi) = &snap.dpi {
        println!(
            "DPI (0x{:04X}): current={}, default={:?}, range={}..={} ({} steps)",
            dpi.feature_id,
            dpi.current,
            dpi.default,
            dpi.min,
            dpi.max,
            dpi.supported.len()
        );
    }
    if let Some(rate) = &snap.report_rate {
        println!(
            "Report Rate (0x{:04X}): current={} Hz, supported={:?}",
            rate.feature_id, rate.current_hz, rate.supported_hz
        );
    }
    if let Some(ob) = &snap.onboard {
        println!(
            "Onboard Profiles (0x8100): mode={} ({}), active_sector=0x{:04X}, active_dpi_idx={:?}, dir_crc_ok={}",
            ob.mode,
            if ob.is_host_mode() { "Host" } else { "Onboard" },
            ob.active_sector,
            ob.active_dpi_index,
            ob.directory_crc_valid
        );
        println!(
            "  Info: fmt={}, profiles={}, rom={}, buttons={}, sectors={}, sector_size={}",
            ob.info.profile_format,
            ob.info.profile_count,
            ob.info.rom_profile_count,
            ob.info.button_count,
            ob.info.sector_count,
            ob.info.sector_size
        );
        for p in &ob.profiles {
            println!(
                "  {} slot={} sector=0x{:04X} enabled={} crc_ok={:?} name={:?} rate={:?}Hz default_dpi_idx={} dpis={:?}",
                if p.rom { "ROM " } else { "USER" },
                p.slot,
                p.sector,
                p.enabled,
                p.crc_valid,
                p.name,
                p.report_rate_hz,
                p.default_dpi_index,
                p.dpi_levels
            );
        }
    }
    println!("Autostart enabled: {:?}", autostart::is_enabled());
    for w in &snap.warnings {
        println!("Warning: {w}");
    }
    print_memory_stats();

    if snap.status == ConnectionStatus::Disconnected {
        1
    } else {
        0
    }
}

fn run_set_dpi(dpi: u16) -> i32 {
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    let mut mgr = MouseManager::new(Arc::new(|_| {}));
    let _ = mgr.scan_and_read();
    match mgr.set_dpi(dpi) {
        Ok(snap) => {
            let actual = snap.dpi.as_ref().map(|d| d.current);
            println!("DPI set & verified: target={dpi}, readback={actual:?}");
            0
        }
        Err(e) => {
            eprintln!("Failed to set DPI {dpi}: {e}");
            1
        }
    }
}

fn run_set_rate(hz: u32) -> i32 {
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    let mut mgr = MouseManager::new(Arc::new(|_| {}));
    let _ = mgr.scan_and_read();
    match mgr.set_report_rate(hz) {
        Ok(snap) => {
            let actual = snap.report_rate.as_ref().map(|r| r.current_hz);
            println!("Report rate set & verified: target={hz} Hz, readback={actual:?} Hz");
            0
        }
        Err(e) => {
            eprintln!("Failed to set report rate {hz} Hz: {e}");
            1
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--probe") {
        std::process::exit(run_probe());
    }
    if let Some(pos) = args.iter().position(|a| a == "--set-dpi") {
        let val = args.get(pos + 1).and_then(|s| s.parse::<u16>().ok());
        match val {
            Some(dpi) => std::process::exit(run_set_dpi(dpi)),
            None => {
                unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
                eprintln!("Usage: gpro-tray --set-dpi <DPI>");
                std::process::exit(2);
            }
        }
    }
    if let Some(pos) = args.iter().position(|a| a == "--set-rate") {
        let val = args.get(pos + 1).and_then(|s| s.parse::<u32>().ok());
        match val {
            Some(hz) => std::process::exit(run_set_rate(hz)),
            None => {
                unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
                eprintln!("Usage: gpro-tray --set-rate <HZ>");
                std::process::exit(2);
            }
        }
    }

    let mutex_name = str_to_utf16_z("Local\\GProTray_SingleInstance_Mutex");
    let hmutex = unsafe { CreateMutexW(null(), 1, mutex_name.as_ptr()) };
    if hmutex.is_null() {
        std::process::exit(1);
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(hmutex) };
        std::process::exit(0);
    }

    let code = tray::run();
    unsafe { CloseHandle(hmutex) };
    std::process::exit(code);
}
