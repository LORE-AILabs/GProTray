//! Native Windows 11 system tray UI and event-driven device controller.
//!
//! Zero polling: state refreshes only on startup, Windows device arrival/removal
//! (`WM_DEVICECHANGE`), sleep resume (`WM_POWERBROADCAST`), HID++ notifications,
//! or manual user actions.

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO,
    NIIF_NOSOUND, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW,
    NOTIFYICONDATAW_0, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    KillTimer, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterDeviceNotificationW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, TrackPopupMenuEx, TranslateMessage,
    UnregisterDeviceNotification, CW_USEDEFAULT, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE,
    DBT_DEVTYP_DEVICEINTERFACE, DEVICE_NOTIFY_WINDOW_HANDLE, DEV_BROADCAST_DEVICEINTERFACE_W,
    HDEVNOTIFY, HICON, HMENU, ICONINFO, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    MF_UNCHECKED, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_CREATE,
    WM_DESTROY, WM_DEVICECHANGE, WM_NULL, WM_POWERBROADCAST, WM_TIMER, WNDCLASSEXW,
    WS_OVERLAPPEDWINDOW,
};

use crate::autostart;
use crate::editor_ui;
use crate::hidpp::{EventSink, HidppEvent};
use crate::mouse::{ConnectionStatus, MouseManager, MouseSnapshot};
use crate::onboard::{LedEffect, ProfileEdit};
use crate::win_hid::str_to_utf16_z;

const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const WM_STATE_UPDATED: u32 = WM_APP + 2;
const WM_HID_EVENT: u32 = WM_APP + 3;
const WM_OP_FEEDBACK: u32 = WM_APP + 4;

const TIMER_DEBOUNCE_RESCAN: usize = 1;
const TIMER_WAKE_RESCAN: usize = 2;

const ID_REFRESH: usize = 1001;
const ID_AUTOSTART: usize = 1002;
const ID_QUIT: usize = 1003;
const ID_MODE_ONBOARD: usize = 1004;
const ID_OPEN_EDITOR: usize = 1005;

const ID_DPI_BASE: usize = 2000;
const ID_RATE_BASE: usize = 3000;
const ID_PROFILE_BASE: usize = 4000;
const ID_SLOT_TOGGLE_BASE: usize = 5000;
const ID_RGB_BASE: usize = 6000;

const GUID_DEVINTERFACE_HID: GUID = GUID::from_u128(0x4d1e55b2_f16f_11cf_88cb_001111000030);

const RGB_PRESETS: &[(&str, LedEffect)] = &[
    ("消灯 (Off)", LedEffect::Off),
    (
        "カラーサイクル",
        LedEffect::Cycle {
            period_ms: 8000,
            brightness: 100,
        },
    ),
    (
        "固定色: シアン",
        LedEffect::Fixed {
            r: 0x00,
            g: 0xD4,
            b: 0xFF,
        },
    ),
    (
        "固定色: ホワイト",
        LedEffect::Fixed {
            r: 0xFF,
            g: 0xFF,
            b: 0xFF,
        },
    ),
    (
        "固定色: レッド",
        LedEffect::Fixed {
            r: 0xFF,
            g: 0x20,
            b: 0x20,
        },
    ),
    (
        "固定色: グリーン",
        LedEffect::Fixed {
            r: 0x20,
            g: 0xFF,
            b: 0x60,
        },
    ),
    (
        "固定色: ブルー",
        LedEffect::Fixed {
            r: 0x20,
            g: 0x80,
            b: 0xFF,
        },
    ),
    (
        "固定色: パープル",
        LedEffect::Fixed {
            r: 0xB0,
            g: 0x40,
            b: 0xFF,
        },
    ),
    (
        "ブリージング: シアン",
        LedEffect::Breathing {
            r: 0x00,
            g: 0xD4,
            b: 0xFF,
            period_ms: 8000,
            brightness: 100,
        },
    ),
    (
        "ブリージング: レッド",
        LedEffect::Breathing {
            r: 0xFF,
            g: 0x20,
            b: 0x20,
            period_ms: 8000,
            brightness: 100,
        },
    ),
];

enum WorkerCmd {
    Rescan,
    SetDpi(u16),
    SetReportRate(u32),
    SetProfile(u16),
    SetOnboardMode(bool),
    SetSlotEnabled(u8, bool),
    SaveProfile {
        slot: u8,
        enable: bool,
        edit: ProfileEdit,
    },
    SetRgb(LedEffect),
    Shutdown,
}

#[derive(Default)]
struct MenuSelections {
    dpis: Vec<u16>,
    rates: Vec<u32>,
    sectors: Vec<u16>,
    slot_toggles: Vec<(u8, bool)>,
}

struct OpFeedback {
    ok: bool,
    message: String,
}

struct AppContext {
    _hwnd: HWND,
    hicon: HICON,
    hdev_notify: HDEVNOTIFY,
    taskbar_created_msg: u32,
    snapshot: Arc<Mutex<MouseSnapshot>>,
    selections: MenuSelections,
    pending_feedback: Arc<Mutex<Option<OpFeedback>>>,
    busy: Arc<AtomicBool>,
    suppress_hid_until: Arc<Mutex<Instant>>,
    cmd_tx: Sender<WorkerCmd>,
    worker: Option<JoinHandle<()>>,
}

unsafe impl Send for AppContext {}

static CTX: OnceLock<Mutex<Option<AppContext>>> = OnceLock::new();

fn with_ctx<R>(f: impl FnOnce(&mut AppContext) -> R) -> Option<R> {
    let lock = CTX.get()?;
    let mut guard = lock.lock().ok()?;
    guard.as_mut().map(f)
}

fn create_tray_icon() -> HICON {
    const W: i32 = 32;
    const H: i32 = 32;
    let mut pixels = vec![0u32; (W * H) as usize];

    for y in 0..H {
        for x in 0..W {
            let dx = (x as f32) - 15.5;
            let dy = (y as f32) - 15.5;
            let r = (dx * dx + dy * dy).sqrt();
            if r <= 14.5 {
                let mut color = if r >= 13.0 {
                    0xFF_00_B4_D8u32
                } else {
                    0xFF_1E_22_2Au32
                };

                let in_body = (dx / 6.2).powi(2) + ((dy - 0.8) / 9.2).powi(2) <= 1.0;
                if in_body {
                    if dy < -1.0 && dx.abs() < 1.1 {
                        color = if dy > -5.8 {
                            0xFF_00_D4_FF
                        } else {
                            0xFF_1E_22_2A
                        };
                    } else if (dy + 1.0).abs() < 0.75 {
                        color = 0xFF_1E_22_2A;
                    } else {
                        color = 0xFF_F0_F4_F8;
                    }
                }
                pixels[(y * W + x) as usize] = color;
            }
        }
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: W,
            biHeight: -H,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [unsafe { std::mem::zeroed() }],
    };

    unsafe {
        let hdc = CreateCompatibleDC(null_mut());
        let mut bits: *mut core::ffi::c_void = null_mut();
        let hbm_color: HBITMAP =
            CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if !hbm_color.is_null() && !bits.is_null() {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u32, pixels.len());
        }
        let mask_bytes = vec![0u8; ((W * H) / 8) as usize];
        let hbm_mask: HBITMAP = CreateBitmap(W, H, 1, 1, mask_bytes.as_ptr() as *const _);
        let ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let icon = CreateIconIndirect(&ii);
        if !hbm_color.is_null() {
            DeleteObject(hbm_color);
        }
        if !hbm_mask.is_null() {
            DeleteObject(hbm_mask);
        }
        DeleteDC(hdc);
        icon
    }
}

fn copy_utf16_fixed(dst: &mut [u16], text: &str) {
    dst.fill(0);
    let mut i = 0;
    for unit in text.encode_utf16() {
        if i + 1 >= dst.len() {
            break;
        }
        dst[i] = unit;
        i += 1;
    }
}

fn make_nid(hwnd: HWND, hicon: HICON, tooltip: &str) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY_CALLBACK;
    nid.hIcon = hicon;
    nid.Anonymous = NOTIFYICONDATAW_0 {
        uVersion: NOTIFYICON_VERSION_4,
    };
    copy_utf16_fixed(&mut nid.szTip, tooltip);
    nid
}

fn format_tooltip(snap: &MouseSnapshot) -> String {
    match snap.status {
        ConnectionStatus::Connected => {
            let dpi = snap
                .dpi
                .as_ref()
                .map(|d| format!("{} DPI", d.current))
                .unwrap_or_else(|| "DPI不明".into());
            let hz = snap
                .report_rate
                .as_ref()
                .map(|r| format!("{} Hz", r.current_hz))
                .unwrap_or_else(|| "レート不明".into());
            format!("GProTray - {}\n{} / {}", snap.name, dpi, hz)
        }
        ConnectionStatus::Sleeping => {
            format!("GProTray - {}\nスリープ中・未応答", snap.name)
        }
        ConnectionStatus::Disconnected => "GProTray - マウス未接続".into(),
    }
}

fn add_or_update_tray(hwnd: HWND, hicon: HICON, add: bool, snap: &MouseSnapshot) {
    let tip = format_tooltip(snap);
    let mut nid = make_nid(hwnd, hicon, &tip);
    unsafe {
        if add {
            Shell_NotifyIconW(NIM_ADD, &mut nid);
            Shell_NotifyIconW(NIM_SETVERSION, &mut nid);
        } else {
            Shell_NotifyIconW(NIM_MODIFY, &mut nid);
        }
    }
}

fn show_balloon(hwnd: HWND, hicon: HICON, title: &str, body: &str, is_error: bool) {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_INFO;
    nid.hIcon = hicon;
    nid.dwInfoFlags = if is_error {
        NIIF_ERROR
    } else {
        NIIF_INFO | NIIF_NOSOUND
    };
    copy_utf16_fixed(&mut nid.szInfoTitle, title);
    copy_utf16_fixed(&mut nid.szInfo, body);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &mut nid) };
}

fn append_text(menu: HMENU, flags: u32, id: usize, label: &str) {
    let w = str_to_utf16_z(label);
    unsafe { AppendMenuW(menu, flags, id, w.as_ptr()) };
}

fn append_sep(menu: HMENU) {
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, null()) };
}

fn build_dpi_choices(snap: &MouseSnapshot) -> Vec<u16> {
    let Some(dpi) = &snap.dpi else {
        return Vec::new();
    };
    let mut choices: Vec<u16> = vec![400, 800, 1000, 1200, 1600, 2400, 3200, 6400];
    if let Some(ob) = &snap.onboard {
        if let Some(p) = ob.profiles.iter().find(|p| p.sector == ob.active_sector) {
            choices.extend(p.dpi_levels.iter().copied());
        }
    }
    choices.push(dpi.current);
    choices.retain(|&v| {
        v >= dpi.min && v <= dpi.max && (dpi.supported.is_empty() || dpi.supported.contains(&v))
    });
    choices.sort_unstable();
    choices.dedup();
    choices
}

fn show_context_menu(hwnd: HWND, snap: &MouseSnapshot, is_busy: bool) -> (MenuSelections, usize) {
    let mut sel = MenuSelections::default();
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return (sel, 0);
    }

    let connected = snap.status == ConnectionStatus::Connected && !is_busy;

    let header = if is_busy {
        "GProTray (反映中...)"
    } else {
        "GProTray"
    };
    append_text(menu, MF_STRING | MF_GRAYED, 0, header);
    append_sep(menu);

    let status_line = match snap.status {
        ConnectionStatus::Connected => format!("{}：接続中", snap.name),
        ConnectionStatus::Sleeping => format!("{}：スリープ中・未応答", snap.name),
        ConnectionStatus::Disconnected => "マウス未接続".into(),
    };
    append_text(menu, MF_STRING | MF_GRAYED, 0, &status_line);

    if snap.status == ConnectionStatus::Connected {
        if let Some(dpi) = &snap.dpi {
            append_text(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                &format!("現在のDPI：{}", dpi.current),
            );
        }
        if let Some(rate) = &snap.report_rate {
            append_text(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                &format!("ポーリングレート：{} Hz", rate.current_hz),
            );
        }
        if let Some(ob) = &snap.onboard {
            let mode_text = if ob.is_host_mode() {
                "モード：ホスト制御 (ソフトウェア)".to_string()
            } else {
                let slot_label = ob
                    .profiles
                    .iter()
                    .find(|p| p.sector == ob.active_sector)
                    .map(|p| {
                        p.name
                            .clone()
                            .unwrap_or_else(|| format!("プロファイル {}", p.slot))
                    })
                    .unwrap_or_else(|| format!("0x{:04X}", ob.active_sector));
                format!("モード：オンボード ({slot_label})")
            };
            append_text(menu, MF_STRING | MF_GRAYED, 0, &mode_text);
        }
    }

    append_sep(menu);

    // DPI submenu
    if connected {
        if let Some(dpi) = &snap.dpi {
            sel.dpis = build_dpi_choices(snap);
            let sub = unsafe { CreatePopupMenu() };
            for (i, &val) in sel.dpis.iter().enumerate() {
                let checked = if val == dpi.current {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                };
                append_text(sub, MF_STRING | checked, ID_DPI_BASE + i, &format!("{val} DPI"));
            }
            let title = str_to_utf16_z("DPI変更");
            unsafe { AppendMenuW(menu, MF_POPUP, sub as usize, title.as_ptr()) };
        } else {
            append_text(menu, MF_STRING | MF_GRAYED, 0, "DPI変更");
        }
    } else {
        append_text(menu, MF_STRING | MF_GRAYED, 0, "DPI変更");
    }

    // Polling rate submenu
    if connected {
        if let Some(rate) = &snap.report_rate {
            sel.rates = rate.supported_hz.clone();
            let sub = unsafe { CreatePopupMenu() };
            for (i, &hz) in sel.rates.iter().enumerate() {
                let checked = if hz == rate.current_hz {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                };
                append_text(sub, MF_STRING | checked, ID_RATE_BASE + i, &format!("{hz} Hz"));
            }
            let title = str_to_utf16_z("ポーリングレート変更");
            unsafe { AppendMenuW(menu, MF_POPUP, sub as usize, title.as_ptr()) };
        } else {
            append_text(menu, MF_STRING | MF_GRAYED, 0, "ポーリングレート変更");
        }
    } else {
        append_text(menu, MF_STRING | MF_GRAYED, 0, "ポーリングレート変更");
    }

    // Profile switch submenu
    if connected {
        if let Some(ob) = &snap.onboard {
            let sub = unsafe { CreatePopupMenu() };
            if ob.is_host_mode() {
                append_text(sub, MF_STRING, ID_MODE_ONBOARD, "オンボードモードへ切替");
                append_sep(sub);
            }
            for p in &ob.profiles {
                let kind = if p.rom {
                    "ROM".to_string()
                } else {
                    format!("スロット {}", p.slot)
                };
                let name = p
                    .name
                    .as_deref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                let dpis = if p.dpi_levels.is_empty() {
                    String::new()
                } else {
                    let s: Vec<String> = p.dpi_levels.iter().map(ToString::to_string).collect();
                    format!(" [{}]", s.join("/"))
                };
                let active = !ob.is_host_mode() && ob.active_sector == p.sector;
                let checked = if active { MF_CHECKED } else { MF_UNCHECKED };
                let clickable = !p.rom && p.enabled && p.crc_valid == Some(true);
                if clickable {
                    let idx = sel.sectors.len();
                    sel.sectors.push(p.sector);
                    append_text(
                        sub,
                        MF_STRING | checked,
                        ID_PROFILE_BASE + idx,
                        &format!("{kind}{name}{dpis}"),
                    );
                } else {
                    let suffix = if p.rom {
                        " (工場出荷ROM)"
                    } else if !p.enabled {
                        " (無効)"
                    } else {
                        " (CRC不正)"
                    };
                    append_text(
                        sub,
                        MF_STRING | MF_GRAYED | checked,
                        0,
                        &format!("{kind}{name}{dpis}{suffix}"),
                    );
                }
            }
            let title = str_to_utf16_z("プロファイル切替");
            unsafe { AppendMenuW(menu, MF_POPUP, sub as usize, title.as_ptr()) };
        } else {
            append_text(menu, MF_STRING | MF_GRAYED, 0, "プロファイル切替");
        }
    } else {
        append_text(menu, MF_STRING | MF_GRAYED, 0, "プロファイル切替");
    }

    // Button / Onboard Profile Editor (P2)
    let has_onboard = connected && snap.onboard.is_some();
    if has_onboard {
        append_text(menu, MF_STRING, ID_OPEN_EDITOR, "ボタン・プロファイル設定...");
    } else {
        append_text(menu, MF_STRING | MF_GRAYED, 0, "ボタン・プロファイル設定...");
    }

    // Onboard slot management submenu (P2)
    if has_onboard {
        if let Some(ob) = &snap.onboard {
            let sub = unsafe { CreatePopupMenu() };
            append_text(sub, MF_STRING, ID_OPEN_EDITOR, "プロファイル編集を開く...");
            append_sep(sub);
            for p in ob.profiles.iter().filter(|p| !p.rom) {
                let is_active = p.sector == ob.active_sector;
                let nm = p
                    .name
                    .as_deref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                if is_active {
                    append_text(
                        sub,
                        MF_STRING | MF_CHECKED | MF_GRAYED,
                        0,
                        &format!("スロット {}{nm} (使用中)", p.slot),
                    );
                } else {
                    let idx = sel.slot_toggles.len();
                    sel.slot_toggles.push((p.slot, !p.enabled));
                    let checked = if p.enabled { MF_CHECKED } else { MF_UNCHECKED };
                    append_text(
                        sub,
                        MF_STRING | checked,
                        ID_SLOT_TOGGLE_BASE + idx,
                        &format!("スロット {}{nm} 有効", p.slot),
                    );
                }
            }
            let title = str_to_utf16_z("オンボードメモリ管理");
            unsafe { AppendMenuW(menu, MF_POPUP, sub as usize, title.as_ptr()) };
        }
    } else {
        append_text(menu, MF_STRING | MF_GRAYED, 0, "オンボードメモリ管理");
    }

    // Simple RGB submenu (P2)
    if has_onboard {
        if let Some(ob) = &snap.onboard {
            let cur_led = ob
                .profiles
                .iter()
                .find(|p| p.sector == ob.active_sector)
                .and_then(|p| p.leds.first().copied());
            let sub = unsafe { CreatePopupMenu() };
            for (i, (label, eff)) in RGB_PRESETS.iter().enumerate() {
                let checked = if cur_led == Some(*eff) {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                };
                append_text(sub, MF_STRING | checked, ID_RGB_BASE + i, label);
            }
            let title = str_to_utf16_z("簡易RGB設定");
            unsafe { AppendMenuW(menu, MF_POPUP, sub as usize, title.as_ptr()) };
        }
    } else {
        append_text(menu, MF_STRING | MF_GRAYED, 0, "簡易RGB設定");
    }

    append_text(menu, MF_STRING, ID_REFRESH, "状態を再読み込み");
    append_sep(menu);

    let auto_on = autostart::is_enabled().unwrap_or(false);
    append_text(
        menu,
        MF_STRING | if auto_on { MF_CHECKED } else { MF_UNCHECKED },
        ID_AUTOSTART,
        "Windows起動時に実行",
    );
    append_text(menu, MF_STRING, ID_QUIT, "終了");

    let mut pt = POINT { x: 0, y: 0 };
    let cmd = unsafe {
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenuEx(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            hwnd,
            null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        cmd
    };
    (sel, cmd.max(0) as usize)
}

fn post_feedback(
    hwnd_raw: usize,
    pending_feedback: &Arc<Mutex<Option<OpFeedback>>>,
    ok: bool,
    message: String,
) {
    if let Ok(mut guard) = pending_feedback.lock() {
        *guard = Some(OpFeedback { ok, message });
    }
    unsafe { PostMessageW(hwnd_raw as HWND, WM_OP_FEEDBACK, 0, 0) };
}

fn mark_write_settled(suppress_hid_until: &Arc<Mutex<Instant>>) {
    if let Ok(mut until) = suppress_hid_until.lock() {
        *until = Instant::now() + Duration::from_millis(900);
    }
}

fn preferred_dpi_path() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("GProTray").join("preferred_dpi.txt")
}

fn load_preferred_dpi() -> Option<u16> {
    std::fs::read_to_string(preferred_dpi_path())
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|&d| d > 0)
}

fn save_preferred_dpi(dpi: u16) {
    let path = preferred_dpi_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, dpi.to_string());
}

fn clear_preferred_dpi() {
    let _ = std::fs::remove_file(preferred_dpi_path());
}

fn apply_preferred_dpi_if_needed(
    mgr: &mut MouseManager,
    mut snap: MouseSnapshot,
    suppress_hid_until: &Arc<Mutex<Instant>>,
) -> MouseSnapshot {
    if snap.status != ConnectionStatus::Connected {
        return snap;
    }
    let Some(pref) = load_preferred_dpi() else {
        return snap;
    };
    if snap.dpi.as_ref().is_some_and(|d| d.current != pref) {
        if let Ok(updated) = mgr.set_dpi(pref) {
            mark_write_settled(suppress_hid_until);
            snap = updated;
        }
    }
    snap
}

fn spawn_worker(
    hwnd: HWND,
    snapshot: Arc<Mutex<MouseSnapshot>>,
    pending_feedback: Arc<Mutex<Option<OpFeedback>>>,
    busy: Arc<AtomicBool>,
    suppress_hid_until: Arc<Mutex<Instant>>,
    rx: Receiver<WorkerCmd>,
) -> JoinHandle<()> {
    let hwnd_raw = hwnd as usize;
    let suppress_in_sink = suppress_hid_until.clone();
    let sink: EventSink = Arc::new(move |ev| {
        if let Ok(until) = suppress_in_sink.lock() {
            if Instant::now() < *until {
                if matches!(ev, HidppEvent::Notification { .. }) {
                    return;
                }
            }
        }
        let relevant = match &ev {
            HidppEvent::TransportGone { .. } => true,
            HidppEvent::Notification { report, .. } => {
                report.len() >= 3 && (report[2] == 0x41 || report[0] == 0x11)
            }
        };
        if relevant {
            unsafe { PostMessageW(hwnd_raw as HWND, WM_HID_EVENT, 0, 0) };
        }
    });

    std::thread::Builder::new()
        .name("gpro-worker".into())
        .spawn(move || {
            let mut mgr = MouseManager::new_with_notifications(sink);
            let scanned = mgr.scan_and_read();
            let initial = apply_preferred_dpi_if_needed(&mut mgr, scanned, &suppress_hid_until);
            let mut was_connected = initial.status == ConnectionStatus::Connected;
            if let Ok(mut guard) = snapshot.lock() {
                *guard = initial;
            }
            unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };

            while let Ok(cmd) = rx.recv() {
                if matches!(cmd, WorkerCmd::Shutdown) {
                    mgr.clear_transports();
                    break;
                }
                busy.store(true, Ordering::SeqCst);
                match cmd {
                    WorkerCmd::Rescan => {
                        let mut snap = mgr.scan_and_read();
                        let now_connected = snap.status == ConnectionStatus::Connected;
                        if !was_connected && now_connected {
                            snap = apply_preferred_dpi_if_needed(
                                &mut mgr,
                                snap,
                                &suppress_hid_until,
                            );
                        }
                        if let Ok(mut guard) = snapshot.lock() {
                            *guard = snap;
                        }
                        unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                    }
                    WorkerCmd::SetDpi(dpi) => {
                        let res = mgr.set_dpi(dpi);
                        mark_write_settled(&suppress_hid_until);
                        match res {
                            Ok(snap) => {
                                save_preferred_dpi(dpi);
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    true,
                                    format!("DPIを {dpi} に変更しました（確認済）。"),
                                );
                            }
                            Err(e) => {
                                let snap = mgr.scan_and_read();
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    false,
                                    format!("DPI変更に失敗しました: {e}"),
                                );
                            }
                        }
                    }
                    WorkerCmd::SetReportRate(hz) => {
                        let res = mgr.set_report_rate(hz);
                        mark_write_settled(&suppress_hid_until);
                        match res {
                            Ok(snap) => {
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    true,
                                    format!("ポーリングレートを {hz} Hz に変更しました（確認済）。"),
                                );
                            }
                            Err(e) => {
                                let snap = mgr.scan_and_read();
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    false,
                                    format!("ポーリングレート変更に失敗しました: {e}"),
                                );
                            }
                        }
                    }
                    WorkerCmd::SetProfile(sector) => {
                        let res = mgr.set_active_profile(sector);
                        mark_write_settled(&suppress_hid_until);
                        match res {
                            Ok(snap) => {
                                clear_preferred_dpi();
                                let label = snap
                                    .onboard
                                    .as_ref()
                                    .and_then(|o| o.profiles.iter().find(|p| p.sector == sector))
                                    .map(|p| format!("スロット {}", p.slot))
                                    .unwrap_or_else(|| format!("0x{sector:04X}"));
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    true,
                                    format!("プロファイルを {label} に切り替えました（確認済）。"),
                                );
                            }
                            Err(e) => {
                                let snap = mgr.scan_and_read();
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    false,
                                    format!("プロファイル切替に失敗しました: {e}"),
                                );
                            }
                        }
                    }
                    WorkerCmd::SetOnboardMode(host_mode) => {
                        let res = mgr.set_onboard_mode(host_mode);
                        mark_write_settled(&suppress_hid_until);
                        match res {
                            Ok(snap) => {
                                clear_preferred_dpi();
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    true,
                                    "オンボードメモリモードに切り替えました（確認済）。".into(),
                                );
                            }
                            Err(e) => {
                                let snap = mgr.scan_and_read();
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    false,
                                    format!("モード切替に失敗しました: {e}"),
                                );
                            }
                        }
                    }
                    WorkerCmd::SetSlotEnabled(slot, enabled) => {
                        let res = mgr.set_slot_enabled(slot, enabled);
                        mark_write_settled(&suppress_hid_until);
                        match res {
                            Ok(snap) => {
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                let st = if enabled { "有効化" } else { "無効化" };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    true,
                                    format!("スロット {slot} を{st}しました（確認済）。"),
                                );
                            }
                            Err(e) => {
                                let snap = mgr.scan_and_read();
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    false,
                                    format!("スロット状態変更に失敗しました: {e}"),
                                );
                            }
                        }
                    }
                    WorkerCmd::SaveProfile { slot, enable, edit } => {
                        let res = mgr
                            .save_profile_edit(edit)
                            .and_then(|_| mgr.set_slot_enabled(slot, enable));
                        mark_write_settled(&suppress_hid_until);
                        match res {
                            Ok(snap) => {
                                clear_preferred_dpi();
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    true,
                                    format!("スロット {slot} の設定を保存・検証しました。"),
                                );
                            }
                            Err(e) => {
                                let snap = mgr.scan_and_read();
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    false,
                                    format!("プロファイル保存に失敗しました: {e}"),
                                );
                            }
                        }
                    }
                    WorkerCmd::SetRgb(effect) => {
                        let label = effect.label();
                        let res = mgr.set_simple_rgb(effect);
                        mark_write_settled(&suppress_hid_until);
                        match res {
                            Ok(snap) => {
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    true,
                                    format!("RGB設定を「{label}」に変更しました。"),
                                );
                            }
                            Err(e) => {
                                let snap = mgr.scan_and_read();
                                if let Ok(mut guard) = snapshot.lock() {
                                    *guard = snap;
                                }
                                unsafe { PostMessageW(hwnd_raw as HWND, WM_STATE_UPDATED, 0, 0) };
                                post_feedback(
                                    hwnd_raw,
                                    &pending_feedback,
                                    false,
                                    format!("RGB設定の変更に失敗しました: {e}"),
                                );
                            }
                        }
                    }
                    WorkerCmd::Shutdown => unreachable!(),
                }
                was_connected = snapshot
                    .lock()
                    .is_ok_and(|s| s.status == ConnectionStatus::Connected);
                busy.store(false, Ordering::SeqCst);
            }
        })
        .expect("worker thread spawn failed")
}

fn handle_command(hwnd: HWND, cmd_id: usize) {
    match cmd_id {
        ID_REFRESH => {
            with_ctx(|ctx| {
                let _ = ctx.cmd_tx.send(WorkerCmd::Rescan);
            });
        }
        ID_MODE_ONBOARD => {
            with_ctx(|ctx| {
                let _ = ctx.cmd_tx.send(WorkerCmd::SetOnboardMode(false));
            });
        }
        ID_OPEN_EDITOR => {
            let params = with_ctx(|ctx| {
                let snap = ctx.snapshot.lock().map(|s| s.clone()).unwrap_or_default();
                let ob = snap.onboard?;
                let (dpi_min, dpi_max) = snap
                    .dpi
                    .as_ref()
                    .map(|d| (d.min, d.max))
                    .unwrap_or((100, 25600));
                Some((ob, dpi_min, dpi_max, ctx.cmd_tx.clone()))
            })
            .flatten();
            if let Some((ob, dpi_min, dpi_max, tx)) = params {
                editor_ui::open_editor(
                    &ob,
                    dpi_min,
                    dpi_max,
                    None,
                    Box::new(move |slot, enable, edit| {
                        let _ = tx.send(WorkerCmd::SaveProfile { slot, enable, edit });
                    }),
                );
            }
        }
        ID_AUTOSTART => {
            let current = autostart::is_enabled().unwrap_or(false);
            let target = !current;
            let hicon = with_ctx(|ctx| ctx.hicon);
            if let Some(hicon) = hicon {
                match autostart::set_enabled(target) {
                    Ok(()) => {
                        let msg = if target {
                            "Windows起動時の自動実行を有効にしました。"
                        } else {
                            "Windows起動時の自動実行を無効にしました。"
                        };
                        show_balloon(hwnd, hicon, "GProTray", msg, false);
                    }
                    Err(e) => {
                        show_balloon(hwnd, hicon, "GProTray エラー", &e, true);
                    }
                }
            }
        }
        ID_QUIT => {
            unsafe { DestroyWindow(hwnd) };
        }
        id if (ID_DPI_BASE..ID_RATE_BASE).contains(&id) => {
            let idx = id - ID_DPI_BASE;
            with_ctx(|ctx| {
                if let Some(&dpi) = ctx.selections.dpis.get(idx) {
                    let _ = ctx.cmd_tx.send(WorkerCmd::SetDpi(dpi));
                }
            });
        }
        id if (ID_RATE_BASE..ID_PROFILE_BASE).contains(&id) => {
            let idx = id - ID_RATE_BASE;
            with_ctx(|ctx| {
                if let Some(&hz) = ctx.selections.rates.get(idx) {
                    let _ = ctx.cmd_tx.send(WorkerCmd::SetReportRate(hz));
                }
            });
        }
        id if (ID_PROFILE_BASE..ID_SLOT_TOGGLE_BASE).contains(&id) => {
            let idx = id - ID_PROFILE_BASE;
            with_ctx(|ctx| {
                if let Some(&sector) = ctx.selections.sectors.get(idx) {
                    let _ = ctx.cmd_tx.send(WorkerCmd::SetProfile(sector));
                }
            });
        }
        id if (ID_SLOT_TOGGLE_BASE..ID_RGB_BASE).contains(&id) => {
            let idx = id - ID_SLOT_TOGGLE_BASE;
            with_ctx(|ctx| {
                if let Some(&(slot, enable)) = ctx.selections.slot_toggles.get(idx) {
                    let _ = ctx.cmd_tx.send(WorkerCmd::SetSlotEnabled(slot, enable));
                }
            });
        }
        id if id >= ID_RGB_BASE => {
            let idx = id - ID_RGB_BASE;
            if let Some((_, eff)) = RGB_PRESETS.get(idx) {
                with_ctx(|ctx| {
                    let _ = ctx.cmd_tx.send(WorkerCmd::SetRgb(*eff));
                });
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let taskbar_recreated = with_ctx(|ctx| {
        (ctx.taskbar_created_msg != 0 && msg == ctx.taskbar_created_msg)
            .then(|| (ctx.hicon, ctx.snapshot.lock().map(|s| s.clone()).unwrap_or_default()))
    })
    .flatten();
    if let Some((hicon, snap)) = taskbar_recreated {
        add_or_update_tray(hwnd, hicon, true, &snap);
        return 0;
    }

    match msg {
        WM_CREATE => 0,
        WM_TRAY_CALLBACK => {
            let ev = (lparam as u32) & 0xFFFF;
            if ev == WM_CONTEXTMENU || ev == NIN_SELECT {
                let (snap, is_busy) = with_ctx(|ctx| {
                    let s = ctx.snapshot.lock().map(|s| s.clone()).unwrap_or_default();
                    let b = ctx.busy.load(Ordering::SeqCst);
                    (s, b)
                })
                .unwrap_or_default();
                let (sel, cmd_id) = show_context_menu(hwnd, &snap, is_busy);
                with_ctx(|ctx| {
                    ctx.selections = sel;
                    if cmd_id == 0 && !is_busy && snap.status == ConnectionStatus::Sleeping {
                        let _ = ctx.cmd_tx.send(WorkerCmd::Rescan);
                    }
                });
                if cmd_id > 0 {
                    handle_command(hwnd, cmd_id);
                }
            }
            0
        }
        WM_COMMAND => {
            handle_command(hwnd, wparam & 0xFFFF);
            0
        }
        WM_STATE_UPDATED => {
            let update = with_ctx(|ctx| {
                let snap = ctx.snapshot.lock().map(|s| s.clone()).unwrap_or_default();
                (ctx.hicon, snap)
            });
            if let Some((hicon, snap)) = update {
                add_or_update_tray(hwnd, hicon, false, &snap);
            }
            0
        }
        WM_OP_FEEDBACK => {
            KillTimer(hwnd, TIMER_DEBOUNCE_RESCAN);
            let feedback = with_ctx(|ctx| {
                ctx.pending_feedback
                    .lock()
                    .ok()
                    .and_then(|mut g| g.take())
                    .map(|fb| (ctx.hicon, fb))
            })
            .flatten();
            if let Some((hicon, fb)) = feedback {
                let title = if fb.ok {
                    "GProTray"
                } else {
                    "GProTray エラー"
                };
                show_balloon(hwnd, hicon, title, &fb.message, !fb.ok);
            }
            0
        }
        WM_HID_EVENT => {
            let suppressed = with_ctx(|ctx| {
                ctx.busy.load(Ordering::SeqCst)
                    || ctx
                        .suppress_hid_until
                        .lock()
                        .is_ok_and(|u| Instant::now() < *u)
            })
            .unwrap_or(false);
            if !suppressed {
                SetTimer(hwnd, TIMER_DEBOUNCE_RESCAN, 450, None);
            }
            0
        }
        WM_DEVICECHANGE => {
            let ev = wparam as u32;
            if ev == DBT_DEVICEARRIVAL || ev == DBT_DEVICEREMOVECOMPLETE {
                SetTimer(hwnd, TIMER_DEBOUNCE_RESCAN, 600, None);
            }
            0
        }
        WM_POWERBROADCAST => {
            let ev = wparam as u32;
            if ev == PBT_APMRESUMEAUTOMATIC || ev == PBT_APMRESUMESUSPEND {
                SetTimer(hwnd, TIMER_WAKE_RESCAN, 1200, None);
            }
            0
        }
        WM_TIMER => {
            KillTimer(hwnd, wparam);
            with_ctx(|ctx| {
                let _ = ctx.cmd_tx.send(WorkerCmd::Rescan);
            });
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_DEBOUNCE_RESCAN);
            KillTimer(hwnd, TIMER_WAKE_RESCAN);
            if let Some(lock) = CTX.get() {
                if let Ok(mut guard) = lock.lock() {
                    if let Some(mut ctx) = guard.take() {
                        let _ = ctx.cmd_tx.send(WorkerCmd::Shutdown);
                        if let Some(jh) = ctx.worker.take() {
                            let _ = jh.join();
                        }
                        if !ctx.hdev_notify.is_null() {
                            UnregisterDeviceNotification(ctx.hdev_notify);
                        }
                        let mut nid = make_nid(hwnd, ctx.hicon, "");
                        Shell_NotifyIconW(NIM_DELETE, &mut nid);
                        if !ctx.hicon.is_null() {
                            DestroyIcon(ctx.hicon);
                        }
                    }
                }
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn run() -> i32 {
    let hinst = unsafe { GetModuleHandleW(null()) };
    let class_name = str_to_utf16_z("GProTrayHiddenWindow");

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: null_mut(),
        hCursor: null_mut(),
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: null_mut(),
    };

    if unsafe { RegisterClassExW(&wc) } == 0 {
        return 1;
    }

    let win_title = str_to_utf16_z("GProTray");
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            win_title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        )
    };
    if hwnd.is_null() {
        return 1;
    }

    let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
        dbcc_size: std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
        dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE,
        dbcc_reserved: 0,
        dbcc_classguid: GUID_DEVINTERFACE_HID,
        dbcc_name: [0],
    };
    let hdev_notify = unsafe {
        RegisterDeviceNotificationW(
            hwnd,
            &filter as *const _ as *const _,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
    };

    let taskbar_msg_name = str_to_utf16_z("TaskbarCreated");
    let taskbar_created_msg = unsafe { RegisterWindowMessageW(taskbar_msg_name.as_ptr()) };

    let hicon = create_tray_icon();
    let snapshot = Arc::new(Mutex::new(MouseSnapshot::default()));
    let pending_feedback = Arc::new(Mutex::new(None));
    let busy = Arc::new(AtomicBool::new(false));
    let suppress_hid_until = Arc::new(Mutex::new(Instant::now()));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let worker = spawn_worker(
        hwnd,
        snapshot.clone(),
        pending_feedback.clone(),
        busy.clone(),
        suppress_hid_until.clone(),
        cmd_rx,
    );

    let ctx = AppContext {
        _hwnd: hwnd,
        hicon,
        hdev_notify,
        taskbar_created_msg,
        snapshot: snapshot.clone(),
        selections: MenuSelections::default(),
        pending_feedback,
        busy,
        suppress_hid_until,
        cmd_tx,
        worker: Some(worker),
    };
    let _ = CTX.set(Mutex::new(Some(ctx)));

    let init_snap = snapshot.lock().map(|s| s.clone()).unwrap_or_default();
    add_or_update_tray(hwnd, hicon, true, &init_snap);

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        let ret = unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) };
        if ret <= 0 {
            break;
        }
        if editor_ui::is_dialog_message(&mut msg) {
            continue;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    0
}
