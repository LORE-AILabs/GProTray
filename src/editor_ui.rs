//! Lightweight native Win32 dialog for editing onboard profiles and button bindings.
//!
//! Created on demand and completely destroyed on close so idle memory stays minimal.

use std::ptr::{null, null_mut};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetStockObject, COLOR_BTNFACE, DEFAULT_GUI_FONT, HBRUSH,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, GetWindowTextW,
    IsDialogMessageW, MessageBoxW, RegisterClassExW, SendMessageW, SetForegroundWindow,
    SetWindowTextW, ShowWindow, ES_AUTOHSCROLL, ES_NUMBER, MB_ICONERROR, MB_ICONWARNING, MB_OK,
    MB_YESNO, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, WM_CLOSE, WM_COMMAND, WM_DESTROY,
    WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_DLGMODALFRAME, WS_GROUP, WS_SYSMENU,
    WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

#[link(name = "user32")]
extern "system" {
    fn EnableWindow(hwnd: HWND, benable: i32) -> i32;
}

use crate::onboard::{
    button_presets, decode_button_label, format_key_combo, g_pro_button_slot_name,
    named_hid_keys, ButtonPreset, OnboardProfile, OnboardSnapshot, ProfileEdit,
};
use crate::win_hid::str_to_utf16_z;

const WM_SETFONT: u32 = 0x0030;
const CB_ADDSTRING: u32 = 0x0143;
const CB_RESETCONTENT: u32 = 0x014B;
const CB_GETCURSEL: u32 = 0x0147;
const CB_SETCURSEL: u32 = 0x014E;
const CBN_SELCHANGE: u32 = 1;
const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;
const BST_UNCHECKED: usize = 0;
const BST_CHECKED: usize = 1;

const CBS_DROPDOWNLIST: u32 = 0x0003;
const BS_AUTOCHECKBOX: u32 = 0x0003;
const BS_DEFPUSHBUTTON: u32 = 0x0001;
const BS_PUSHBUTTON: u32 = 0x0000;
const BS_GROUPBOX: u32 = 0x0007;

const ID_SLOT_COMBO: usize = 101;
const ID_ENABLE_CHECK: usize = 102;
const ID_NAME_EDIT: usize = 103;
const ID_RATE_COMBO: usize = 104;
const ID_DPI_EDIT_BASE: usize = 110; // 110..114
const ID_DEFAULT_DPI_COMBO: usize = 115;
const ID_BTN_COMBO_BASE: usize = 120; // 120..127
const ID_CUSTOM_TARGET_COMBO: usize = 130;
const ID_CUSTOM_CTRL: usize = 131;
const ID_CUSTOM_SHIFT: usize = 132;
const ID_CUSTOM_ALT: usize = 133;
const ID_CUSTOM_WIN: usize = 134;
const ID_CUSTOM_KEY_COMBO: usize = 135;
const ID_CUSTOM_APPLY_BTN: usize = 136;
const ID_SAVE_BTN: usize = 190;
const ID_CANCEL_BTN: usize = 191;

const RATES: [u32; 4] = [1000, 500, 250, 125];

pub type SaveCallback = Box<dyn Fn(u8, bool, ProfileEdit) + Send + 'static>;

struct EditorState {
    hwnd: HWND,
    user_profiles: Vec<OnboardProfile>,
    active_sector: u16,
    dpi_min: u16,
    dpi_max: u16,
    slot_combo: HWND,
    enable_check: HWND,
    name_edit: HWND,
    rate_combo: HWND,
    dpi_edits: [HWND; 5],
    default_dpi_combo: HWND,
    btn_combos: [HWND; 8],
    btn_choices: [Vec<[u8; 4]>; 8],
    presets: Vec<ButtonPreset>,
    hid_keys: Vec<(u8, String)>,
    custom_target_combo: HWND,
    custom_ctrl: HWND,
    custom_shift: HWND,
    custom_alt: HWND,
    custom_win: HWND,
    custom_key_combo: HWND,
    on_save: SaveCallback,
}

unsafe impl Send for EditorState {}

static EDITOR: Mutex<Option<EditorState>> = Mutex::new(None);
static CLASS_REGISTERED: Mutex<bool> = Mutex::new(false);

fn set_font(hwnd: HWND) {
    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    unsafe { SendMessageW(hwnd, WM_SETFONT, font as WPARAM, 1) };
}

fn create_ctrl(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    ex_style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: usize,
) -> HWND {
    let c = str_to_utf16_z(class);
    let t = str_to_utf16_z(text);
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            c.as_ptr(),
            t.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            x,
            y,
            w,
            h,
            parent,
            id as _,
            GetModuleHandleW(null()),
            null(),
        )
    };
    set_font(hwnd);
    hwnd
}

fn combo_clear(combo: HWND) {
    unsafe { SendMessageW(combo, CB_RESETCONTENT, 0, 0) };
}

fn combo_add(combo: HWND, label: &str) {
    let w = str_to_utf16_z(label);
    unsafe { SendMessageW(combo, CB_ADDSTRING, 0, w.as_ptr() as LPARAM) };
}

fn combo_set_sel(combo: HWND, idx: usize) {
    unsafe { SendMessageW(combo, CB_SETCURSEL, idx, 0) };
}

fn combo_get_sel(combo: HWND) -> Option<usize> {
    let r = unsafe { SendMessageW(combo, CB_GETCURSEL, 0, 0) };
    if r < 0 {
        None
    } else {
        Some(r as usize)
    }
}

fn get_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize]).trim().to_string()
}

fn set_text(hwnd: HWND, text: &str) {
    let w = str_to_utf16_z(text);
    unsafe { SetWindowTextW(hwnd, w.as_ptr()) };
}

fn msgbox(hwnd: HWND, title: &str, body: &str, flags: u32) -> i32 {
    let t = str_to_utf16_z(title);
    let b = str_to_utf16_z(body);
    unsafe { MessageBoxW(hwnd, b.as_ptr(), t.as_ptr(), flags) }
}

fn load_slot_into_controls(st: &mut EditorState, idx: usize) {
    let Some(p) = st.user_profiles.get(idx).cloned() else {
        return;
    };

    let is_active = p.sector == st.active_sector;
    unsafe {
        SendMessageW(
            st.enable_check,
            BM_SETCHECK,
            if p.enabled { BST_CHECKED } else { BST_UNCHECKED },
            0,
        );
        // Active slot must stay enabled while active
        EnableWindow(st.enable_check, if is_active { 0 } else { 1 });
    }

    set_text(st.name_edit, p.name.as_deref().unwrap_or(""));

    let rate_idx = RATES
        .iter()
        .position(|&r| Some(r) == p.report_rate_hz)
        .unwrap_or(0);
    combo_set_sel(st.rate_combo, rate_idx);

    for i in 0..5 {
        let val = p.dpi_levels.get(i).map(|d| d.to_string()).unwrap_or_default();
        set_text(st.dpi_edits[i], &val);
    }

    let def_idx = usize::from(p.default_dpi_index).min(4);
    combo_set_sel(st.default_dpi_combo, def_idx);

    for (bi, combo) in st.btn_combos.iter().enumerate() {
        combo_clear(*combo);
        st.btn_choices[bi].clear();

        let cur = p.buttons.get(bi).copied().unwrap_or([0x80, 0x00, 0x00, 0x00]);
        let selected = if let Some(pos) = st.presets.iter().position(|pr| pr.raw == cur) {
            pos
        } else {
            let label = format!("現在値: {}", decode_button_label(cur));
            combo_add(*combo, &label);
            st.btn_choices[bi].push(cur);
            0
        };

        for pr in &st.presets {
            combo_add(*combo, &pr.label);
            st.btn_choices[bi].push(pr.raw);
        }
        combo_set_sel(*combo, selected);
    }
}

fn apply_custom_key_to_button(st: &mut EditorState) {
    let target_bi = combo_get_sel(st.custom_target_combo).unwrap_or(0);
    if target_bi >= 8 {
        return;
    }
    let key_idx = combo_get_sel(st.custom_key_combo).unwrap_or(0);
    let Some(&(hid_code, _)) = st.hid_keys.get(key_idx) else {
        return;
    };

    let mut mods = 0u8;
    unsafe {
        if SendMessageW(st.custom_ctrl, BM_GETCHECK, 0, 0) as usize == BST_CHECKED {
            mods |= 0x01;
        }
        if SendMessageW(st.custom_shift, BM_GETCHECK, 0, 0) as usize == BST_CHECKED {
            mods |= 0x02;
        }
        if SendMessageW(st.custom_alt, BM_GETCHECK, 0, 0) as usize == BST_CHECKED {
            mods |= 0x04;
        }
        if SendMessageW(st.custom_win, BM_GETCHECK, 0, 0) as usize == BST_CHECKED {
            mods |= 0x08;
        }
    }

    let raw = [0x80, 0x02, mods, hid_code];
    let combo = st.btn_combos[target_bi];
    if let Some(pos) = st.btn_choices[target_bi].iter().position(|&r| r == raw) {
        combo_set_sel(combo, pos);
        return;
    }

    // Rebuild list with custom key combo at index 0
    combo_clear(combo);
    st.btn_choices[target_bi].clear();
    let label = format_key_combo(mods, hid_code);
    combo_add(combo, &label);
    st.btn_choices[target_bi].push(raw);
    for pr in &st.presets {
        combo_add(combo, &pr.label);
        st.btn_choices[target_bi].push(pr.raw);
    }
    combo_set_sel(combo, 0);
}

fn handle_save(hwnd: HWND) {
    let validation: Result<(u8, bool, ProfileEdit), String> = (|| {
        let guard = EDITOR
            .lock()
            .map_err(|_| "内部状態ロックエラー".to_string())?;
        let st = guard
            .as_ref()
            .ok_or_else(|| "編集状態が見つかりません".to_string())?;
        let slot_idx = combo_get_sel(st.slot_combo).unwrap_or(0);
        let prof = st
            .user_profiles
            .get(slot_idx)
            .ok_or_else(|| "対象スロットが見つかりません".to_string())?;

        let enabled =
            unsafe { SendMessageW(st.enable_check, BM_GETCHECK, 0, 0) } as usize == BST_CHECKED;
        let name_txt = get_text(st.name_edit);
        let name = (!name_txt.is_empty()).then_some(name_txt);

        let rate_idx = combo_get_sel(st.rate_combo).unwrap_or(0);
        let report_rate_hz = RATES.get(rate_idx).copied().unwrap_or(1000);

        let mut dpi_levels = Vec::new();
        for (i, edit) in st.dpi_edits.iter().enumerate() {
            let s = get_text(*edit);
            if s.is_empty() {
                continue;
            }
            match s.parse::<u16>() {
                Ok(v) if v >= st.dpi_min && v <= st.dpi_max => dpi_levels.push(v),
                _ => {
                    return Err(format!(
                        "DPIステージ {} は {}〜{} の数値で入力してください。",
                        i + 1,
                        st.dpi_min,
                        st.dpi_max
                    ));
                }
            }
        }
        if dpi_levels.is_empty() {
            return Err("少なくとも1つのDPIステージを入力してください。".into());
        }

        let default_dpi_index = combo_get_sel(st.default_dpi_combo).unwrap_or(0) as u8;
        if usize::from(default_dpi_index) >= dpi_levels.len() {
            return Err("デフォルトDPIは入力済みのDPIステージ範囲内を選択してください。".into());
        }

        let mut buttons = Vec::with_capacity(8);
        for (bi, combo) in st.btn_combos.iter().enumerate() {
            let sel = combo_get_sel(*combo).unwrap_or(0);
            let raw = st.btn_choices[bi]
                .get(sel)
                .copied()
                .unwrap_or([0x80, 0x00, 0x00, 0x00]);
            buttons.push(raw);
        }

        Ok((
            prof.slot,
            enabled,
            ProfileEdit {
                slot: prof.slot,
                name,
                dpi_levels,
                default_dpi_index,
                report_rate_hz,
                buttons: Some(buttons),
                leds: None,
            },
        ))
    })();

    let parsed = match validation {
        Ok(v) => v,
        Err(err_msg) => {
            msgbox(hwnd, "入力エラー", &err_msg, MB_OK | MB_ICONERROR);
            return;
        }
    };

    if parsed
        .2
        .buttons
        .as_ref()
        .is_some_and(|b| b[0] != [0x80, 0x01, 0x00, 0x01])
    {
        let ans = msgbox(
            hwnd,
            "確認",
            "ボタン1（左クリック）が別の機能に変更されています。このまま保存しますか？",
            MB_YESNO | MB_ICONWARNING,
        );
        if ans != 6 {
            return;
        }
    }

    let on_save = EDITOR
        .lock()
        .ok()
        .and_then(|mut g| g.take().map(|st| st.on_save));
    if let Some(cb) = on_save {
        cb(parsed.0, parsed.1, parsed.2);
    }
    unsafe { DestroyWindow(hwnd) };
}

unsafe extern "system" fn editor_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = wparam & 0xFFFF;
            let code = ((wparam >> 16) & 0xFFFF) as u32;
            if id == ID_SLOT_COMBO && code == CBN_SELCHANGE {
                if let Ok(mut guard) = EDITOR.lock() {
                    if let Some(st) = guard.as_mut() {
                        if let Some(sel) = combo_get_sel(st.slot_combo) {
                            load_slot_into_controls(st, sel);
                        }
                    }
                }
            } else if id == ID_CUSTOM_APPLY_BTN {
                if let Ok(mut guard) = EDITOR.lock() {
                    if let Some(st) = guard.as_mut() {
                        apply_custom_key_to_button(st);
                    }
                }
            } else if id == ID_SAVE_BTN {
                handle_save(hwnd);
            } else if id == ID_CANCEL_BTN {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            if let Ok(mut guard) = EDITOR.lock() {
                let _ = guard.take();
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn is_dialog_message(msg: &mut MSG) -> bool {
    let hwnd = match EDITOR.lock() {
        Ok(guard) => guard.as_ref().map(|st| st.hwnd),
        Err(_) => None,
    };
    let Some(hwnd) = hwnd else {
        return false;
    };
    unsafe { IsDialogMessageW(hwnd, msg) != 0 }
}

pub fn open_editor(
    onboard: &OnboardSnapshot,
    dpi_min: u16,
    dpi_max: u16,
    initial_slot: Option<u8>,
    on_save: SaveCallback,
) {
    // If already open, bring existing window to front.
    let existing = EDITOR
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|st| st.hwnd));
    if let Some(hwnd) = existing {
        unsafe { SetForegroundWindow(hwnd) };
        return;
    }

    let user_profiles: Vec<OnboardProfile> = onboard
        .profiles
        .iter()
        .filter(|p| !p.rom)
        .cloned()
        .collect();
    if user_profiles.is_empty() {
        return;
    }

    let hinst = unsafe { GetModuleHandleW(null()) };
    let class_name = str_to_utf16_z("GProTrayEditorWindow");
    if let Ok(mut reg) = CLASS_REGISTERED.lock() {
        if !*reg {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: Some(editor_wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: null_mut(),
                hbrBackground: (COLOR_BTNFACE + 1) as HBRUSH,
                lpszMenuName: null(),
                lpszClassName: class_name.as_ptr(),
                hIconSm: null_mut(),
            };
            if unsafe { RegisterClassExW(&wc) } != 0 {
                *reg = true;
            }
        }
    }

    let win_w = 520i32;
    let win_h = 640i32;
    let sx = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let sy = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let x = ((sx - win_w) / 2).max(0);
    let y = ((sy - win_h) / 2).max(0);

    let title = str_to_utf16_z("GProTray - オンボードプロファイル / ボタン設定");
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_CAPTION | WS_SYSMENU,
            x,
            y,
            win_w,
            win_h,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        )
    };
    if hwnd.is_null() {
        return;
    }

    // Slot selector & enable
    create_ctrl(hwnd, "STATIC", "対象スロット:", 0, 0, 16, 16, 95, 20, 0);
    let slot_combo = create_ctrl(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST | WS_TABSTOP | WS_VSCROLL,
        0,
        115,
        12,
        215,
        160,
        ID_SLOT_COMBO,
    );
    for p in &user_profiles {
        let state = if p.sector == onboard.active_sector {
            "使用中"
        } else if p.enabled {
            "有効"
        } else {
            "無効"
        };
        let nm = p
            .name
            .as_deref()
            .map(|n| format!(" - {n}"))
            .unwrap_or_default();
        combo_add(slot_combo, &format!("スロット {} ({state}){nm}", p.slot));
    }
    let enable_check = create_ctrl(
        hwnd,
        "BUTTON",
        "スロットを有効化",
        BS_AUTOCHECKBOX | WS_TABSTOP,
        0,
        345,
        14,
        150,
        22,
        ID_ENABLE_CHECK,
    );

    // Profile settings group
    create_ctrl(
        hwnd,
        "BUTTON",
        "プロファイル設定",
        BS_GROUPBOX,
        0,
        14,
        44,
        476,
        142,
        0,
    );
    create_ctrl(hwnd, "STATIC", "プロファイル名:", 0, 0, 28, 68, 100, 20, 0);
    let name_edit = create_ctrl(
        hwnd,
        "EDIT",
        "",
        WS_BORDER | WS_TABSTOP | (ES_AUTOHSCROLL as u32),
        0,
        132,
        65,
        170,
        22,
        ID_NAME_EDIT,
    );
    create_ctrl(hwnd, "STATIC", "レート:", 0, 0, 316, 68, 52, 20, 0);
    let rate_combo = create_ctrl(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST | WS_TABSTOP,
        0,
        372,
        65,
        102,
        120,
        ID_RATE_COMBO,
    );
    for r in RATES {
        combo_add(rate_combo, &format!("{r} Hz"));
    }

    create_ctrl(
        hwnd,
        "STATIC",
        "DPI 1〜5:",
        0,
        0,
        28,
        104,
        95,
        20,
        0,
    );
    let dpi_edits = std::array::from_fn(|i| {
        create_ctrl(
            hwnd,
            "EDIT",
            "",
            WS_BORDER | WS_TABSTOP | (ES_NUMBER as u32),
            0,
            132 + (i as i32) * 68,
            101,
            60,
            22,
            ID_DPI_EDIT_BASE + i,
        )
    });

    create_ctrl(
        hwnd,
        "STATIC",
        "デフォルトDPI:",
        0,
        0,
        28,
        142,
        100,
        20,
        0,
    );
    let default_dpi_combo = create_ctrl(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST | WS_TABSTOP,
        0,
        132,
        138,
        140,
        140,
        ID_DEFAULT_DPI_COMBO,
    );
    for i in 1..=5 {
        combo_add(default_dpi_combo, &format!("ステージ {i}"));
    }

    // Button assignments group
    create_ctrl(
        hwnd,
        "BUTTON",
        "ボタン割り当て (1〜8番)",
        BS_GROUPBOX,
        0,
        14,
        196,
        476,
        345,
        0,
    );
    let btn_combos = std::array::from_fn(|i| {
        let y_row = 220 + (i as i32) * 29;
        create_ctrl(
            hwnd,
            "STATIC",
            g_pro_button_slot_name(i),
            0,
            0,
            28,
            y_row + 3,
            148,
            20,
            0,
        );
        create_ctrl(
            hwnd,
            "COMBOBOX",
            "",
            CBS_DROPDOWNLIST | WS_TABSTOP | WS_VSCROLL,
            0,
            182,
            y_row,
            292,
            260,
            ID_BTN_COMBO_BASE + i,
        )
    });

    // Custom key combination helper row
    create_ctrl(
        hwnd,
        "STATIC",
        "修飾キー組合せ:",
        0,
        0,
        28,
        463,
        100,
        20,
        0,
    );
    let custom_target_combo = create_ctrl(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST | WS_TABSTOP | WS_VSCROLL,
        0,
        132,
        460,
        148,
        180,
        ID_CUSTOM_TARGET_COMBO,
    );
    for i in 0..8 {
        combo_add(custom_target_combo, g_pro_button_slot_name(i));
    }
    combo_set_sel(custom_target_combo, 3); // Default to button 4 (side back)

    let custom_ctrl = create_ctrl(
        hwnd,
        "BUTTON",
        "Ctrl",
        BS_AUTOCHECKBOX | WS_TABSTOP,
        0,
        290,
        462,
        46,
        20,
        ID_CUSTOM_CTRL,
    );
    let custom_shift = create_ctrl(
        hwnd,
        "BUTTON",
        "Shift",
        BS_AUTOCHECKBOX | WS_TABSTOP,
        0,
        338,
        462,
        48,
        20,
        ID_CUSTOM_SHIFT,
    );
    let custom_alt = create_ctrl(
        hwnd,
        "BUTTON",
        "Alt",
        BS_AUTOCHECKBOX | WS_TABSTOP,
        0,
        388,
        462,
        42,
        20,
        ID_CUSTOM_ALT,
    );
    let custom_win = create_ctrl(
        hwnd,
        "BUTTON",
        "Win",
        BS_AUTOCHECKBOX | WS_TABSTOP,
        0,
        432,
        462,
        46,
        20,
        ID_CUSTOM_WIN,
    );

    create_ctrl(hwnd, "STATIC", "キー:", 0, 0, 28, 498, 44, 20, 0);
    let custom_key_combo = create_ctrl(
        hwnd,
        "COMBOBOX",
        "",
        CBS_DROPDOWNLIST | WS_TABSTOP | WS_VSCROLL,
        0,
        132,
        495,
        215,
        240,
        ID_CUSTOM_KEY_COMBO,
    );
    let hid_keys = named_hid_keys();
    for (_, label) in &hid_keys {
        combo_add(custom_key_combo, label);
    }
    combo_set_sel(custom_key_combo, 0);

    create_ctrl(
        hwnd,
        "BUTTON",
        "選択ボタンへ適用",
        BS_PUSHBUTTON | WS_TABSTOP,
        0,
        356,
        494,
        118,
        26,
        ID_CUSTOM_APPLY_BTN,
    );

    create_ctrl(
        hwnd,
        "BUTTON",
        "保存して書き込む",
        BS_DEFPUSHBUTTON | WS_TABSTOP | WS_GROUP,
        0,
        242,
        556,
        125,
        30,
        ID_SAVE_BTN,
    );
    create_ctrl(
        hwnd,
        "BUTTON",
        "キャンセル",
        BS_PUSHBUTTON | WS_TABSTOP,
        0,
        378,
        556,
        112,
        30,
        ID_CANCEL_BTN,
    );

    let init_idx = initial_slot
        .and_then(|s| user_profiles.iter().position(|p| p.slot == s))
        .or_else(|| {
            user_profiles
                .iter()
                .position(|p| p.sector == onboard.active_sector)
        })
        .unwrap_or(0);
    combo_set_sel(slot_combo, init_idx);

    let mut state = EditorState {
        hwnd,
        user_profiles,
        active_sector: onboard.active_sector,
        dpi_min,
        dpi_max,
        slot_combo,
        enable_check,
        name_edit,
        rate_combo,
        dpi_edits,
        default_dpi_combo,
        btn_combos,
        btn_choices: std::array::from_fn(|_| Vec::new()),
        presets: button_presets(),
        hid_keys,
        custom_target_combo,
        custom_ctrl,
        custom_shift,
        custom_alt,
        custom_win,
        custom_key_combo,
        on_save,
    };
    load_slot_into_controls(&mut state, init_idx);

    if let Ok(mut guard) = EDITOR.lock() {
        *guard = Some(state);
    }

    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::{LedEffect, OnboardInfo};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, PM_REMOVE,
    };

    #[test]
    fn editor_opens_populates_controls_and_closes_cleanly() {
        let ob = OnboardSnapshot {
            info: OnboardInfo {
                memory_model: 1,
                profile_format: 3,
                macro_format: 1,
                profile_count: 5,
                rom_profile_count: 1,
                button_count: 8,
                sector_count: 16,
                sector_size: 255,
                mechanical_layout: 10,
                various_info: 4,
            },
            mode: 1,
            active_sector: 1,
            active_dpi_index: Some(1),
            directory_crc_valid: true,
            profiles: vec![OnboardProfile {
                slot: 1,
                sector: 1,
                rom: false,
                enabled: true,
                crc_valid: Some(true),
                name: Some("Test".into()),
                report_rate_hz: Some(1000),
                default_dpi_index: 1,
                shift_dpi_index: 0,
                dpi_levels: vec![400, 800, 1600],
                buttons: vec![[0x80, 0x01, 0x00, 0x01]; 8],
                leds: vec![LedEffect::Off, LedEffect::Off],
            }],
        };

        open_editor(&ob, 100, 25600, Some(1), Box::new(|_, _, _| {}));

        let hwnd = EDITOR
            .lock()
            .unwrap()
            .as_ref()
            .map(|st| st.hwnd)
            .expect("editor window should be created");

        let mut msg: MSG = unsafe { std::mem::zeroed() };
        unsafe {
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                if !is_dialog_message(&mut msg) {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            DestroyWindow(hwnd);
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        assert!(EDITOR.lock().unwrap().is_none());
    }
}
