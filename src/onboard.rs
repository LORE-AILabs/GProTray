//! HID++ 0x8100 (OnboardProfiles) sector decoding, editing, and CRC-16/CCITT-FALSE verification.
//!
//! Reference / adapted under MIT License from:
//! - `omm-rust` (Copyright (c) 2026 Morten Nissen, MIT License)
//! - `Moreno.LogiPro` (MIT License)
//! - `libratbag` (Copyright (c) 2015-2017 Red Hat, Inc., David Herrmann, MIT License)

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::hidpp::{Error, Result, Transport};

const TIMEOUT: Duration = Duration::from_millis(1500);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(5);
const SETTLE_DELAY: Duration = Duration::from_millis(50);
const DIRECTORY_SECTOR: u16 = 0x0000;
const ROM_SECTOR_START: u16 = 0x0100;

const BUTTON_TABLE_OFFSET: usize = 0x20;
const NAME_OFFSET: usize = 0xA0;
const NAME_LEN: usize = 0x30;
const LED_TABLE_OFFSET: usize = 0xD0; // 208
const LED_RECORD_SIZE: usize = 11;
const MAX_BUTTONS: usize = 16;

pub mod func {
    pub const GET_INFO: u8 = 0;
    pub const SET_MODE: u8 = 1;
    pub const GET_MODE: u8 = 2;
    pub const SET_ACTIVE_PROFILE: u8 = 3;
    pub const GET_ACTIVE_PROFILE: u8 = 4;
    pub const READ: u8 = 5;
    pub const WRITE_START: u8 = 6;
    pub const WRITE_DATA: u8 = 7;
    pub const WRITE_END: u8 = 8;
    pub const GET_ACTIVE_DPI_INDEX: u8 = 0x0B;
}

/// CRC-16/CCITT-FALSE (poly 0x1021, seed 0xFFFF).
pub fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub fn crc_ok(sector: &[u8]) -> bool {
    if sector.len() < 2 {
        return false;
    }
    let n = sector.len();
    let stored = u16::from_be_bytes([sector[n - 2], sector[n - 1]]);
    crc16_ccitt(&sector[..n - 2]) == stored
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry {
    pub sector: u16,
    pub flags: u8,
    pub extra: u8,
}

impl DirEntry {
    pub fn enabled(&self) -> bool {
        (self.flags & 1) != 0
    }
}

pub fn parse_directory(d: &[u8]) -> Vec<DirEntry> {
    d[..d.len().saturating_sub(2)]
        .chunks_exact(4)
        .map(|c| DirEntry {
            sector: u16::from_be_bytes([c[0], c[1]]),
            flags: c[2],
            extra: c[3],
        })
        .take_while(|e| e.sector != 0xFFFF && e.sector != 0x0000)
        .collect()
}

pub fn build_directory(entries: &[DirEntry], size: usize) -> Result<Vec<u8>> {
    if entries.len() * 4 + 2 > size.saturating_sub(2) {
        return Err(Error::Other("ディレクトリサイズがセクタ上限を超えています".into()));
    }
    if !entries.iter().any(DirEntry::enabled) {
        return Err(Error::Other("少なくとも1つのプロファイルを有効のままにする必要があります".into()));
    }
    let mut buf = vec![0xFFu8; size];
    for (i, e) in entries.iter().enumerate() {
        buf[i * 4..i * 4 + 2].copy_from_slice(&e.sector.to_be_bytes());
        buf[i * 4 + 2] = e.flags;
        buf[i * 4 + 3] = e.extra;
    }
    let n = entries.len() * 4;
    buf[n..n + 2].copy_from_slice(&[0xFF, 0xFF]);
    let crc = crc16_ccitt(&buf[..size - 2]);
    buf[size - 2..].copy_from_slice(&crc.to_be_bytes());
    Ok(buf)
}

pub fn with_enabled(entry: DirEntry, enabled: bool) -> DirEntry {
    match (enabled, entry.extra) {
        (true, 0xFF) => DirEntry {
            flags: entry.flags | 1,
            extra: 0x00,
            ..entry
        },
        (true, extra) => DirEntry {
            flags: entry.flags | 1,
            extra,
            ..entry
        },
        (false, 0x00) => DirEntry {
            flags: entry.flags & !1,
            extra: 0xFF,
            ..entry
        },
        (false, extra) => DirEntry {
            flags: entry.flags & !1,
            extra,
            ..entry
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedEffect {
    Off,
    Fixed { r: u8, g: u8, b: u8 },
    Cycle { period_ms: u16, brightness: u8 },
    Breathing { r: u8, g: u8, b: u8, period_ms: u16, brightness: u8 },
}

impl LedEffect {
    pub fn decode(raw: &[u8]) -> Self {
        if raw.len() < LED_RECORD_SIZE {
            return LedEffect::Off;
        }
        match raw[0] {
            0x01 => LedEffect::Fixed {
                r: raw[1],
                g: raw[2],
                b: raw[3],
            },
            0x03 => LedEffect::Cycle {
                period_ms: u16::from_be_bytes([raw[6], raw[7]]),
                brightness: if raw[8] == 0 { 100 } else { raw[8] },
            },
            0x0A => LedEffect::Breathing {
                r: raw[1],
                g: raw[2],
                b: raw[3],
                period_ms: u16::from_be_bytes([raw[6], raw[7]]),
                brightness: if raw[8] == 0 { 100 } else { raw[8] },
            },
            _ => LedEffect::Off,
        }
    }

    pub fn encode(&self) -> [u8; LED_RECORD_SIZE] {
        let mut out = [0u8; LED_RECORD_SIZE];
        match *self {
            LedEffect::Off => {}
            LedEffect::Fixed { r, g, b } => {
                out[0] = 0x01;
                out[1] = r;
                out[2] = g;
                out[3] = b;
            }
            LedEffect::Cycle {
                period_ms,
                brightness,
            } => {
                out[0] = 0x03;
                let [ph, pl] = period_ms.clamp(1000, 20000).to_be_bytes();
                out[6] = ph;
                out[7] = pl;
                out[8] = if brightness >= 100 { 0 } else { brightness };
            }
            LedEffect::Breathing {
                r,
                g,
                b,
                period_ms,
                brightness,
            } => {
                out[0] = 0x0A;
                out[1] = r;
                out[2] = g;
                out[3] = b;
                let [ph, pl] = period_ms.clamp(1000, 20000).to_be_bytes();
                out[6] = ph;
                out[7] = pl;
                out[8] = if brightness >= 100 { 0 } else { brightness };
            }
        }
        out
    }

    pub fn label(&self) -> String {
        match *self {
            LedEffect::Off => "消灯 (Off)".into(),
            LedEffect::Fixed { r, g, b } => format!("固定色 #{r:02X}{g:02X}{b:02X}"),
            LedEffect::Cycle { .. } => "カラーサイクル".into(),
            LedEffect::Breathing { r, g, b, .. } => format!("ブリージング #{r:02X}{g:02X}{b:02X}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ButtonPreset {
    pub label: String,
    pub raw: [u8; 4],
}

pub fn hid_key_name(code: u8) -> Option<String> {
    let s = match code {
        0x04..=0x1D => return Some(((b'A' + (code - 0x04)) as char).to_string()),
        0x1E..=0x26 => return Some((code - 0x1D).to_string()),
        0x27 => "0",
        0x28 => "Enter",
        0x29 => "Esc",
        0x2A => "Backspace",
        0x2B => "Tab",
        0x2C => "Space",
        0x2D => "-",
        0x2E => "=",
        0x2F => "[",
        0x30 => "]",
        0x31 => "\\",
        0x33 => ";",
        0x34 => "'",
        0x35 => "`",
        0x36 => ",",
        0x37 => ".",
        0x38 => "/",
        0x39 => "CapsLock",
        0x3A..=0x45 => return Some(format!("F{}", code - 0x39)),
        0x46 => "PrintScreen",
        0x47 => "ScrollLock",
        0x48 => "Pause",
        0x49 => "Insert",
        0x4A => "Home",
        0x4B => "Page Up",
        0x4C => "Delete",
        0x4D => "End",
        0x4E => "Page Down",
        0x4F => "→ (Right)",
        0x50 => "← (Left)",
        0x51 => "↓ (Down)",
        0x52 => "↑ (Up)",
        0x68..=0x73 => return Some(format!("F{}", code - 0x68 + 13)),
        _ => return None,
    };
    Some(s.to_string())
}

pub fn named_hid_keys() -> Vec<(u8, String)> {
    // Ordered ergonomically: Nav/Edit -> F1..F24 -> A..Z -> 0..9 -> Symbols
    let mut order = Vec::new();
    order.extend_from_slice(&[
        0x4B, 0x4E, 0x4A, 0x4D, 0x49, 0x4C, // PageUp, PageDown, Home, End, Insert, Delete
        0x29, 0x2B, 0x28, 0x2C, 0x2A, // Esc, Tab, Enter, Space, Backspace
        0x52, 0x51, 0x50, 0x4F, // Up, Down, Left, Right
        0x46, 0x47, 0x48, 0x39, // PrintScreen, ScrollLock, Pause, CapsLock
    ]);
    order.extend(0x3A..=0x45); // F1..F12
    order.extend(0x68..=0x73); // F13..F24
    order.extend(0x04..=0x1D); // A..Z
    order.extend(0x1E..=0x27); // 1..0
    order.extend_from_slice(&[0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38]);

    order
        .into_iter()
        .filter_map(|c| hid_key_name(c).map(|n| (c, n)))
        .collect()
}

pub fn format_key_combo(mods: u8, hid_code: u8) -> String {
    let mut parts = Vec::new();
    if (mods & 0x01) != 0 {
        parts.push("Ctrl");
    }
    if (mods & 0x02) != 0 {
        parts.push("Shift");
    }
    if (mods & 0x04) != 0 {
        parts.push("Alt");
    }
    if (mods & 0x08) != 0 {
        parts.push("Win");
    }
    let key = hid_key_name(hid_code).unwrap_or_else(|| format!("0x{hid_code:02X}"));
    if parts.is_empty() {
        format!("キー: {key}")
    } else {
        format!("キー: {}+{key}", parts.join("+"))
    }
}

pub fn button_presets() -> Vec<ButtonPreset> {
    let mut out = vec![
        ButtonPreset { label: "左クリック".into(), raw: [0x80, 0x01, 0x00, 0x01] },
        ButtonPreset { label: "右クリック".into(), raw: [0x80, 0x01, 0x00, 0x02] },
        ButtonPreset { label: "中クリック".into(), raw: [0x80, 0x01, 0x00, 0x04] },
        ButtonPreset { label: "戻る (マウス4)".into(), raw: [0x80, 0x01, 0x00, 0x08] },
        ButtonPreset { label: "進む (マウス5)".into(), raw: [0x80, 0x01, 0x00, 0x10] },
        ButtonPreset { label: "DPIサイクル".into(), raw: [0x90, 0x05, 0x00, 0x00] },
        ButtonPreset { label: "DPIアップ".into(), raw: [0x90, 0x03, 0x00, 0x00] },
        ButtonPreset { label: "DPIダウン".into(), raw: [0x90, 0x04, 0x00, 0x00] },
        ButtonPreset { label: "DPIシフト".into(), raw: [0x90, 0x07, 0x00, 0x00] },
        ButtonPreset { label: "プロファイル切替".into(), raw: [0x90, 0x0A, 0x00, 0x00] },
        ButtonPreset { label: "G-Shift".into(), raw: [0x90, 0x0B, 0x00, 0x00] },
        ButtonPreset { label: "メディア: 再生/一時停止".into(), raw: [0x80, 0x03, 0x00, 0xCD] },
        ButtonPreset { label: "メディア: ミュート".into(), raw: [0x80, 0x03, 0x00, 0xE2] },
        ButtonPreset { label: "メディア: 音量+".into(), raw: [0x80, 0x03, 0x00, 0xE9] },
        ButtonPreset { label: "メディア: 音量-".into(), raw: [0x80, 0x03, 0x00, 0xEA] },
        ButtonPreset { label: "メディア: 次のトラック".into(), raw: [0x80, 0x03, 0x00, 0xB5] },
        ButtonPreset { label: "メディア: 前のトラック".into(), raw: [0x80, 0x03, 0x00, 0xB6] },
        ButtonPreset { label: "メディア: 停止".into(), raw: [0x80, 0x03, 0x00, 0xB7] },
        ButtonPreset { label: "ショートカット: Ctrl+C (コピー)".into(), raw: [0x80, 0x02, 0x01, 0x06] },
        ButtonPreset { label: "ショートカット: Ctrl+V (貼付)".into(), raw: [0x80, 0x02, 0x01, 0x19] },
        ButtonPreset { label: "ショートカット: Ctrl+X (切取)".into(), raw: [0x80, 0x02, 0x01, 0x1B] },
        ButtonPreset { label: "ショートカット: Ctrl+Z (元に戻す)".into(), raw: [0x80, 0x02, 0x01, 0x1D] },
        ButtonPreset { label: "ショートカット: Ctrl+W (タブを閉じる)".into(), raw: [0x80, 0x02, 0x01, 0x1A] },
        ButtonPreset { label: "ショートカット: Ctrl+T (新規タブ)".into(), raw: [0x80, 0x02, 0x01, 0x17] },
        ButtonPreset { label: "ショートカット: Alt+← (戻る)".into(), raw: [0x80, 0x02, 0x04, 0x50] },
        ButtonPreset { label: "ショートカット: Alt+→ (進む)".into(), raw: [0x80, 0x02, 0x04, 0x4F] },
        ButtonPreset { label: "ショートカット: Alt+Tab".into(), raw: [0x80, 0x02, 0x04, 0x2B] },
    ];

    for (code, name) in named_hid_keys() {
        out.push(ButtonPreset {
            label: format!("キー: {name}"),
            raw: [0x80, 0x02, 0x00, code],
        });
    }

    out.push(ButtonPreset {
        label: "無効 (割り当てなし)".into(),
        raw: [0x80, 0x00, 0x00, 0x00],
    });
    out
}

pub fn g_pro_button_slot_name(index: usize) -> &'static str {
    match index {
        0 => "1: 左クリック",
        1 => "2: 右クリック",
        2 => "3: 中クリック",
        3 => "4: 左側面 (戻る)",
        4 => "5: 左側面 (進む)",
        5 => "6: 底面DPIボタン",
        6 => "7: 右側面 (奥)",
        7 => "8: 右側面 (手前)",
        _ => "ボタン",
    }
}

pub fn decode_button_label(raw: [u8; 4]) -> String {
    let presets = button_presets();
    if let Some(preset) = presets.iter().find(|p| p.raw == raw) {
        return preset.label.clone();
    }
    match raw[0] {
        0x80 => match raw[1] {
            0x00 => "無効 (割り当てなし)".into(),
            0x01 => {
                let mask = u16::from_be_bytes([raw[2], raw[3]]);
                let btn = if mask == 0 { 0 } else { mask.trailing_zeros() + 1 };
                format!("マウスボタン {btn}")
            }
            0x02 => format_key_combo(raw[2], raw[3]),
            0x03 => format!("メディア 0x{:04X}", u16::from_be_bytes([raw[2], raw[3]])),
            other => format!("HID種別 0x{other:02X}"),
        },
        0x90 => match raw[1] {
            0x00 => "無効 (割り当てなし)".into(),
            other => format!("特殊機能 0x{other:02X}"),
        },
        0x00 => format!("マクロ (page {} off {})", raw[1], raw[3]),
        0xFF => "未設定/無効".into(),
        other => format!("不明 0x{other:02X}"),
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OnboardInfo {
    pub memory_model: u8,
    pub profile_format: u8,
    pub macro_format: u8,
    pub profile_count: u8,
    pub rom_profile_count: u8,
    pub button_count: u8,
    pub sector_count: u8,
    pub sector_size: u16,
    pub mechanical_layout: u8,
    pub various_info: u8,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OnboardProfile {
    pub slot: u8,
    pub sector: u16,
    pub rom: bool,
    pub enabled: bool,
    pub crc_valid: Option<bool>,
    pub name: Option<String>,
    pub report_rate_hz: Option<u32>,
    pub default_dpi_index: u8,
    pub shift_dpi_index: u8,
    pub dpi_levels: Vec<u16>,
    pub buttons: Vec<[u8; 4]>,
    pub leds: Vec<LedEffect>,
}

#[derive(Debug, Clone)]
pub struct OnboardSnapshot {
    pub info: OnboardInfo,
    /// 1 = Onboard memory mode, 2 = Host (software) mode.
    pub mode: u8,
    /// Active sector (0x0001..), or 0 when in Host mode.
    pub active_sector: u16,
    pub active_dpi_index: Option<u8>,
    pub directory_crc_valid: bool,
    pub profiles: Vec<OnboardProfile>,
}

impl OnboardSnapshot {
    pub fn is_host_mode(&self) -> bool {
        self.mode == 2
    }
}

fn decode_name(raw: &[u8]) -> Option<String> {
    if raw.iter().all(|&b| b == 0xFF) {
        return None;
    }
    let looks_utf16 = raw
        .chunks_exact(2)
        .take_while(|c| c != &[0, 0])
        .all(|c| c[1] == 0);
    let text = if looks_utf16 {
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0 && u != 0xFFFF)
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        let end = raw
            .iter()
            .position(|&b| b == 0 || b == 0xFF)
            .unwrap_or(raw.len());
        String::from_utf8_lossy(&raw[..end]).into_owned()
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn encode_name(name: Option<&str>) -> [u8; NAME_LEN] {
    let mut out = [0xFFu8; NAME_LEN];
    let Some(text) = name.map(str::trim).filter(|s| !s.is_empty()) else {
        return out;
    };
    out.fill(0);
    let mut pos = 0usize;
    for unit in text.encode_utf16() {
        if pos + 2 >= NAME_LEN {
            break;
        }
        let [lo, hi] = unit.to_le_bytes();
        out[pos] = lo;
        out[pos + 1] = hi;
        pos += 2;
    }
    out
}

pub fn parse_profile_sector(
    slot: u8,
    sector: u16,
    rom: bool,
    enabled: bool,
    data: &[u8],
    button_count: usize,
) -> OnboardProfile {
    let le16 = |off: usize| -> u16 { u16::from_le_bytes([data[off], data[off + 1]]) };
    let rate_ms = data.first().copied().unwrap_or(0);
    let report_rate_hz = (1..=8).contains(&rate_ms).then(|| 1000 / u32::from(rate_ms));
    let dpi_levels = if data.len() >= 13 {
        (0..5)
            .map(|i| le16(3 + i * 2))
            .filter(|&d| d != 0 && d != 0xFFFF)
            .collect()
    } else {
        Vec::new()
    };
    let count = button_count.min(MAX_BUTTONS);
    let buttons = if data.len() >= BUTTON_TABLE_OFFSET + count * 4 {
        (0..count)
            .map(|i| {
                let off = BUTTON_TABLE_OFFSET + i * 4;
                [data[off], data[off + 1], data[off + 2], data[off + 3]]
            })
            .collect()
    } else {
        Vec::new()
    };
    let name = if data.len() >= NAME_OFFSET + NAME_LEN {
        decode_name(&data[NAME_OFFSET..NAME_OFFSET + NAME_LEN])
    } else {
        None
    };
    let mut leds = Vec::new();
    if data.len() >= LED_TABLE_OFFSET + LED_RECORD_SIZE * 2 {
        for z in 0..2 {
            let off = LED_TABLE_OFFSET + z * LED_RECORD_SIZE;
            if data[off] != 0xFF {
                leds.push(LedEffect::decode(&data[off..off + LED_RECORD_SIZE]));
            }
        }
    }

    OnboardProfile {
        slot,
        sector,
        rom,
        enabled,
        crc_valid: (!rom).then(|| crc_ok(data)),
        name,
        report_rate_hz,
        default_dpi_index: data.get(1).copied().unwrap_or(0),
        shift_dpi_index: data.get(2).copied().unwrap_or(0),
        dpi_levels,
        buttons,
        leds,
    }
}

fn read_sector(
    t: &Transport,
    dev_idx: u8,
    feat_idx: u8,
    sector: u16,
    size: u16,
) -> Result<Vec<u8>> {
    let size = usize::from(size);
    let mut out = vec![0u8; size];
    let mut off = 0usize;
    while off < size {
        let at = if size - off < 16 {
            size.saturating_sub(16)
        } else {
            off
        };
        let [sh, sl] = sector.to_be_bytes();
        let [oh, ol] = (at as u16).to_be_bytes();
        let r = t.request20(dev_idx, feat_idx, func::READ, &[sh, sl, oh, ol], TIMEOUT)?;
        let n = 16.min(size - at);
        out[at..at + n].copy_from_slice(&r[..n]);
        off = at + 16;
    }
    Ok(out)
}

pub fn read_snapshot(t: &Transport, dev_idx: u8, feat_idx: u8) -> Result<OnboardSnapshot> {
    let gi = t.request20(dev_idx, feat_idx, func::GET_INFO, &[], TIMEOUT)?;
    let info = OnboardInfo {
        memory_model: gi[0],
        profile_format: gi[1],
        macro_format: gi[2],
        profile_count: gi[3],
        rom_profile_count: gi[4],
        button_count: gi[5],
        sector_count: gi[6],
        sector_size: u16::from_be_bytes([gi[7], gi[8]]),
        mechanical_layout: gi[9],
        various_info: gi[10],
    };

    let mode = t
        .request20(dev_idx, feat_idx, func::GET_MODE, &[], TIMEOUT)?
        .first()
        .copied()
        .unwrap_or(0);
    let active_raw = t.request20(dev_idx, feat_idx, func::GET_ACTIVE_PROFILE, &[], TIMEOUT)?;
    let active_sector = u16::from_be_bytes([active_raw[0], active_raw[1]]);
    let active_dpi_index = t
        .request20(dev_idx, feat_idx, func::GET_ACTIVE_DPI_INDEX, &[], TIMEOUT)
        .ok()
        .and_then(|r| r.first().copied());

    let mut profiles = Vec::new();
    let mut directory_crc_valid = false;
    let btn_count = usize::from(info.button_count);
    if info.sector_size >= 16 && info.sector_size <= 4096 {
        let dir_bytes = read_sector(t, dev_idx, feat_idx, DIRECTORY_SECTOR, info.sector_size)?;
        directory_crc_valid = crc_ok(&dir_bytes);
        if directory_crc_valid {
            for (i, entry) in parse_directory(&dir_bytes)
                .into_iter()
                .take(usize::from(info.profile_count))
                .enumerate()
            {
                let slot = (i + 1) as u8;
                if let Ok(sec) = read_sector(t, dev_idx, feat_idx, entry.sector, info.sector_size) {
                    profiles.push(parse_profile_sector(
                        slot,
                        entry.sector,
                        entry.sector >= ROM_SECTOR_START,
                        entry.enabled(),
                        &sec,
                        btn_count,
                    ));
                }
            }
        }
        for i in 1..=info.rom_profile_count {
            let sector = ROM_SECTOR_START + u16::from(i);
            if let Ok(sec) = read_sector(t, dev_idx, feat_idx, sector, info.sector_size) {
                profiles.push(parse_profile_sector(
                    i, sector, true, true, &sec, btn_count,
                ));
            }
        }
    }

    Ok(OnboardSnapshot {
        info,
        mode,
        active_sector,
        active_dpi_index,
        directory_crc_valid,
        profiles,
    })
}

/// Switches onboard/host mode (1 = Onboard, 2 = Host) and verifies via readback.
pub fn set_mode(t: &Transport, dev_idx: u8, feat_idx: u8, host_mode: bool) -> Result<u8> {
    let target = if host_mode { 2u8 } else { 1u8 };
    t.request20(dev_idx, feat_idx, func::SET_MODE, &[target], TIMEOUT)?;
    std::thread::sleep(SETTLE_DELAY);
    let actual = t
        .request20(dev_idx, feat_idx, func::GET_MODE, &[], TIMEOUT)?
        .first()
        .copied()
        .unwrap_or(0);
    if actual != target {
        return Err(Error::Other(format!(
            "モード切替の読み戻し不一致 (期待: {target}, 実際: {actual})"
        )));
    }
    Ok(actual)
}

/// Switches the active onboard profile to `sector` after validating directory enablement & CRC,
/// ensuring onboard mode is active, and verifying via readback.
pub fn set_active_profile(
    t: &Transport,
    dev_idx: u8,
    feat_idx: u8,
    sector: u16,
) -> Result<u16> {
    let snap = read_snapshot(t, dev_idx, feat_idx)?;
    let target = snap
        .profiles
        .iter()
        .find(|p| p.sector == sector)
        .ok_or_else(|| Error::Other(format!("プロファイルセクタ 0x{sector:04X} が見つかりません")))?;
    if !target.enabled && !target.rom {
        return Err(Error::Other(format!(
            "スロット {} は無効化されているため切り替えられません",
            target.slot
        )));
    }
    if !target.rom && target.crc_valid != Some(true) {
        return Err(Error::Other(format!(
            "スロット {} のCRCが不正なため切り替えを中止しました",
            target.slot
        )));
    }

    if snap.is_host_mode() {
        set_mode(t, dev_idx, feat_idx, false)?;
    }

    t.request20(
        dev_idx,
        feat_idx,
        func::SET_ACTIVE_PROFILE,
        &sector.to_be_bytes(),
        TIMEOUT,
    )?;
    std::thread::sleep(SETTLE_DELAY);

    let actual_raw = t.request20(dev_idx, feat_idx, func::GET_ACTIVE_PROFILE, &[], TIMEOUT)?;
    let actual = u16::from_be_bytes([actual_raw[0], actual_raw[1]]);
    if actual != sector {
        return Err(Error::Other(format!(
            "プロファイル切替の読み戻し不一致 (期待: 0x{sector:04X}, 実際: 0x{actual:04X})"
        )));
    }
    Ok(actual)
}

pub fn patch_report_rate_sector(original: &[u8], hz: u32) -> Result<Vec<u8>> {
    if original.len() < 0x20 {
        return Err(Error::Other("プロファイルセクタのサイズが不正です".into()));
    }
    if !crc_ok(original) {
        return Err(Error::Other(
            "現在のオンボードプロファイルのCRCが不正なため書き込みを中止しました".into(),
        ));
    }
    let period_ms = match hz {
        1000 => 1u8,
        500 => 2,
        250 => 4,
        125 => 8,
        _ => {
            return Err(Error::Other(format!(
                "{hz} Hz はオンボードプロファイルに保存できません (125/250/500/1000 Hzのみ)"
            )))
        }
    };
    let mut patched = original.to_vec();
    patched[0] = period_ms;
    let n = patched.len();
    let crc = crc16_ccitt(&patched[..n - 2]);
    patched[n - 2..n].copy_from_slice(&crc.to_be_bytes());
    Ok(patched)
}

#[derive(Debug, Clone)]
pub struct ProfileEdit {
    pub slot: u8,
    pub name: Option<String>,
    pub dpi_levels: Vec<u16>,
    pub default_dpi_index: u8,
    pub report_rate_hz: u32,
    pub buttons: Option<Vec<[u8; 4]>>,
    pub leds: Option<[LedEffect; 2]>,
}

pub fn patch_profile_sector_full(
    original: &[u8],
    edit: &ProfileEdit,
    dpi_min: u16,
    dpi_max: u16,
    button_count: usize,
) -> Result<Vec<u8>> {
    if original.len() < NAME_OFFSET + NAME_LEN + 2 {
        return Err(Error::Other("プロファイルセクタが短すぎます".into()));
    }
    if !crc_ok(original) {
        return Err(Error::Other(
            "対象プロファイルのCRCが不正なため書き込みを中止しました".into(),
        ));
    }
    if edit.dpi_levels.is_empty() || edit.dpi_levels.len() > 5 {
        return Err(Error::Other("DPIステージは1〜5個の範囲で指定してください".into()));
    }
    for &d in &edit.dpi_levels {
        if d < dpi_min || d > dpi_max {
            return Err(Error::Other(format!(
                "DPI {d} が対応範囲 ({dpi_min}〜{dpi_max}) 外です"
            )));
        }
    }
    if usize::from(edit.default_dpi_index) >= edit.dpi_levels.len() {
        return Err(Error::Other("デフォルトDPI番号が有効なステージ数を超えています".into()));
    }
    let period_ms = match edit.report_rate_hz {
        1000 => 1u8,
        500 => 2,
        250 => 4,
        125 => 8,
        other => {
            return Err(Error::Other(format!(
                "{other} Hz は保存できません (125/250/500/1000 Hz)"
            )))
        }
    };

    let mut data = original.to_vec();
    data[0] = period_ms;
    data[1] = edit.default_dpi_index;
    if usize::from(data[2]) >= edit.dpi_levels.len() {
        data[2] = 0;
    }
    for i in 0..5 {
        let v = edit.dpi_levels.get(i).copied().unwrap_or(0);
        data[3 + i * 2..5 + i * 2].copy_from_slice(&v.to_le_bytes());
    }

    if let Some(btns) = &edit.buttons {
        let count = button_count.min(MAX_BUTTONS).min(btns.len());
        for (i, raw) in btns.iter().take(count).enumerate() {
            let off = BUTTON_TABLE_OFFSET + i * 4;
            data[off..off + 4].copy_from_slice(raw);
        }
    }

    let name_bytes = encode_name(edit.name.as_deref());
    data[NAME_OFFSET..NAME_OFFSET + NAME_LEN].copy_from_slice(&name_bytes);

    if let Some(leds) = &edit.leds {
        if data.len() >= LED_TABLE_OFFSET + LED_RECORD_SIZE * 2 + 2 {
            for (z, eff) in leds.iter().enumerate() {
                let off = LED_TABLE_OFFSET + z * LED_RECORD_SIZE;
                data[off..off + LED_RECORD_SIZE].copy_from_slice(&eff.encode());
            }
        }
    }

    let n = data.len();
    let crc = crc16_ccitt(&data[..n - 2]);
    data[n - 2..n].copy_from_slice(&crc.to_be_bytes());
    Ok(data)
}

fn save_backup(sector: u16, data: &[u8]) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("GProTray").join("backups");
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::Other(format!("バックアップフォルダ作成失敗: {e}")))?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("sector_{sector:04x}_{ts}.bin"));
    std::fs::write(&path, data)
        .map_err(|e| Error::Other(format!("バックアップ書込失敗: {e}")))?;
    Ok(path)
}

fn write_sector_raw(
    t: &Transport,
    dev_idx: u8,
    feat_idx: u8,
    sector: u16,
    data: &[u8],
) -> Result<()> {
    if sector >= ROM_SECTOR_START {
        return Err(Error::Other("ROMセクタへの書き込みは禁止されています".into()));
    }
    let [sh, sl] = sector.to_be_bytes();
    let [lh, ll] = (data.len() as u16).to_be_bytes();
    t.request20(
        dev_idx,
        feat_idx,
        func::WRITE_START,
        &[sh, sl, 0, 0, lh, ll],
        TIMEOUT,
    )?;
    for chunk in data.chunks(16) {
        let mut buf = [0xFFu8; 16];
        buf[..chunk.len()].copy_from_slice(chunk);
        t.request20(dev_idx, feat_idx, func::WRITE_DATA, &buf, TIMEOUT)?;
    }
    t.request20(dev_idx, feat_idx, func::WRITE_END, &[], COMMIT_TIMEOUT)?;
    Ok(())
}

fn write_sector_verified(
    t: &Transport,
    dev_idx: u8,
    feat_idx: u8,
    sector: u16,
    new_data: &[u8],
    original: &[u8],
) -> Result<()> {
    let size = new_data.len() as u16;
    let outcome = write_sector_raw(t, dev_idx, feat_idx, sector, new_data)
        .and_then(|()| read_sector(t, dev_idx, feat_idx, sector, size));
    let reason = match outcome {
        Ok(rb) if rb == new_data => return Ok(()),
        Ok(_) => "読み戻しバイト列が一致しませんでした".to_string(),
        Err(e) => e.to_string(),
    };
    let rolled_back = write_sector_raw(t, dev_idx, feat_idx, sector, original)
        .and_then(|()| read_sector(t, dev_idx, feat_idx, sector, size))
        .is_ok_and(|rb| rb == original);
    if rolled_back {
        Err(Error::Other(format!(
            "セクタ 0x{sector:04X} 書込検証失敗 ({reason})。元の設定を復元しました。"
        )))
    } else {
        Err(Error::Other(format!(
            "セクタ 0x{sector:04X} 書込検証失敗 ({reason})。バックアップを確認してください。"
        )))
    }
}

pub fn set_slot_enabled(
    t: &Transport,
    dev_idx: u8,
    feat_idx: u8,
    slot: u8,
    enabled: bool,
) -> Result<()> {
    let gi = t.request20(dev_idx, feat_idx, func::GET_INFO, &[], TIMEOUT)?;
    let profile_count = gi[3];
    let sector_size = u16::from_be_bytes([gi[7], gi[8]]);
    if slot == 0 || slot > profile_count {
        return Err(Error::Other(format!("スロット {slot} は存在しません")));
    }

    let dir_orig = read_sector(t, dev_idx, feat_idx, DIRECTORY_SECTOR, sector_size)?;
    if !crc_ok(&dir_orig) {
        return Err(Error::Other("ディレクトリのCRCが不正なため変更を中止しました".into()));
    }
    let mut entries = parse_directory(&dir_orig);
    let idx = usize::from(slot - 1);
    if idx >= entries.len() {
        return Err(Error::Other(format!("スロット {slot} がディレクトリに存在しません")));
    }

    if !enabled {
        let active_raw = t.request20(dev_idx, feat_idx, func::GET_ACTIVE_PROFILE, &[], TIMEOUT)?;
        let active_sector = u16::from_be_bytes([active_raw[0], active_raw[1]]);
        if entries[idx].sector == active_sector {
            return Err(Error::Other(
                "現在使用中のプロファイルは無効化できません。別のスロットに切り替えてから無効化してください。"
                    .into(),
            ));
        }
    }

    entries[idx] = with_enabled(entries[idx], enabled);
    let new_dir = build_directory(&entries, usize::from(sector_size))?;
    if new_dir == dir_orig {
        return Ok(());
    }
    save_backup(DIRECTORY_SECTOR, &dir_orig)?;
    write_sector_verified(t, dev_idx, feat_idx, DIRECTORY_SECTOR, &new_dir, &dir_orig)?;
    std::thread::sleep(SETTLE_DELAY);
    Ok(())
}

pub fn apply_profile_edit(
    t: &Transport,
    dev_idx: u8,
    feat_idx: u8,
    led_feat_idx: Option<u8>,
    edit: &ProfileEdit,
    dpi_min: u16,
    dpi_max: u16,
) -> Result<()> {
    let gi = t.request20(dev_idx, feat_idx, func::GET_INFO, &[], TIMEOUT)?;
    let profile_count = gi[3];
    let button_count = usize::from(gi[5]);
    let sector_size = u16::from_be_bytes([gi[7], gi[8]]);
    if edit.slot == 0 || edit.slot > profile_count {
        return Err(Error::Other(format!("スロット {} は存在しません", edit.slot)));
    }

    let dir = read_sector(t, dev_idx, feat_idx, DIRECTORY_SECTOR, sector_size)?;
    if !crc_ok(&dir) {
        return Err(Error::Other("ディレクトリのCRCが不正なため中止しました".into()));
    }
    let entries = parse_directory(&dir);
    let entry = entries
        .get(usize::from(edit.slot - 1))
        .ok_or_else(|| Error::Other(format!("スロット {} が見つかりません", edit.slot)))?;
    let sector = entry.sector;
    if sector == 0 || sector >= ROM_SECTOR_START {
        return Err(Error::Other("ROMセクタは編集できません".into()));
    }

    let original = read_sector(t, dev_idx, feat_idx, sector, sector_size)?;
    let patched = patch_profile_sector_full(&original, edit, dpi_min, dpi_max, button_count)?;
    if patched != original {
        save_backup(sector, &original)?;
        write_sector_verified(t, dev_idx, feat_idx, sector, &patched, &original)?;
    }

    let active_raw = t.request20(dev_idx, feat_idx, func::GET_ACTIVE_PROFILE, &[], TIMEOUT)?;
    let active_sector = u16::from_be_bytes([active_raw[0], active_raw[1]]);
    if active_sector == sector {
        t.request20(
            dev_idx,
            feat_idx,
            func::SET_ACTIVE_PROFILE,
            &sector.to_be_bytes(),
            TIMEOUT,
        )?;
        std::thread::sleep(SETTLE_DELAY);
    }

    if edit.leds.is_some() {
        if let Some(lf) = led_feat_idx {
            let _ = release_led_sw_control(t, dev_idx, lf);
        }
    }
    Ok(())
}

/// Releases `0x8070` ColorLedEffects software control so onboard profile LEDs display immediately.
pub fn release_led_sw_control(t: &Transport, dev_idx: u8, led_feat_idx: u8) -> Result<()> {
    let ctrl = t.request20(dev_idx, led_feat_idx, 7, &[], TIMEOUT)?;
    if ctrl.first().copied().unwrap_or(0) != 0 {
        let sync_events = ctrl.get(1).copied().unwrap_or(0);
        t.request20(dev_idx, led_feat_idx, 8, &[0, sync_events, 0], TIMEOUT)?;
    }
    Ok(())
}

pub fn set_onboard_rgb(
    t: &Transport,
    dev_idx: u8,
    feat_idx: u8,
    led_feat_idx: Option<u8>,
    effect: LedEffect,
) -> Result<()> {
    let gi = t.request20(dev_idx, feat_idx, func::GET_INFO, &[], TIMEOUT)?;
    let sector_size = u16::from_be_bytes([gi[7], gi[8]]);
    let active_raw = t.request20(dev_idx, feat_idx, func::GET_ACTIVE_PROFILE, &[], TIMEOUT)?;
    let active_sector = u16::from_be_bytes([active_raw[0], active_raw[1]]);
    if active_sector == 0 || active_sector >= ROM_SECTOR_START {
        return Err(Error::Other(
            "有効なユーザープロファイルが選択されていないためRGB設定を保存できません".into(),
        ));
    }
    let original = read_sector(t, dev_idx, feat_idx, active_sector, sector_size)?;
    if !crc_ok(&original) {
        return Err(Error::Other("プロファイルのCRCが不正なため中止しました".into()));
    }
    if original.len() < LED_TABLE_OFFSET + LED_RECORD_SIZE * 2 + 2 {
        return Err(Error::Other("このプロファイル形式はオンボードLED変更に非対応です".into()));
    }

    let mut patched = original.clone();
    let encoded = effect.encode();
    for z in 0..2 {
        let off = LED_TABLE_OFFSET + z * LED_RECORD_SIZE;
        patched[off..off + LED_RECORD_SIZE].copy_from_slice(&encoded);
    }
    let n = patched.len();
    let crc = crc16_ccitt(&patched[..n - 2]);
    patched[n - 2..n].copy_from_slice(&crc.to_be_bytes());

    if patched != original {
        save_backup(active_sector, &original)?;
        write_sector_verified(t, dev_idx, feat_idx, active_sector, &patched, &original)?;
    }

    t.request20(
        dev_idx,
        feat_idx,
        func::SET_ACTIVE_PROFILE,
        &active_sector.to_be_bytes(),
        TIMEOUT,
    )?;
    std::thread::sleep(SETTLE_DELAY);

    if let Some(lf) = led_feat_idx {
        let _ = release_led_sw_control(t, dev_idx, lf);
    }
    Ok(())
}

/// Updates byte 0 (report rate period) of the currently active user profile sector,
/// backs up the original sector, writes with CRC & readback verification, and reloads it.
pub fn set_onboard_report_rate(
    t: &Transport,
    dev_idx: u8,
    feat_idx: u8,
    hz: u32,
) -> Result<()> {
    let gi = t.request20(dev_idx, feat_idx, func::GET_INFO, &[], TIMEOUT)?;
    let sector_size = u16::from_be_bytes([gi[7], gi[8]]);
    let active_raw = t.request20(dev_idx, feat_idx, func::GET_ACTIVE_PROFILE, &[], TIMEOUT)?;
    let active_sector = u16::from_be_bytes([active_raw[0], active_raw[1]]);
    if active_sector == 0 || active_sector >= ROM_SECTOR_START {
        return Err(Error::Other(
            "有効なユーザープロファイルが選択されていないためポーリングレートを変更できません".into(),
        ));
    }
    let original = read_sector(t, dev_idx, feat_idx, active_sector, sector_size)?;
    let patched = patch_report_rate_sector(&original, hz)?;
    if patched == original {
        return Ok(());
    }
    save_backup(active_sector, &original)?;
    write_sector_verified(t, dev_idx, feat_idx, active_sector, &patched, &original)?;
    t.request20(
        dev_idx,
        feat_idx,
        func::SET_ACTIVE_PROFILE,
        &active_sector.to_be_bytes(),
        TIMEOUT,
    )?;
    std::thread::sleep(SETTLE_DELAY);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_ccitt_false_standard_vector() {
        assert_eq!(crc16_ccitt(b"123456789"), 0x29B1);
    }

    #[test]
    fn parses_g_pro_wireless_format3_sector() {
        let mut s = vec![0xFFu8; 255];
        s[0] = 1; // 1000 Hz
        s[1] = 1; // default DPI idx
        s[2] = 0; // shift DPI idx
        for (i, dpi) in [400u16, 800, 1600, 3200, 0].iter().enumerate() {
            s[3 + i * 2..5 + i * 2].copy_from_slice(&dpi.to_le_bytes());
        }
        s[0x20..0x24].copy_from_slice(&[0x80, 0x01, 0x00, 0x01]);
        let name_utf16: Vec<u8> = "Default\0"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        s[0xA0..0xA0 + name_utf16.len()].copy_from_slice(&name_utf16);
        let crc = crc16_ccitt(&s[..253]);
        s[253..255].copy_from_slice(&crc.to_be_bytes());

        let parsed = parse_profile_sector(1, 0x0001, false, true, &s, 8);
        assert_eq!(parsed.crc_valid, Some(true));
        assert_eq!(parsed.report_rate_hz, Some(1000));
        assert_eq!(parsed.default_dpi_index, 1);
        assert_eq!(parsed.dpi_levels, vec![400, 800, 1600, 3200]);
        assert_eq!(parsed.name.as_deref(), Some("Default"));
        assert_eq!(parsed.buttons.len(), 8);
        assert_eq!(parsed.buttons[0], [0x80, 0x01, 0x00, 0x01]);
    }

    #[test]
    fn patches_report_rate_preserving_other_bytes_and_crc() {
        let mut s = vec![0xFFu8; 255];
        s[0] = 1; // 1000 Hz
        s[1] = 1;
        let crc = crc16_ccitt(&s[..253]);
        s[253..255].copy_from_slice(&crc.to_be_bytes());

        let patched = patch_report_rate_sector(&s, 500).unwrap();
        assert!(crc_ok(&patched));
        assert_eq!(patched[0], 2);
        assert_eq!(&patched[1..253], &s[1..253]);
        assert!(patch_report_rate_sector(&s, 8000).is_err());
    }

    #[test]
    fn directory_enable_toggle_enforces_at_least_one_enabled() {
        let entries = vec![
            DirEntry { sector: 1, flags: 1, extra: 0x00 },
            DirEntry { sector: 2, flags: 0, extra: 0xFF },
        ];
        let toggled = vec![
            with_enabled(entries[0], true),
            with_enabled(entries[1], true),
        ];
        let dir = build_directory(&toggled, 255).unwrap();
        assert!(crc_ok(&dir));
        let parsed = parse_directory(&dir);
        assert!(parsed[0].enabled());
        assert!(parsed[1].enabled());

        let all_off = vec![
            with_enabled(entries[0], false),
            with_enabled(entries[1], false),
        ];
        assert!(build_directory(&all_off, 255).is_err());
    }

    #[test]
    fn roundtrips_led_effects_and_full_profile_patch() {
        let mut s = vec![0xFFu8; 255];
        s[0] = 1;
        s[1] = 0;
        s[2] = 0;
        s[3..5].copy_from_slice(&800u16.to_le_bytes());
        s[5..13].fill(0);
        let crc = crc16_ccitt(&s[..253]);
        s[253..255].copy_from_slice(&crc.to_be_bytes());

        let edit = ProfileEdit {
            slot: 1,
            name: Some("FPS".into()),
            dpi_levels: vec![400, 800, 1600],
            default_dpi_index: 1,
            report_rate_hz: 500,
            buttons: Some(vec![[0x80, 0x01, 0x00, 0x01]; 8]),
            leds: Some([
                LedEffect::Fixed { r: 0x00, g: 0xD4, b: 0xFF },
                LedEffect::Off,
            ]),
        };
        let patched = patch_profile_sector_full(&s, &edit, 100, 25600, 8).unwrap();
        let parsed = parse_profile_sector(1, 1, false, true, &patched, 8);
        assert_eq!(parsed.crc_valid, Some(true));
        assert_eq!(parsed.name.as_deref(), Some("FPS"));
        assert_eq!(parsed.dpi_levels, vec![400, 800, 1600]);
        assert_eq!(parsed.report_rate_hz, Some(500));
        assert_eq!(parsed.leds.len(), 2);
        assert_eq!(parsed.leds[0], LedEffect::Fixed { r: 0x00, g: 0xD4, b: 0xFF });
        assert_eq!(parsed.leds[1], LedEffect::Off);
    }

    #[test]
    fn includes_standard_keys_like_f5_and_page_up() {
        let presets = button_presets();
        assert!(presets.iter().any(|p| p.raw == [0x80, 0x02, 0x00, 0x3E] && p.label == "キー: F5"));
        assert!(presets.iter().any(|p| p.raw == [0x80, 0x02, 0x00, 0x4B] && p.label == "キー: Page Up"));
        assert_eq!(format_key_combo(0x03, 0x17), "キー: Ctrl+Shift+T");
    }
}
