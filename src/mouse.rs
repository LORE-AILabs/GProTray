//! Read-only HID++ 2.0 device discovery and state reading for Logitech mice.
//!
//! P0 stage performs strictly read-only HID++ queries:
//! - Root (0x0000), FeatureSet (0x0001), DeviceInfo (0x0003), DeviceName (0x0005)
//! - AdjustableDPI (0x2201) / ExtendedAdjustableDPI (0x2202)
//! - ReportRate (0x8060) / ExtendedReportRate (0x8061)
//! - OnboardProfiles (0x8100)
//! - Receiver pairing info (HID++ 1.0 Reg 0x02 / 0xB5) when mouse is asleep.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use crate::hidpp::{
    Error, EventSink, Result, Transport, DIRECT_INDEX, HIDPP_LONG_USAGE, HIDPP_SHORT_USAGE,
    HIDPP_USAGE_PAGE,
};
use crate::onboard::{self, LedEffect, OnboardSnapshot, ProfileEdit};
use crate::win_hid::{enumerate_logitech_hid, HidInterfaceInfo};

pub mod feature {
    pub const ROOT: u16 = 0x0000;
    pub const FEATURE_SET: u16 = 0x0001;
    pub const DEVICE_INFO: u16 = 0x0003;
    pub const DEVICE_NAME: u16 = 0x0005;
    pub const ADJUSTABLE_DPI: u16 = 0x2201;
    pub const EXTENDED_DPI: u16 = 0x2202;
    pub const REPORT_RATE: u16 = 0x8060;
    pub const EXTENDED_REPORT_RATE: u16 = 0x8061;
    pub const COLOR_LED_EFFECTS: u16 = 0x8070;
    pub const ONBOARD_PROFILES: u16 = 0x8100;
}

const RECEIVER_PIDS: &[u16] = &[
    0xC517, 0xC52B, 0xC52F, 0xC531, 0xC532, 0xC534, 0xC539, 0xC53A, 0xC53D, 0xC53F, 0xC541,
    0xC545, 0xC547, 0xC548, 0xC54D,
];

const PROBE_TIMEOUT: Duration = Duration::from_millis(350);
const TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Sleeping,
    Disconnected,
}

#[derive(Debug, Clone)]
pub struct FeatureEntry {
    pub id: u16,
    pub index: u8,
    pub flags: u8,
    pub version: u8,
}

#[derive(Debug, Clone)]
pub struct DpiInfo {
    pub feature_id: u16,
    pub current: u16,
    pub default: Option<u16>,
    pub min: u16,
    pub max: u16,
    pub supported: Vec<u16>,
}

#[derive(Debug, Clone)]
pub struct ReportRateInfo {
    pub feature_id: u16,
    pub current_hz: u32,
    pub supported_hz: Vec<u32>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MouseSnapshot {
    pub status: ConnectionStatus,
    pub transport_path: String,
    pub device_index: u8,
    pub receiver_vid: u16,
    pub receiver_pid: u16,
    pub receiver_product: String,
    pub name: String,
    pub firmware_name: Option<String>,
    pub unit_id: Option<String>,
    pub model_ids: Vec<u16>,
    pub firmware_version: Option<String>,
    pub protocol: Option<(u8, u8)>,
    pub features: BTreeMap<u16, FeatureEntry>,
    pub dpi: Option<DpiInfo>,
    pub report_rate: Option<ReportRateInfo>,
    pub onboard: Option<OnboardSnapshot>,
    pub warnings: Vec<String>,
}

impl Default for MouseSnapshot {
    fn default() -> Self {
        Self {
            status: ConnectionStatus::Disconnected,
            transport_path: String::new(),
            device_index: 0,
            receiver_vid: 0,
            receiver_pid: 0,
            receiver_product: String::new(),
            name: "Logicoolマウス".into(),
            firmware_name: None,
            unit_id: None,
            model_ids: Vec::new(),
            firmware_version: None,
            protocol: None,
            features: BTreeMap::new(),
            dpi: None,
            report_rate: None,
            onboard: None,
            warnings: Vec::new(),
        }
    }
}

pub fn is_receiver(pid: u16, product: &str) -> bool {
    RECEIVER_PIDS.contains(&pid) || product.to_ascii_lowercase().contains("receiver")
}

/// Shared interface key for pairing short (`col01`) and long (`col02`) HID++ collections.
pub fn interface_key(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    let mut key = match lower.find("&col") {
        Some(i) => format!("{}{}", &lower[..i], &lower[(i + 6).min(lower.len())..]),
        None => lower,
    };
    if let Some(i) = key.find("#{") {
        key.truncate(i);
    }
    if let Some(i) = key.rfind('&') {
        key.truncate(i);
    }
    key
}

fn be16(hi: u8, lo: u8) -> u16 {
    u16::from_be_bytes([hi, lo])
}

/// Decodes HID++ DPI list (0x2201/0x2202): big-endian u16 values until 0,
/// where `0b111x_xxxx_xxxx_xxxx` encodes step size up to the next value.
pub fn parse_dpi_list(bytes: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let val = be16(bytes[i], bytes[i + 1]);
        if val == 0 {
            break;
        }
        if (val >> 13) == 0b111 {
            let step = u32::from(val & 0x1FFF);
            if i + 3 >= bytes.len() {
                break;
            }
            let last = u32::from(be16(bytes[i + 2], bytes[i + 3]));
            if let (Some(&prev), true) = (out.last(), step > 0) {
                let mut d = u32::from(prev) + step;
                while d <= last {
                    out.push(d as u16);
                    d += step;
                }
            }
            i += 4;
        } else {
            out.push(val);
            i += 2;
        }
    }
    out
}

pub fn parse_model_ids(r: &[u8]) -> Vec<u16> {
    if r.len() < 13 {
        return Vec::new();
    }
    let transports = r[6];
    let mut ids = Vec::new();
    let mut off = 7;
    for flag in [0x01u8, 0x02, 0x04, 0x08] {
        if (transports & flag) != 0 && off + 1 < 13 {
            let pid = be16(r[off], r[off + 1]);
            if pid != 0 {
                ids.push(pid);
            }
            off += 2;
        }
    }
    ids
}

fn marketing_name(pid: u16) -> Option<&'static str> {
    match pid {
        0x4079 | 0xC088 | 0xAADF => Some("G PRO Wireless"),
        0x4093 | 0xC094 | 0xAB00 => Some("G PRO X SUPERLIGHT"),
        _ => None,
    }
}

pub struct HidCollectionPair {
    pub long: HidInterfaceInfo,
    pub short: Option<HidInterfaceInfo>,
}

pub fn find_hidpp_collections() -> Vec<HidCollectionPair> {
    let all = enumerate_logitech_hid();
    let mut shorts: HashMap<String, HidInterfaceInfo> = HashMap::new();
    let mut longs = Vec::new();

    for iface in all {
        if iface.usage_page != HIDPP_USAGE_PAGE {
            continue;
        }
        if iface.usage == HIDPP_SHORT_USAGE {
            shorts.insert(interface_key(&iface.path), iface);
        } else if iface.usage == HIDPP_LONG_USAGE {
            longs.push(iface);
        }
    }

    longs
        .into_iter()
        .map(|long| {
            let short = shorts.remove(&interface_key(&long.path));
            HidCollectionPair { long, short }
        })
        .collect()
}

pub struct MouseManager {
    transports: HashMap<String, Transport>,
    events: EventSink,
    last_connected: Option<MouseSnapshot>,
    enable_notifications: bool,
}

impl MouseManager {
    pub fn new(events: EventSink) -> Self {
        Self {
            transports: HashMap::new(),
            events,
            last_connected: None,
            enable_notifications: false,
        }
    }

    pub fn new_with_notifications(events: EventSink) -> Self {
        Self {
            transports: HashMap::new(),
            events,
            last_connected: None,
            enable_notifications: true,
        }
    }

    /// Scans Logitech HID++ interfaces and reads the mouse state (read-only).
    pub fn scan_and_read(&mut self) -> MouseSnapshot {
        let pairs = find_hidpp_collections();
        self.transports
            .retain(|path, tr| tr.is_alive() && pairs.iter().any(|p| &p.long.path == path));

        let mut warnings = Vec::new();
        for pair in &pairs {
            if !self.transports.contains_key(&pair.long.path) {
                match Transport::open(&pair.long, pair.short.as_ref(), self.events.clone()) {
                    Ok(t) => {
                        if self.enable_notifications && is_receiver(pair.long.pid, &pair.long.product) {
                            ensure_receiver_notifications(&t);
                        }
                        self.transports.insert(pair.long.path.clone(), t);
                    }
                    Err(e) => warnings.push(format!("HIDオープン失敗 ({}): {e}", pair.long.product)),
                }
            }
        }

        let mut sleeping_fallback: Option<MouseSnapshot> = None;

        for pair in &pairs {
            let Some(tr) = self.transports.get(&pair.long.path) else {
                continue;
            };
            let recv = is_receiver(pair.long.pid, &pair.long.product);
            let indices: Vec<u8> = if recv {
                (1..=6).collect()
            } else {
                vec![DIRECT_INDEX]
            };

            // Read paired slot hints on receiver (HID++ 1.0 Reg 0x02 & 0xB5) before pinging.
            let paired_info = if recv {
                read_receiver_paired_slots(tr)
            } else {
                HashMap::new()
            };

            for idx in indices {
                // Skip empty receiver slots if Reg 0x02/0xB5 already told us which slots exist.
                if recv && !paired_info.is_empty() && !paired_info.contains_key(&idx) {
                    continue;
                }

                match tr.request20(idx, 0x00, 1, &[0x00, 0x00, 0xA5], PROBE_TIMEOUT) {
                    Ok(ping) if ping.len() >= 2 && ping[0] >= 2 => {
                        let mut snap = read_connected_device(
                            tr,
                            &pair.long,
                            idx,
                            (ping[0], ping[1]),
                            paired_info.get(&idx),
                        );
                        snap.warnings.extend(warnings);
                        self.last_connected = Some(snap.clone());
                        return snap;
                    }
                    Ok(_) => {}
                    Err(Error::Timeout | Error::Hidpp10(_)) => {
                        if let Some((wpid, rname)) = paired_info.get(&idx) {
                            if sleeping_fallback.is_none() {
                                let name = marketing_name(*wpid)
                                    .map(str::to_string)
                                    .or_else(|| (!rname.is_empty()).then(|| rname.clone()))
                                    .unwrap_or_else(|| "Logicoolワイヤレスマウス".into());
                                sleeping_fallback = Some(MouseSnapshot {
                                    status: ConnectionStatus::Sleeping,
                                    transport_path: pair.long.path.clone(),
                                    device_index: idx,
                                    receiver_vid: pair.long.vid,
                                    receiver_pid: pair.long.pid,
                                    receiver_product: pair.long.product.clone(),
                                    name,
                                    model_ids: (*wpid != 0).then(|| vec![*wpid]).unwrap_or_default(),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                    Err(e) => warnings.push(format!("プローブエラー (slot {idx}): {e}")),
                }
            }
        }

        self.last_connected = None;
        if let Some(mut snap) = sleeping_fallback {
            snap.warnings.extend(warnings);
            return snap;
        }

        MouseSnapshot {
            warnings,
            ..Default::default()
        }
    }

    fn connected_target(&self) -> Result<(&Transport, MouseSnapshot)> {
        let snap = self
            .last_connected
            .as_ref()
            .filter(|s| s.status == ConnectionStatus::Connected)
            .cloned()
            .ok_or(Error::Disconnected)?;
        let tr = self
            .transports
            .get(&snap.transport_path)
            .filter(|t| t.is_alive())
            .ok_or(Error::Disconnected)?;
        Ok((tr, snap))
    }

    pub fn set_dpi(&mut self, dpi: u16) -> Result<MouseSnapshot> {
        let (tr, mut snap) = self.connected_target()?;
        let info = snap
            .dpi
            .as_ref()
            .ok_or(Error::Unsupported(feature::ADJUSTABLE_DPI))?;
        validate_dpi(info, dpi)?;

        let dev_idx = snap.device_index;
        let [xh, xl] = dpi.to_be_bytes();

        if let Some(f) = snap.features.get(&feature::EXTENDED_DPI) {
            let cur = tr.request20(dev_idx, f.index, 5, &[0], TIMEOUT)?;
            let lod = cur.get(9).copied().unwrap_or(0);
            tr.request20(dev_idx, f.index, 6, &[0, xh, xl, xh, xl, lod], TIMEOUT)?;
            let rb = tr.request20(dev_idx, f.index, 5, &[0], TIMEOUT)?;
            let actual = be16(rb[1], rb[2]);
            if actual != dpi {
                return Err(Error::Other(format!(
                    "DPI読み戻し不一致 (期待: {dpi}, 実際: {actual})"
                )));
            }
        } else if let Some(f) = snap.features.get(&feature::ADJUSTABLE_DPI) {
            tr.request20(dev_idx, f.index, 3, &[0, xh, xl], TIMEOUT)?;
            let rb = tr.request20(dev_idx, f.index, 2, &[0], TIMEOUT)?;
            let actual = be16(rb[1], rb[2]);
            if actual != dpi {
                return Err(Error::Other(format!(
                    "DPI読み戻し不一致 (期待: {dpi}, 実際: {actual})"
                )));
            }
        } else {
            return Err(Error::Unsupported(feature::ADJUSTABLE_DPI));
        }

        snap.warnings.clear();
        snap.dpi = read_dpi(tr, dev_idx, &snap.features, &mut snap.warnings);
        if let Some(f) = snap.features.get(&feature::ONBOARD_PROFILES) {
            if let Ok(ob) = onboard::read_snapshot(tr, dev_idx, f.index) {
                snap.onboard = Some(ob);
            }
        }
        self.last_connected = Some(snap.clone());
        Ok(snap)
    }

    pub fn set_report_rate(&mut self, hz: u32) -> Result<MouseSnapshot> {
        let (tr, mut snap) = self.connected_target()?;
        let info = snap
            .report_rate
            .as_ref()
            .ok_or(Error::Unsupported(feature::REPORT_RATE))?;
        if !info.supported_hz.contains(&hz) {
            return Err(Error::Other(format!(
                "{hz} Hz はこのマウスでサポートされていません"
            )));
        }

        let dev_idx = snap.device_index;
        let prev_dpi = snap.dpi.as_ref().map(|d| d.current);
        let in_onboard_mode = snap
            .onboard
            .as_ref()
            .is_some_and(|ob| !ob.is_host_mode());

        if in_onboard_mode {
            let ob_feat = snap
                .features
                .get(&feature::ONBOARD_PROFILES)
                .map(|f| f.index)
                .ok_or(Error::Unsupported(feature::ONBOARD_PROFILES))?;
            onboard::set_onboard_report_rate(tr, dev_idx, ob_feat, hz)?;
            if let Some(f) = snap.features.get(&feature::REPORT_RATE) {
                let rb = tr.request20(dev_idx, f.index, 1, &[], TIMEOUT)?;
                let actual = if rb[0] > 0 { 1000 / u32::from(rb[0]) } else { 0 };
                if actual != hz {
                    return Err(Error::Other(format!(
                        "ポーリングレート読み戻し不一致 (期待: {hz} Hz, 実際: {actual} Hz)"
                    )));
                }
            }
            if let Some(dpi) = prev_dpi {
                let _ = restore_live_dpi(tr, dev_idx, &snap.features, dpi);
            }
        } else if let Some(f) = snap.features.get(&feature::EXTENDED_REPORT_RATE) {
            const HZ: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];
            let idx = HZ
                .iter()
                .position(|&v| v == hz)
                .ok_or_else(|| Error::Other(format!("未対応のポーリングレート: {hz} Hz")))?
                as u8;
            tr.request20(dev_idx, f.index, 3, &[idx], TIMEOUT)?;
            let rb = tr.request20(dev_idx, f.index, 2, &[], TIMEOUT)?;
            let actual = HZ.get(usize::from(rb[0])).copied().unwrap_or(0);
            if actual != hz {
                return Err(Error::Other(format!(
                    "ポーリングレート読み戻し不一致 (期待: {hz} Hz, 実際: {actual} Hz)"
                )));
            }
        } else if let Some(f) = snap.features.get(&feature::REPORT_RATE) {
            let period_ms = report_rate_period_ms(hz)?;
            tr.request20(dev_idx, f.index, 2, &[period_ms], TIMEOUT)?;
            let rb = tr.request20(dev_idx, f.index, 1, &[], TIMEOUT)?;
            let actual = if rb[0] > 0 { 1000 / u32::from(rb[0]) } else { 0 };
            if actual != hz {
                return Err(Error::Other(format!(
                    "ポーリングレート読み戻し不一致 (期待: {hz} Hz, 実際: {actual} Hz)"
                )));
            }
        } else {
            return Err(Error::Unsupported(feature::REPORT_RATE));
        }

        snap.warnings.clear();
        snap.dpi = read_dpi(tr, dev_idx, &snap.features, &mut snap.warnings);
        snap.report_rate = read_report_rate(tr, dev_idx, &snap.features, &mut snap.warnings);
        if let Some(f) = snap.features.get(&feature::ONBOARD_PROFILES) {
            if let Ok(ob) = onboard::read_snapshot(tr, dev_idx, f.index) {
                snap.onboard = Some(ob);
            }
        }
        self.last_connected = Some(snap.clone());
        Ok(snap)
    }

    pub fn set_active_profile(&mut self, sector: u16) -> Result<MouseSnapshot> {
        let (tr, mut snap) = self.connected_target()?;
        let dev_idx = snap.device_index;
        let feat_idx = snap
            .features
            .get(&feature::ONBOARD_PROFILES)
            .map(|f| f.index)
            .ok_or(Error::Unsupported(feature::ONBOARD_PROFILES))?;

        onboard::set_active_profile(tr, dev_idx, feat_idx, sector)?;

        snap.warnings.clear();
        snap.dpi = read_dpi(tr, dev_idx, &snap.features, &mut snap.warnings);
        snap.report_rate = read_report_rate(tr, dev_idx, &snap.features, &mut snap.warnings);
        snap.onboard = Some(onboard::read_snapshot(tr, dev_idx, feat_idx)?);
        self.last_connected = Some(snap.clone());
        Ok(snap)
    }

    pub fn set_onboard_mode(&mut self, host_mode: bool) -> Result<MouseSnapshot> {
        let (tr, mut snap) = self.connected_target()?;
        let dev_idx = snap.device_index;
        let feat_idx = snap
            .features
            .get(&feature::ONBOARD_PROFILES)
            .map(|f| f.index)
            .ok_or(Error::Unsupported(feature::ONBOARD_PROFILES))?;

        onboard::set_mode(tr, dev_idx, feat_idx, host_mode)?;

        snap.warnings.clear();
        snap.dpi = read_dpi(tr, dev_idx, &snap.features, &mut snap.warnings);
        snap.report_rate = read_report_rate(tr, dev_idx, &snap.features, &mut snap.warnings);
        snap.onboard = Some(onboard::read_snapshot(tr, dev_idx, feat_idx)?);
        self.last_connected = Some(snap.clone());
        Ok(snap)
    }

    pub fn set_slot_enabled(&mut self, slot: u8, enabled: bool) -> Result<MouseSnapshot> {
        let (tr, mut snap) = self.connected_target()?;
        let dev_idx = snap.device_index;
        let feat_idx = snap
            .features
            .get(&feature::ONBOARD_PROFILES)
            .map(|f| f.index)
            .ok_or(Error::Unsupported(feature::ONBOARD_PROFILES))?;

        onboard::set_slot_enabled(tr, dev_idx, feat_idx, slot, enabled)?;

        snap.warnings.clear();
        snap.onboard = Some(onboard::read_snapshot(tr, dev_idx, feat_idx)?);
        self.last_connected = Some(snap.clone());
        Ok(snap)
    }

    pub fn save_profile_edit(&mut self, edit: ProfileEdit) -> Result<MouseSnapshot> {
        let (tr, mut snap) = self.connected_target()?;
        let dev_idx = snap.device_index;
        let feat_idx = snap
            .features
            .get(&feature::ONBOARD_PROFILES)
            .map(|f| f.index)
            .ok_or(Error::Unsupported(feature::ONBOARD_PROFILES))?;
        let led_feat_idx = snap
            .features
            .get(&feature::COLOR_LED_EFFECTS)
            .map(|f| f.index);
        let (dpi_min, dpi_max) = snap
            .dpi
            .as_ref()
            .map(|d| (d.min, d.max))
            .unwrap_or((100, 25600));

        onboard::apply_profile_edit(tr, dev_idx, feat_idx, led_feat_idx, &edit, dpi_min, dpi_max)?;

        snap.warnings.clear();
        snap.dpi = read_dpi(tr, dev_idx, &snap.features, &mut snap.warnings);
        snap.report_rate = read_report_rate(tr, dev_idx, &snap.features, &mut snap.warnings);
        snap.onboard = Some(onboard::read_snapshot(tr, dev_idx, feat_idx)?);
        self.last_connected = Some(snap.clone());
        Ok(snap)
    }

    pub fn set_simple_rgb(&mut self, effect: LedEffect) -> Result<MouseSnapshot> {
        let (tr, mut snap) = self.connected_target()?;
        let dev_idx = snap.device_index;
        let prev_dpi = snap.dpi.as_ref().map(|d| d.current);
        let feat_idx = snap
            .features
            .get(&feature::ONBOARD_PROFILES)
            .map(|f| f.index)
            .ok_or(Error::Unsupported(feature::ONBOARD_PROFILES))?;
        let led_feat_idx = snap
            .features
            .get(&feature::COLOR_LED_EFFECTS)
            .map(|f| f.index);

        onboard::set_onboard_rgb(tr, dev_idx, feat_idx, led_feat_idx, effect)?;
        if let Some(dpi) = prev_dpi {
            let _ = restore_live_dpi(tr, dev_idx, &snap.features, dpi);
        }

        snap.warnings.clear();
        snap.dpi = read_dpi(tr, dev_idx, &snap.features, &mut snap.warnings);
        snap.onboard = Some(onboard::read_snapshot(tr, dev_idx, feat_idx)?);
        self.last_connected = Some(snap.clone());
        Ok(snap)
    }

    pub fn clear_transports(&mut self) {
        self.transports.clear();
        self.last_connected = None;
    }
}

fn ensure_receiver_notifications(tr: &Transport) {
    const NOTIFY_WIRELESS: u8 = 0x01;
    const NOTIFY_SOFTWARE_PRESENT: u8 = 0x08;
    if let Ok(r) = tr.register10(0x81, 0x00, [0, 0, 0], PROBE_TIMEOUT) {
        let wanted = NOTIFY_WIRELESS | NOTIFY_SOFTWARE_PRESENT;
        if r.len() >= 3 && (r[1] & wanted) != wanted {
            let _ = tr.register10(0x80, 0x00, [r[0], r[1] | wanted, r[2]], PROBE_TIMEOUT);
        }
    }
}

fn restore_live_dpi(
    tr: &Transport,
    dev_idx: u8,
    features: &BTreeMap<u16, FeatureEntry>,
    dpi: u16,
) -> Result<()> {
    let [xh, xl] = dpi.to_be_bytes();
    if let Some(f) = features.get(&feature::EXTENDED_DPI) {
        let cur = tr.request20(dev_idx, f.index, 5, &[0], TIMEOUT)?;
        let lod = cur.get(9).copied().unwrap_or(0);
        tr.request20(dev_idx, f.index, 6, &[0, xh, xl, xh, xl, lod], TIMEOUT)?;
    } else if let Some(f) = features.get(&feature::ADJUSTABLE_DPI) {
        tr.request20(dev_idx, f.index, 3, &[0, xh, xl], TIMEOUT)?;
    }
    Ok(())
}

pub fn validate_dpi(info: &DpiInfo, dpi: u16) -> Result<()> {
    if dpi < info.min || dpi > info.max {
        return Err(Error::Other(format!(
            "DPI {dpi} は対応範囲 ({}〜{}) 外です",
            info.min, info.max
        )));
    }
    if !info.supported.is_empty() && !info.supported.contains(&dpi) {
        return Err(Error::Other(format!(
            "DPI {dpi} はセンサーの対応ステップに一致しません"
        )));
    }
    Ok(())
}

pub fn report_rate_period_ms(hz: u32) -> Result<u8> {
    if hz == 0 || 1000 % hz != 0 {
        return Err(Error::Other(format!("不正なポーリングレート: {hz} Hz")));
    }
    let ms = 1000 / hz;
    u8::try_from(ms).map_err(|_| Error::Other(format!("範囲外のポーリングレート: {hz} Hz")))
}

/// Reads paired device WPID & name from HID++ 1.0 receiver registers `0x02` and `0xB5` (read-only).
fn read_receiver_paired_slots(tr: &Transport) -> HashMap<u8, (u16, String)> {
    let mut out = HashMap::new();
    let count = match tr.register10(0x81, 0x02, [0, 0, 0], PROBE_TIMEOUT) {
        Ok(r) if r.len() >= 2 => r[1].min(6),
        _ => return out,
    };
    for slot in 0..count {
        let dev_idx = slot + 1;
        let wpid = match tr.register10(0x83, 0xB5, [0x20 + slot, 0, 0], PROBE_TIMEOUT) {
            Ok(r) if r.len() >= 5 => be16(r[3], r[4]),
            _ => 0,
        };
        let name = match tr.register10(0x83, 0xB5, [0x40 + slot, 0, 0], PROBE_TIMEOUT) {
            Ok(r) if r.len() >= 2 => {
                let nlen = usize::from(r[1]).min(r.len().saturating_sub(2));
                String::from_utf8_lossy(&r[2..2 + nlen]).trim().to_string()
            }
            _ => String::new(),
        };
        out.insert(dev_idx, (wpid, name));
    }
    out
}

fn read_connected_device(
    tr: &Transport,
    iface: &HidInterfaceInfo,
    idx: u8,
    protocol: (u8, u8),
    paired_hint: Option<&(u16, String)>,
) -> MouseSnapshot {
    let mut snap = MouseSnapshot {
        status: ConnectionStatus::Connected,
        transport_path: iface.path.clone(),
        device_index: idx,
        receiver_vid: iface.vid,
        receiver_pid: iface.pid,
        receiver_product: iface.product.clone(),
        name: iface.product.clone(),
        protocol: Some(protocol),
        ..Default::default()
    };

    // 0x0001 FeatureSet
    if let Ok(r) = tr.request20(idx, 0x00, 0, &feature::FEATURE_SET.to_be_bytes(), TIMEOUT) {
        let fset_idx = r[0];
        if fset_idx != 0 {
            snap.features.insert(
                feature::ROOT,
                FeatureEntry {
                    id: feature::ROOT,
                    index: 0,
                    flags: 0,
                    version: 0,
                },
            );
            if let Ok(cnt) = tr.request20(idx, fset_idx, 0, &[], TIMEOUT) {
                for i in 1..=cnt[0] {
                    if let Ok(fr) = tr.request20(idx, fset_idx, 1, &[i], TIMEOUT) {
                        let fid = be16(fr[0], fr[1]);
                        snap.features.insert(
                            fid,
                            FeatureEntry {
                                id: fid,
                                index: i,
                                flags: fr[2],
                                version: fr[3],
                            },
                        );
                    }
                }
            }
        }
    }

    // 0x0005 DeviceName
    if let Some(f) = snap.features.get(&feature::DEVICE_NAME) {
        if let Ok(r0) = tr.request20(idx, f.index, 0, &[], TIMEOUT) {
            let len = usize::from(r0[0]);
            let mut bytes = Vec::with_capacity(len);
            while bytes.len() < len {
                match tr.request20(idx, f.index, 1, &[bytes.len() as u8], TIMEOUT) {
                    Ok(chunk) => {
                        let take = (len - bytes.len()).min(chunk.len());
                        if take == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&chunk[..take]);
                    }
                    Err(_) => break,
                }
            }
            let fw_name = String::from_utf8_lossy(&bytes)
                .trim_end_matches('\0')
                .trim()
                .to_string();
            if !fw_name.is_empty() {
                snap.firmware_name = Some(fw_name.clone());
                snap.name = fw_name;
            }
        }
    }

    // 0x0003 DeviceInfo
    if let Some(f) = snap.features.get(&feature::DEVICE_INFO) {
        if let Ok(r0) = tr.request20(idx, f.index, 0, &[], TIMEOUT) {
            let entity_count = r0[0];
            snap.unit_id = Some(format!(
                "{:02x}{:02x}{:02x}{:02x}",
                r0[1], r0[2], r0[3], r0[4]
            ));
            snap.model_ids = parse_model_ids(&r0);
            for ei in 0..entity_count {
                if let Ok(rf) = tr.request20(idx, f.index, 1, &[ei], TIMEOUT) {
                    // Type 0 = main application firmware (e.g. MPM)
                    if rf[0] == 0 {
                        let prefix = String::from_utf8_lossy(&rf[1..4])
                            .trim_end_matches('\0')
                            .to_string();
                        snap.firmware_version = Some(format!(
                            "{} {:02x}.{:02x}.{:02x}{:02x} (build {:02x}{:02x})",
                            prefix, rf[4], rf[5], rf[6], rf[7], rf[8], rf[9]
                        ));
                        break;
                    }
                }
            }
        }
    }

    let mut pid_candidates = snap.model_ids.clone();
    if let Some((wpid, _)) = paired_hint {
        if *wpid != 0 {
            pid_candidates.push(*wpid);
        }
    }
    if idx == DIRECT_INDEX {
        pid_candidates.push(iface.pid);
    }
    if let Some(mkt) = pid_candidates.into_iter().find_map(marketing_name) {
        snap.name = mkt.to_string();
    }

    // DPI: 0x2202 or 0x2201
    snap.dpi = read_dpi(tr, idx, &snap.features, &mut snap.warnings);

    // Report rate: 0x8061 or 0x8060
    snap.report_rate = read_report_rate(tr, idx, &snap.features, &mut snap.warnings);

    // Onboard profiles: 0x8100 (read-only)
    if let Some(f) = snap.features.get(&feature::ONBOARD_PROFILES) {
        match onboard::read_snapshot(tr, idx, f.index) {
            Ok(ob) => snap.onboard = Some(ob),
            Err(e) => snap.warnings.push(format!("オンボード情報取得エラー: {e}")),
        }
    }

    snap
}

fn read_dpi(
    tr: &Transport,
    idx: u8,
    features: &BTreeMap<u16, FeatureEntry>,
    warnings: &mut Vec<String>,
) -> Option<DpiInfo> {
    if let Some(f) = features.get(&feature::EXTENDED_DPI) {
        let mut raw = Vec::new();
        for page in 0..32u8 {
            match tr.request20(idx, f.index, 2, &[0, 0, page], TIMEOUT) {
                Ok(r) => {
                    raw.extend_from_slice(&r[3..]);
                    if raw.ends_with(&[0, 0]) {
                        break;
                    }
                }
                Err(e) => {
                    warnings.push(format!("拡張DPI一覧取得エラー: {e}"));
                    break;
                }
            }
        }
        let supported = parse_dpi_list(&raw);
        match tr.request20(idx, f.index, 5, &[0], TIMEOUT) {
            Ok(p) => {
                let current = be16(p[1], p[2]);
                let default = Some(be16(p[3], p[4])).filter(|&d| d > 0);
                let min = supported.iter().copied().min().unwrap_or(current);
                let max = supported.iter().copied().max().unwrap_or(current);
                return Some(DpiInfo {
                    feature_id: feature::EXTENDED_DPI,
                    current,
                    default,
                    min,
                    max,
                    supported,
                });
            }
            Err(e) => warnings.push(format!("拡張DPI取得エラー: {e}")),
        }
    }

    if let Some(f) = features.get(&feature::ADJUSTABLE_DPI) {
        let supported = match tr.request20(idx, f.index, 1, &[0], TIMEOUT) {
            Ok(list) => parse_dpi_list(&list[1..]),
            Err(e) => {
                warnings.push(format!("DPI対応範囲取得エラー: {e}"));
                Vec::new()
            }
        };
        match tr.request20(idx, f.index, 2, &[0], TIMEOUT) {
            Ok(p) => {
                let current = be16(p[1], p[2]);
                let default = Some(be16(p[3], p[4])).filter(|&d| d > 0);
                let min = supported.iter().copied().min().unwrap_or(current);
                let max = supported.iter().copied().max().unwrap_or(current);
                return Some(DpiInfo {
                    feature_id: feature::ADJUSTABLE_DPI,
                    current,
                    default,
                    min,
                    max,
                    supported,
                });
            }
            Err(e) => warnings.push(format!("現在DPI取得エラー: {e}")),
        }
    }

    None
}

fn read_report_rate(
    tr: &Transport,
    idx: u8,
    features: &BTreeMap<u16, FeatureEntry>,
    warnings: &mut Vec<String>,
) -> Option<ReportRateInfo> {
    if let Some(f) = features.get(&feature::EXTENDED_REPORT_RATE) {
        const HZ: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];
        let supported_hz = match tr.request20(idx, f.index, 1, &[], TIMEOUT) {
            Ok(r) => {
                let mask = be16(r[0], r[1]);
                (0..HZ.len())
                    .filter(|&i| (mask >> i) & 1 == 1)
                    .map(|i| HZ[i])
                    .collect()
            }
            Err(e) => {
                warnings.push(format!("拡張ポーリングレート一覧エラー: {e}"));
                Vec::new()
            }
        };
        match tr.request20(idx, f.index, 2, &[], TIMEOUT) {
            Ok(r) => {
                let cur_idx = usize::from(r[0]);
                let current_hz = HZ.get(cur_idx).copied().unwrap_or(0);
                return Some(ReportRateInfo {
                    feature_id: feature::EXTENDED_REPORT_RATE,
                    current_hz,
                    supported_hz,
                });
            }
            Err(e) => warnings.push(format!("拡張ポーリングレート取得エラー: {e}")),
        }
    }

    if let Some(f) = features.get(&feature::REPORT_RATE) {
        let supported_hz = match tr.request20(idx, f.index, 0, &[], TIMEOUT) {
            Ok(r) => {
                let mask = r[0];
                (0..8u32)
                    .filter(|i| (mask >> i) & 1 == 1)
                    .map(|i| 1000 / (i + 1))
                    .collect()
            }
            Err(e) => {
                warnings.push(format!("ポーリングレート一覧エラー: {e}"));
                Vec::new()
            }
        };
        match tr.request20(idx, f.index, 1, &[], TIMEOUT) {
            Ok(r) => {
                let ms = u32::from(r[0]);
                let current_hz = if ms > 0 { 1000 / ms } else { 0 };
                return Some(ReportRateInfo {
                    feature_id: feature::REPORT_RATE,
                    current_hz,
                    supported_hz,
                });
            }
            Err(e) => warnings.push(format!("ポーリングレート取得エラー: {e}")),
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dpi_discrete_and_step_ranges() {
        // 100, step 50 up to 250, then 400
        let raw = [0x00, 0x64, 0xE0, 0x32, 0x00, 0xFA, 0x01, 0x90, 0x00, 0x00];
        assert_eq!(parse_dpi_list(&raw), vec![100, 150, 200, 250, 400]);
    }

    #[test]
    fn parses_g_pro_wireless_model_ids() {
        let r = [
            0x04, 0xA0, 0x68, 0x91, 0x6D, 0x00, 0x0C, 0x40, 0x79, 0xC0, 0x88, 0x00, 0x00,
        ];
        assert_eq!(parse_model_ids(&r), vec![0x4079, 0xC088]);
    }

    #[test]
    fn pairs_short_and_long_interface_keys() {
        let short = r"\\?\hid#vid_046d&pid_c539&mi_02&col01#9&844effa&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        let long = r"\\?\hid#vid_046d&pid_c539&mi_02&col02#9&844effa&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(interface_key(short), interface_key(long));
    }

    #[test]
    fn validates_dpi_and_report_rate_bounds() {
        let dpi_info = DpiInfo {
            feature_id: feature::ADJUSTABLE_DPI,
            current: 800,
            default: Some(800),
            min: 100,
            max: 25600,
            supported: vec![100, 400, 800, 1600, 3200],
        };
        assert!(validate_dpi(&dpi_info, 1600).is_ok());
        assert!(validate_dpi(&dpi_info, 50).is_err());
        assert!(validate_dpi(&dpi_info, 825).is_err());

        assert_eq!(report_rate_period_ms(1000).unwrap(), 1);
        assert_eq!(report_rate_period_ms(500).unwrap(), 2);
        assert_eq!(report_rate_period_ms(250).unwrap(), 4);
        assert_eq!(report_rate_period_ms(125).unwrap(), 8);
        assert!(report_rate_period_ms(0).is_err());
        assert!(report_rate_period_ms(333).is_err());
    }
}
