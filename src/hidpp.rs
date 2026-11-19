//! HID++ 1.0 / 2.0 transport over Windows HID collections (`0xFF00`).
//!
//! Uses a distinct software ID (`SW_ID = 0x0E`) so replies to WirelessBattery (`0x0D`)
//! or other tools are ignored without interference, and unsolicited notifications
//! (`SW_ID = 0` or HID++ 1.0 sub-ID < `0x80`) are forwarded to the event loop.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::CreateEventW;

use crate::win_hid::{HidHandle, HidInterfaceInfo, WinEvent};

pub const HIDPP_USAGE_PAGE: u16 = 0xFF00;
pub const HIDPP_SHORT_USAGE: u16 = 0x0001;
pub const HIDPP_LONG_USAGE: u16 = 0x0002;

pub const DIRECT_INDEX: u8 = 0xFF;

pub const REPORT_SHORT: u8 = 0x10;
pub const REPORT_LONG: u8 = 0x11;
pub const SHORT_LEN: usize = 7;
pub const LONG_LEN: usize = 20;
pub const PAYLOAD_LEN: usize = LONG_LEN - 4;

/// Distinct from WirelessBattery (0x0D), G-Hubick (0x0B), omm-rust (0x0A), LogiPro (0x01).
pub const SW_ID: u8 = 0x0E;

#[derive(Debug)]
#[allow(dead_code)]
pub enum Error {
    Io(io::Error),
    Timeout,
    Disconnected,
    Hidpp20(u8, &'static str),
    Hidpp10(u8),
    Unsupported(u16),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "HID I/Oエラー: {e}"),
            Error::Timeout => write!(f, "デバイスからの応答がタイムアウトしました"),
            Error::Disconnected => write!(f, "デバイスが切断されました"),
            Error::Hidpp20(code, name) => write!(f, "HID++ 2.0エラー: {name} ({code:#04x})"),
            Error::Hidpp10(code) => write!(f, "HID++ 1.0エラー ({code:#04x})"),
            Error::Unsupported(feat) => write!(f, "未対応の機能 (Feature {feat:#06x})"),
            Error::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn hidpp20_error_name(code: u8) -> &'static str {
    match code {
        0x01 => "unknown error",
        0x02 => "invalid argument",
        0x03 => "value out of range",
        0x04 => "hardware error",
        0x05 => "internal error",
        0x06 => "invalid feature index",
        0x07 => "invalid function",
        0x08 => "device busy",
        0x09 => "unsupported",
        _ => "unrecognized error",
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum HidppEvent {
    Notification { path: String, report: Vec<u8> },
    TransportGone { path: String },
}

pub type EventSink = Arc<dyn Fn(HidppEvent) + Send + Sync>;

#[derive(Default)]
struct ReplySlot {
    key: Option<[u8; 3]>,
    reply: Option<Result<Vec<u8>>>,
}

struct SharedState {
    slot: Mutex<ReplySlot>,
    cv: Condvar,
    alive: AtomicBool,
}

impl SharedState {
    fn deliver(&self, r: &[u8]) -> bool {
        let Ok(mut slot) = self.slot.lock() else {
            return false;
        };
        let Some([dev, b2, b3]) = slot.key else {
            return false;
        };
        if r.len() < 6 || (r[0] != REPORT_SHORT && r[0] != REPORT_LONG) || r[1] != dev {
            return false;
        }
        let outcome = if r[2] == b2 && r[3] == b3 {
            let mut payload = r[4..].to_vec();
            payload.resize(PAYLOAD_LEN, 0);
            Ok(payload)
        } else if r[2] == 0xFF && r[3] == b2 && r[4] == b3 {
            Err(Error::Hidpp20(r[5], hidpp20_error_name(r[5])))
        } else if r[2] == 0x8F && r[3] == b2 && r[4] == b3 {
            Err(Error::Hidpp10(r[5]))
        } else {
            return false;
        };
        slot.key = None;
        slot.reply = Some(outcome);
        self.cv.notify_all();
        true
    }

    fn mark_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Ok(mut slot) = self.slot.lock() {
            if slot.key.take().is_some() {
                slot.reply = Some(Err(Error::Disconnected));
            }
        }
        self.cv.notify_all();
    }
}

/// Unsolicited notifications: HID++ 1.0 sub-ID < 0x80 (short), or HID++ 2.0 event (`sw_id == 0`, long).
pub fn is_unsolicited_notification(r: &[u8]) -> bool {
    if r.len() < 4 || r[2] >= 0x80 {
        return false;
    }
    match r[0] {
        REPORT_SHORT => true,
        REPORT_LONG => (r[3] & 0x0F) == 0,
        _ => false,
    }
}

fn spawn_reader(
    reader: HidHandle,
    in_len: usize,
    transport_path: String,
    shared: Arc<SharedState>,
    stop_event: Arc<WinEvent>,
    events: EventSink,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("gpro-hid-rx".into())
        .spawn(move || {
            let io_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if io_event.is_null() {
                shared.mark_dead();
                return;
            }
            while shared.alive.load(Ordering::SeqCst) {
                match reader.read_report_cancellable(in_len, io_event, stop_event.raw()) {
                    Ok(Some(report)) => {
                        if !shared.deliver(&report) && is_unsolicited_notification(&report) {
                            events(HidppEvent::Notification {
                                path: transport_path.clone(),
                                report,
                            });
                        }
                    }
                    Ok(None) => break, // Stop requested
                    Err(_) => {
                        if shared.alive.swap(false, Ordering::SeqCst) {
                            shared.mark_dead();
                            events(HidppEvent::TransportGone {
                                path: transport_path,
                            });
                        }
                        break;
                    }
                }
            }
            unsafe { CloseHandle(io_event) };
        })
}

pub struct Transport {
    long_tx: HidHandle,
    long_out_len: usize,
    short_tx: Option<HidHandle>,
    short_out_len: usize,
    req_lock: Mutex<()>,
    shared: Arc<SharedState>,
    stop_event: Arc<WinEvent>,
    threads: Vec<JoinHandle<()>>,
}

impl Transport {
    pub fn open(
        long_info: &HidInterfaceInfo,
        short_info: Option<&HidInterfaceInfo>,
        events: EventSink,
    ) -> Result<Self> {
        let stop_event = Arc::new(WinEvent::new_manual_reset()?);
        let shared = Arc::new(SharedState {
            slot: Mutex::new(ReplySlot::default()),
            cv: Condvar::new(),
            alive: AtomicBool::new(true),
        });

        let long_tx = HidHandle::open_overlapped(&long_info.path_utf16)?;
        let long_rx = HidHandle::open_overlapped(&long_info.path_utf16)?;
        let mut threads = Vec::new();
        threads.push(spawn_reader(
            long_rx,
            (long_info.in_len as usize).max(LONG_LEN),
            long_info.path.clone(),
            shared.clone(),
            stop_event.clone(),
            events.clone(),
        )?);

        let (short_tx, short_out_len) = match short_info {
            Some(si) => match (
                HidHandle::open_overlapped(&si.path_utf16),
                HidHandle::open_overlapped(&si.path_utf16),
            ) {
                (Ok(tx), Ok(rx)) => {
                    if let Ok(jh) = spawn_reader(
                        rx,
                        (si.in_len as usize).max(SHORT_LEN),
                        long_info.path.clone(),
                        shared.clone(),
                        stop_event.clone(),
                        events,
                    ) {
                        threads.push(jh);
                        (Some(tx), (si.out_len as usize).max(SHORT_LEN))
                    } else {
                        (None, SHORT_LEN)
                    }
                }
                _ => (None, SHORT_LEN),
            },
            None => (None, SHORT_LEN),
        };

        Ok(Self {
            long_tx,
            long_out_len: (long_info.out_len as usize).max(LONG_LEN),
            short_tx,
            short_out_len,
            req_lock: Mutex::new(()),
            shared,
            stop_event,
            threads,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    fn exchange(
        &self,
        writer: &HidHandle,
        out_len: usize,
        key: [u8; 3],
        packet: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let _serialize = self
            .req_lock
            .lock()
            .map_err(|_| Error::Other("HID送信ロックエラー".into()))?;
        if !self.is_alive() {
            return Err(Error::Disconnected);
        }
        {
            let mut slot = self
                .shared
                .slot
                .lock()
                .map_err(|_| Error::Other("HID受信ロックエラー".into()))?;
            slot.key = Some(key);
            slot.reply = None;
        }
        if let Err(e) = writer.write_report(packet, out_len, 500) {
            if let Ok(mut slot) = self.shared.slot.lock() {
                slot.key = None;
            }
            return Err(e.into());
        }

        let deadline = Instant::now() + timeout;
        let mut slot = self
            .shared
            .slot
            .lock()
            .map_err(|_| Error::Other("HID受信ロックエラー".into()))?;
        loop {
            if let Some(reply) = slot.reply.take() {
                return reply;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                slot.key = None;
                return Err(Error::Timeout);
            }
            let (next, _) = self
                .shared
                .cv
                .wait_timeout(slot, left)
                .map_err(|_| Error::Other("HID受信待機エラー".into()))?;
            slot = next;
        }
    }

    /// Sends a HID++ 2.0 long report request and returns the 16-byte payload.
    /// Automatically retries once after 100 ms if the device replies with `0x08` (device busy).
    pub fn request20(
        &self,
        device_index: u8,
        feature_index: u8,
        function: u8,
        params: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let mut pkt = [0u8; LONG_LEN];
        let fn_sw = ((function & 0x0F) << 4) | SW_ID;
        pkt[0] = REPORT_LONG;
        pkt[1] = device_index;
        pkt[2] = feature_index;
        pkt[3] = fn_sw;
        let n = params.len().min(PAYLOAD_LEN);
        pkt[4..4 + n].copy_from_slice(&params[..n]);
        match self.exchange(
            &self.long_tx,
            self.long_out_len,
            [device_index, feature_index, fn_sw],
            &pkt,
            timeout,
        ) {
            Err(Error::Hidpp20(0x08, _)) => {
                std::thread::sleep(Duration::from_millis(100));
                self.exchange(
                    &self.long_tx,
                    self.long_out_len,
                    [device_index, feature_index, fn_sw],
                    &pkt,
                    timeout,
                )
            }
            other => other,
        }
    }

    /// Sends a HID++ 1.0 receiver register request (e.g. `0x81` get short, `0x83` get long).
    /// Replies arrive on either the short or long collection depending on the register.
    pub fn register10(
        &self,
        sub_id: u8,
        address: u8,
        params: [u8; 3],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let short = self
            .short_tx
            .as_ref()
            .ok_or_else(|| Error::Other("Short HID++コレクションが見つかりません".into()))?;
        let pkt = [
            REPORT_SHORT,
            DIRECT_INDEX,
            sub_id,
            address,
            params[0],
            params[1],
            params[2],
        ];
        self.exchange(
            short,
            self.short_out_len,
            [DIRECT_INDEX, sub_id, address],
            &pkt,
            timeout,
        )
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::SeqCst);
        self.stop_event.signal();
        for jh in self.threads.drain(..) {
            let _ = jh.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting_shared(key: [u8; 3]) -> SharedState {
        let s = SharedState {
            slot: Mutex::new(ReplySlot::default()),
            cv: Condvar::new(),
            alive: AtomicBool::new(true),
        };
        s.slot.lock().unwrap().key = Some(key);
        s
    }

    #[test]
    fn matches_expected_reply_and_ignores_other_sw_id() {
        let s = waiting_shared([0x01, 0x0C, 0x2E]);
        // WirelessBattery reply (SW_ID = 0x0D) must not match SW_ID = 0x0E
        assert!(!s.deliver(&[0x11, 0x01, 0x0C, 0x2D, 0, 0, 0]));
        assert!(s.deliver(&[0x11, 0x01, 0x0C, 0x2E, 0x06, 0x40, 0x00]));
        let reply = s.slot.lock().unwrap().reply.take().unwrap().unwrap();
        assert_eq!(&reply[..3], &[0x06, 0x40, 0x00]);
    }

    #[test]
    fn matches_hidpp10_and_hidpp20_errors() {
        let s = waiting_shared([0x02, 0x00, 0x1E]);
        assert!(s.deliver(&[0x10, 0x02, 0x8F, 0x00, 0x1E, 0x08, 0x00]));
        assert!(matches!(
            s.slot.lock().unwrap().reply.take(),
            Some(Err(Error::Hidpp10(0x08)))
        ));

        let s2 = waiting_shared([0x01, 0x09, 0x0E]);
        assert!(s2.deliver(&[0x11, 0x01, 0xFF, 0x09, 0x0E, 0x07, 0x00]));
        assert!(matches!(
            s2.slot.lock().unwrap().reply.take(),
            Some(Err(Error::Hidpp20(0x07, _)))
        ));
    }

    #[test]
    fn classifies_unsolicited_notifications() {
        assert!(is_unsolicited_notification(&[0x10, 0x01, 0x41, 0x04, 0, 0, 0]));
        assert!(is_unsolicited_notification(&[0x11, 0x01, 0x0C, 0x00, 0x03, 0x20]));
        assert!(!is_unsolicited_notification(&[0x11, 0x01, 0x06, 0x0D, 0, 0]));
        assert!(!is_unsolicited_notification(&[0x10, 0xFF, 0x81, 0x02, 0, 1, 0]));
    }
}
