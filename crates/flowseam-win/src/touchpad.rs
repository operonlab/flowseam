//! This PC's Precision Touchpad: its 3 and 4 finger swipes and its pinch, as Flowseam gestures.
//!
//! Two-finger scrolling already reaches the mouse hook as wheel events. The other gestures never do: Windows
//! handles them itself. Raw Input, registered for the touchpad's usage with `RIDEV_INPUTSINK` on the window
//! the capture thread owns, delivers the contacts to that window in `WM_INPUT` wherever the focus is. Each
//! report is read through the HID parser into contacts (`parse_report`), assembled into frames
//! (`Assembler`), and recognized (`flowseam_core::touch`); a recognized gesture goes to the attached
//! session as `CapturedEvent::Gesture`, which it forwards while it drives a peer.
//!
//! Raw Input only listens: Windows still acts on the gesture on this PC. It has no documented way for a
//! background process to stop that, so the user sets 3 and 4 finger gestures to "Nothing" in Settings, and
//! `start` says so in the log when the registry shows they are not (`handling`).
//!
//! Like the wheel, a gesture here takes control back to this PC while a peer is driving this one: it is this
//! PC's own input, and the forwarder treats it so.
//!
//! The parsing and the registry verdict are pure and tested here; the Win32 calls are `#[cfg(windows)]`,
//! compiled only on Windows and never run against a real touchpad by the tests.

use std::collections::HashMap;

use flowseam_core::proto::Gesture;
use flowseam_core::touch::{Contact, Extents, TouchReader};

pub const PAGE_GENERIC: u16 = 0x01;
pub const USAGE_X: u16 = 0x30;
pub const USAGE_Y: u16 = 0x31;
/// The digitizer page and the usages the Precision Touchpad protocol uses on it.
pub const PAGE_DIGITIZER: u16 = 0x0D;
pub const USAGE_TOUCHPAD: u16 = 0x05;
pub const USAGE_FINGER: u16 = 0x22;
pub const USAGE_TIP: u16 = 0x42;
pub const USAGE_CONFIDENCE: u16 = 0x47;
pub const USAGE_CONTACT_ID: u16 = 0x51;
pub const USAGE_CONTACT_COUNT: u16 = 0x54;

/// A value capability of an input report, as the HID parser describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueCap {
    pub usage_page: u16,
    pub usage_min: u16,
    pub usage_max: u16,
    pub link: u16,
    pub report_id: u8,
    pub logical_min: i32,
    pub logical_max: i32,
    /// Bits of the field. `HidP_GetUsageValue` hands it back zero-extended to this width.
    pub bit_size: u16,
    pub is_alias: bool,
}

impl ValueCap {
    fn is(&self, page: u16, usage: u16) -> bool {
        !self.is_alias && self.usage_page == page && self.usage_min <= usage && usage <= self.usage_max
    }
}

/// A button capability of an input report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButtonCap {
    pub usage_page: u16,
    pub usage_min: u16,
    pub usage_max: u16,
    pub link: u16,
    pub report_id: u8,
}

impl ButtonCap {
    fn is(&self, page: u16, usage: u16) -> bool {
        self.usage_page == page && self.usage_min <= usage && usage <= self.usage_max
    }
}

/// One link collection of the device, indexed as the HID parser indexes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkNode {
    pub usage_page: u16,
    pub usage: u16,
}

/// An axis as the report holds it: unsigned bits that mean a signed number when the logical minimum is negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Axis {
    pub min: i32,
    pub bits: u16,
}

impl Axis {
    /// The value `raw` stands for. `HidP_GetUsageValue` does not extend the sign (a 16 bit -200 comes back as
    /// 65336), so an axis with a negative minimum is extended from its own width here.
    pub fn read(&self, raw: u32) -> i32 {
        if self.min < 0 && (1..32).contains(&self.bits) {
            let shift = 32 - u32::from(self.bits);
            ((raw << shift) as i32) >> shift
        } else {
            raw as i32
        }
    }
}

/// Where a Precision Touchpad's contacts are in its input report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub report_id: u8,
    /// The link collection of each finger slot, in report order.
    pub fingers: Vec<Finger>,
    /// The link collection that holds the contact count.
    pub count_link: u16,
    pub x: Axis,
    pub y: Axis,
    pub ext: Extents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finger {
    pub link: u16,
    /// The slot reports confidence; one that does not is taken to be confident.
    pub has_confidence: bool,
}

/// The layout from what the HID parser says of the device, or why this is not a touchpad this module reads.
pub fn layout_from_caps(nodes: &[LinkNode], values: &[ValueCap], buttons: &[ButtonCap]) -> Result<Layout, String> {
    let finger_links: Vec<u16> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.usage_page == PAGE_DIGITIZER && n.usage == USAGE_FINGER)
        .map(|(i, _)| i as u16)
        .collect();
    if finger_links.is_empty() {
        return Err(format!("no finger link collection among its {} link collections", nodes.len()));
    }
    let mut fingers = Vec::new();
    let mut report_id = None;
    let mut ext = None;
    let mut axes = None;
    for link in finger_links {
        let of = |page, usage| values.iter().find(|v| v.link == link && v.is(page, usage));
        let (Some(x), Some(y), Some(id)) =
            (of(PAGE_GENERIC, USAGE_X), of(PAGE_GENERIC, USAGE_Y), of(PAGE_DIGITIZER, USAGE_CONTACT_ID))
        else {
            continue;
        };
        let has_button = |usage| buttons.iter().any(|b| b.link == link && b.is(PAGE_DIGITIZER, usage));
        if !has_button(USAGE_TIP) {
            continue;
        }
        if x.report_id != y.report_id || x.report_id != id.report_id {
            return Err(format!("finger link {link} spreads over several reports"));
        }
        match report_id {
            None => report_id = Some(x.report_id),
            Some(r) if r != x.report_id => return Err("the fingers are in different reports".into()),
            Some(_) => {}
        }
        let e = Extents { x_min: x.logical_min, x_max: x.logical_max, y_min: y.logical_min, y_max: y.logical_max };
        let a = (Axis { min: x.logical_min, bits: x.bit_size }, Axis { min: y.logical_min, bits: y.bit_size });
        match ext {
            None => {
                ext = Some(e);
                axes = Some(a);
            }
            Some(first) if first != e || axes != Some(a) => {
                return Err(format!("finger link {link} has other extents than the first"))
            }
            Some(_) => {}
        }
        fingers.push(Finger { link, has_confidence: has_button(USAGE_CONFIDENCE) });
    }
    let (Some(report_id), Some(ext), Some((x, y))) = (report_id, ext, axes) else {
        return Err("no finger has X, Y, contact id and tip switch".into());
    };
    let count = values.iter().find(|v| v.report_id == report_id && v.is(PAGE_DIGITIZER, USAGE_CONTACT_COUNT));
    let Some(count) = count else {
        return Err("no contact count in the touch report".into());
    };
    Ok(Layout { report_id, fingers, count_link: count.link, x, y, ext })
}

/// Reads fields out of one input report. The Win32 side asks the HID parser; the tests answer from a table.
pub trait ReportFields {
    /// The value of `usage` in link collection `link`, `None` when the report does not carry it.
    fn value(&self, page: u16, link: u16, usage: u16) -> Option<u32>;
    /// The buttons of `page` that are down in link collection `link`, `None` when the report has none.
    fn pressed(&self, page: u16, link: u16) -> Option<Vec<u16>>;
}

/// One touch report: how many contacts the frame has in all, and this report's finger slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub count: usize,
    pub slots: Vec<Contact>,
}

/// The touch report `bytes` is, or `None` when it is another report, or one whose fields cannot be read.
pub fn parse_report(layout: &Layout, bytes: &[u8], f: &dyn ReportFields) -> Option<Report> {
    if bytes.first() != Some(&layout.report_id) {
        return None;
    }
    let count = f.value(PAGE_DIGITIZER, layout.count_link, USAGE_CONTACT_COUNT)? as usize;
    let mut slots = Vec::with_capacity(layout.fingers.len());
    for fin in &layout.fingers {
        let (Some(x), Some(y), Some(id), Some(down)) = (
            f.value(PAGE_GENERIC, fin.link, USAGE_X),
            f.value(PAGE_GENERIC, fin.link, USAGE_Y),
            f.value(PAGE_DIGITIZER, fin.link, USAGE_CONTACT_ID),
            f.pressed(PAGE_DIGITIZER, fin.link),
        ) else {
            return None;
        };
        slots.push(Contact {
            id,
            x: layout.x.read(x),
            y: layout.y.read(y),
            tip: down.contains(&USAGE_TIP),
            confident: !fin.has_confidence || down.contains(&USAGE_CONFIDENCE),
        });
    }
    Some(Report { count, slots })
}

/// Joins the reports of a frame. A device that has more contacts than slots in a report sends the count in
/// the first report and the rest in reports that say 0 (hybrid mode).
#[derive(Debug, Default)]
pub struct Assembler {
    remaining: usize,
    got: Vec<Contact>,
}

impl Assembler {
    /// The frame's contacts once its last report is in. An empty frame is every finger up.
    pub fn push(&mut self, r: &Report) -> Option<Vec<Contact>> {
        if r.count > 0 {
            self.got.clear();
            self.remaining = r.count;
        } else if self.remaining == 0 {
            self.got.clear();
            return Some(Vec::new());
        }
        let take = self.remaining.min(r.slots.len());
        self.got.extend_from_slice(&r.slots[..take]);
        self.remaining -= take;
        if take == 0 {
            // A report that carries nothing of a frame in progress: the frame is lost.
            self.remaining = 0;
            self.got.clear();
            return None;
        }
        (self.remaining == 0).then(|| std::mem::take(&mut self.got))
    }
}

/// What of a recognized gesture this module sends. Windows turns a touchpad pinch into Ctrl+wheel itself and
/// the mouse hook already sends that, so a `Zoom` from here would zoom twice.
pub fn forwardable(g: Gesture) -> Option<Gesture> {
    (!matches!(g, Gesture::Zoom(_))).then_some(g)
}

/// One frame through the recognizer, and the filter.
pub fn recognize(reader: &mut TouchReader, now_ms: u32, frame: &[Contact]) -> Option<Gesture> {
    reader.feed(now_ms, frame).and_then(forwardable)
}

/// How long a device that could not be opened or read is left alone before it is tried again.
pub const RETRY_AFTER_MS: u64 = 30_000;

/// Whether a device that went off at `off_since_ms` is due another try at `now_ms`.
pub fn retry_due(off_since_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(off_since_ms) >= RETRY_AFTER_MS
}

#[derive(Debug)]
enum Slot<T> {
    Ready(T),
    Off { since_ms: u64 },
}

/// What to do with a packet from a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Read it.
    Ready,
    /// First packet from this device: open it.
    Open,
    /// It was off and its time is up: open it again.
    Retry,
    /// Off, and not yet due.
    Skip,
}

/// The devices seen, by Raw Input handle: ready ones, and off ones waiting for their retry.
#[derive(Debug)]
pub struct Table<T> {
    slots: HashMap<isize, Slot<T>>,
    announced: bool,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self { slots: HashMap::new(), announced: false }
    }
}

impl<T> Table<T> {
    pub fn want(&self, dev: isize, now_ms: u64) -> Want {
        match self.slots.get(&dev) {
            None => Want::Open,
            Some(Slot::Ready(_)) => Want::Ready,
            Some(Slot::Off { since_ms }) if retry_due(*since_ms, now_ms) => Want::Retry,
            Some(Slot::Off { .. }) => Want::Skip,
        }
    }

    pub fn get_mut(&mut self, dev: isize) -> Option<&mut T> {
        match self.slots.get_mut(&dev) {
            Some(Slot::Ready(t)) => Some(t),
            _ => None,
        }
    }

    /// `true` the first time any device becomes ready: the moment to say this PC has a touchpad.
    pub fn ready(&mut self, dev: isize, t: T) -> bool {
        self.slots.insert(dev, Slot::Ready(t));
        !std::mem::replace(&mut self.announced, true)
    }

    pub fn off(&mut self, dev: isize, now_ms: u64) {
        self.slots.insert(dev, Slot::Off { since_ms: now_ms });
    }

    /// The device went away; whether it was known.
    pub fn removed(&mut self, dev: isize) -> bool {
        self.slots.remove(&dev).is_some()
    }

    /// The device (re)appeared: an off one is tried at its next packet. Whether there was one to clear.
    pub fn arrived(&mut self, dev: isize) -> bool {
        matches!(self.slots.get(&dev), Some(Slot::Off { .. })) && self.slots.remove(&dev).is_some()
    }
}

/// A registry value as read, for `handling`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    Dword(u32),
    /// Not there: Windows then uses its default, which is on.
    Absent,
    Unreadable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handling {
    /// Both are "Nothing".
    Off,
    /// Windows still acts on these gestures (names of the settings).
    Still(Vec<&'static str>),
    /// Could not tell.
    Unknown(String),
}

/// Registry value names under `HKCU\Software\Microsoft\Windows\CurrentVersion\PrecisionTouchPad`. 0 means
/// "Nothing"; the other values pick what Windows does. Not confirmed on a PC here: from what the Settings
/// page is known to write, unverified.
pub const REG_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\PrecisionTouchPad";
pub const REG_THREE: &str = "ThreeFingerSlideEnabled";
pub const REG_FOUR: &str = "FourFingerSlideEnabled";

pub fn handling(three: &RegValue, four: &RegValue) -> Handling {
    let mut still = Vec::new();
    for (name, v) in [(REG_THREE, three), (REG_FOUR, four)] {
        match v {
            RegValue::Dword(0) => {}
            RegValue::Dword(_) | RegValue::Absent => still.push(name),
            RegValue::Unreadable(e) => return Handling::Unknown(format!("{name}: {e}")),
        }
    }
    if still.is_empty() {
        Handling::Off
    } else {
        Handling::Still(still)
    }
}

#[cfg(windows)]
pub use sys::{on_device_change, on_input, start};

#[cfg(windows)]
mod sys {
    use std::cell::RefCell;
    use std::time::Instant;

    use flowseam_core::backend::CapturedEvent;
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HidP_GetButtonCaps, HidP_GetCaps, HidP_GetLinkCollectionNodes, HidP_GetUsageValue, HidP_GetUsages,
        HidP_GetValueCaps, HidP_Input, HidP_MaxUsageListLength, HIDP_BUTTON_CAPS, HIDP_CAPS,
        HIDP_LINK_COLLECTION_NODE, HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS, PHIDP_PREPARSED_DATA,
    };
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE, HWND};
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, GetRawInputDeviceInfoW, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
        RAWINPUTHEADER, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIDI_PREPARSEDDATA, RID_INPUT, RIM_TYPEHID,
    };

    use windows_sys::Win32::UI::WindowsAndMessaging::{GIDC_ARRIVAL, GIDC_REMOVAL};

    use super::*;

    /// Reports in a row that name this device's touch report but cannot be read before it is given up.
    const MAX_UNREADABLE: u32 = 50;

    fn log(m: &str) {
        crate::app::log(&format!("touchpad: {m}"));
    }

    struct Ready {
        /// 8-byte aligned storage of the HID parser's preparsed data.
        preparsed: Vec<u64>,
        layout: Layout,
        asm: Assembler,
        reader: TouchReader,
        unreadable: u32,
    }

    struct State {
        /// Registration failed or never happened: nothing is read.
        on: bool,
        devices: Table<Box<Ready>>,
        started: Instant,
    }

    thread_local! {
        static STATE: RefCell<State> = RefCell::new(State { on: false, devices: Table::default(), started: Instant::now() });
    }

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn read_dword(name: &str) -> RegValue {
        let key = utf16(REG_KEY);
        let name = utf16(name);
        let mut data = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: NUL-terminated names that outlive the call, and a DWORD buffer with its size.
        let e = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut data as *mut u32).cast(),
                &mut size,
            )
        };
        match e {
            ERROR_SUCCESS => RegValue::Dword(data),
            ERROR_FILE_NOT_FOUND => RegValue::Absent,
            other => RegValue::Unreadable(format!("RegGetValueW error {other}")),
        }
    }

    /// Registers for touchpad input on `hwnd` (the capture thread's window). Call on the thread that owns
    /// `hwnd`. Failure is logged once; nothing else is affected. A PC with no touchpad logs nothing more:
    /// Windows' own gesture settings are checked when the first touchpad is seen (`announce`).
    pub fn start(hwnd: HWND) {
        let rid = RAWINPUTDEVICE {
            usUsagePage: PAGE_DIGITIZER,
            usUsage: USAGE_TOUCHPAD,
            dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
            hwndTarget: hwnd,
        };
        // SAFETY: one fully initialised RAWINPUTDEVICE and its size.
        let ok = unsafe { RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) };
        if ok == 0 {
            log(&format!("Raw Input registration failed ({}): this PC's touchpad gestures are not sent", std::io::Error::last_os_error()));
            return;
        }
        STATE.with(|s| s.borrow_mut().on = true);
    }

    /// The first touchpad this process sees: say so, and say what Windows does with the gestures meanwhile.
    fn announce() {
        log("reading the touchpad (Raw Input, usage page 0x0D usage 0x05)");
        match handling(&read_dword(REG_THREE), &read_dword(REG_FOUR)) {
            Handling::Off => {}
            Handling::Still(which) => log(&format!(
                "Windows still handles 3/4-finger gestures; set them to Nothing so they are only sent to the peer ({} is not 0)",
                which.join(", ")
            )),
            Handling::Unknown(why) => log(&format!("could not read Windows' 3/4-finger gesture settings ({why}); if they are not set to Nothing, Windows acts on them here too")),
        }
    }

    /// `WM_INPUT_DEVICE_CHANGE`: a touchpad was unplugged (forget it) or plugged in (try it again if it was off).
    pub fn on_device_change(wparam: usize, lparam: isize) {
        STATE.with(|s| {
            let Ok(mut s) = s.try_borrow_mut() else { return };
            let dev = lparam;
            match wparam as u32 {
                GIDC_REMOVAL if s.devices.removed(dev) => log(&format!("device {dev:#x} removed")),
                GIDC_ARRIVAL if s.devices.arrived(dev) => log(&format!("device {dev:#x} arrived: trying it again")),
                _ => {}
            }
        });
    }

    /// `WM_INPUT`: one Raw Input packet. Never panics, never blocks.
    pub fn on_input(raw: HRAWINPUT) {
        STATE.with(|s| {
            let Ok(mut s) = s.try_borrow_mut() else { return };
            if !s.on {
                return;
            }
            let Some((device, reports)) = read_packet(raw) else { return };
            let now_ms = s.started.elapsed().as_millis() as u64;
            match s.devices.want(device, now_ms) {
                Want::Skip => return,
                Want::Ready => {}
                w @ (Want::Open | Want::Retry) => {
                    if w == Want::Retry {
                        log(&format!("device {device:#x}: trying it again"));
                    }
                    match try_open(device as HANDLE) {
                        Ok(r) => {
                            if s.devices.ready(device, Box::new(r)) {
                                announce();
                            }
                        }
                        Err(e) => {
                            log(&format!("device {device:#x} not read: {e} (trying again in {} s, or when it is plugged in again)", RETRY_AFTER_MS / 1000));
                            s.devices.off(device, now_ms);
                            return;
                        }
                    }
                }
            }
            let Some(r) = s.devices.get_mut(device) else { return };
            let mut gestures = Vec::new();
            let mut dead = None;
            for rep in &reports {
                if let Some(g) = feed(r, now_ms as u32, rep, &mut dead) {
                    gestures.push(g);
                }
            }
            if let Some(why) = dead {
                log(&format!("device {device:#x}: {why}; trying again in {} s", RETRY_AFTER_MS / 1000));
                s.devices.off(device, now_ms);
            }
            drop(s);
            if gestures.is_empty() {
                return;
            }
            if let Some(t) = crate::capture::target() {
                for g in gestures {
                    t.event(CapturedEvent::Gesture(g));
                }
            }
        });
    }

    fn feed(r: &mut Ready, now: u32, report: &[u8], dead: &mut Option<String>) -> Option<Gesture> {
        if report.first() != Some(&r.layout.report_id) {
            return None;
        }
        let f = HidFields { pre: r.preparsed.as_ptr() as PHIDP_PREPARSED_DATA, report };
        let Some(rep) = parse_report(&r.layout, report, &f) else {
            r.unreadable += 1;
            if r.unreadable == MAX_UNREADABLE {
                *dead = Some(format!("{MAX_UNREADABLE} touch reports in a row could not be read: its gestures are not sent"));
            }
            return None;
        };
        r.unreadable = 0;
        let frame = r.asm.push(&rep)?;
        recognize(&mut r.reader, now, &frame)
    }

    /// The packet's device handle and the HID reports in it, or `None` when it is not a HID packet or unreadable.
    fn read_packet(raw: HRAWINPUT) -> Option<(isize, Vec<Vec<u8>>)> {
        let mut size = 0u32;
        let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        // SAFETY: a size query (null buffer) on the handle WM_INPUT carried.
        if unsafe { GetRawInputData(raw, RID_INPUT, std::ptr::null_mut(), &mut size, header) } != 0 || size == 0 {
            return None;
        }
        // u64 storage: RAWINPUT wants 8-byte alignment.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: `buf` holds at least `size` bytes.
        let got = unsafe { GetRawInputData(raw, RID_INPUT, buf.as_mut_ptr().cast(), &mut size, header) };
        if got == u32::MAX || got as usize > buf.len() * 8 {
            return None;
        }
        let total = got as usize;
        // SAFETY: `buf` holds a RAWINPUT the call just wrote, `total` bytes of it; fields are read by value.
        let (dtype, device, hid_at) = unsafe {
            let ri = buf.as_ptr().cast::<RAWINPUT>();
            ((*ri).header.dwType, (*ri).header.hDevice, std::ptr::addr_of!((*ri).data) as usize - buf.as_ptr() as usize)
        };
        if dtype != RIM_TYPEHID || hid_at + 8 > total {
            return None;
        }
        let bytes = buf.as_ptr().cast::<u8>();
        // SAFETY: two u32 inside the packet (checked above).
        let (each, count) = unsafe {
            (std::ptr::read_unaligned(bytes.add(hid_at).cast::<u32>()) as usize, std::ptr::read_unaligned(bytes.add(hid_at + 4).cast::<u32>()) as usize)
        };
        let data = hid_at + 8;
        if each == 0 || count == 0 || count > 64 || data + each * count > total {
            return None;
        }
        let reports = (0..count)
            .map(|i| {
                // SAFETY: inside the packet (checked above).
                unsafe { std::slice::from_raw_parts(bytes.add(data + i * each), each) }.to_vec()
            })
            .collect();
        Some((device as isize, reports))
    }

    fn try_open(device: HANDLE) -> Result<Ready, String> {
        let mut size = 0u32;
        // SAFETY: a size query (null buffer).
        if unsafe { GetRawInputDeviceInfoW(device, RIDI_PREPARSEDDATA, std::ptr::null_mut(), &mut size) } != 0 || size == 0 {
            return Err(format!("preparsed data size: {}", std::io::Error::last_os_error()));
        }
        let mut pre = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: `pre` holds at least `size` bytes.
        let n = unsafe { GetRawInputDeviceInfoW(device, RIDI_PREPARSEDDATA, pre.as_mut_ptr().cast(), &mut size) };
        if n == u32::MAX {
            return Err(format!("preparsed data: {}", std::io::Error::last_os_error()));
        }
        let p = pre.as_ptr() as PHIDP_PREPARSED_DATA;
        // SAFETY: plain-data structs, zero is a valid value.
        let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
        // SAFETY: valid preparsed data and a HIDP_CAPS to fill.
        if unsafe { HidP_GetCaps(p, &mut caps) } != HIDP_STATUS_SUCCESS {
            return Err("HidP_GetCaps failed".into());
        }
        if caps.UsagePage != PAGE_DIGITIZER || caps.Usage != USAGE_TOUCHPAD {
            return Err(format!("not a touchpad (usage page {:#x} usage {:#x})", caps.UsagePage, caps.Usage));
        }

        let mut raw_nodes: Vec<HIDP_LINK_COLLECTION_NODE> = vec![unsafe { std::mem::zeroed() }; caps.NumberLinkCollectionNodes as usize];
        let mut n_nodes = raw_nodes.len() as u32;
        // SAFETY: `raw_nodes` has `n_nodes` entries.
        if unsafe { HidP_GetLinkCollectionNodes(raw_nodes.as_mut_ptr(), &mut n_nodes, p) } != HIDP_STATUS_SUCCESS {
            return Err("HidP_GetLinkCollectionNodes failed".into());
        }
        raw_nodes.truncate(n_nodes as usize);
        let nodes: Vec<LinkNode> = raw_nodes.iter().map(|n| LinkNode { usage_page: n.LinkUsagePage, usage: n.LinkUsage }).collect();

        let mut raw_values: Vec<HIDP_VALUE_CAPS> = vec![unsafe { std::mem::zeroed() }; caps.NumberInputValueCaps as usize];
        let mut n_values = raw_values.len() as u16;
        // SAFETY: `raw_values` has `n_values` entries.
        if unsafe { HidP_GetValueCaps(HidP_Input, raw_values.as_mut_ptr(), &mut n_values, p) } != HIDP_STATUS_SUCCESS {
            return Err("HidP_GetValueCaps failed".into());
        }
        raw_values.truncate(n_values as usize);
        let values: Vec<ValueCap> = raw_values
            .iter()
            .map(|v| {
                // SAFETY: the union is Range or NotRange by IsRange; both start with usage fields.
                let (lo, hi) = unsafe {
                    if v.IsRange != 0 {
                        (v.Anonymous.Range.UsageMin, v.Anonymous.Range.UsageMax)
                    } else {
                        (v.Anonymous.NotRange.Usage, v.Anonymous.NotRange.Usage)
                    }
                };
                ValueCap {
                    usage_page: v.UsagePage,
                    usage_min: lo,
                    usage_max: hi,
                    link: v.LinkCollection,
                    report_id: v.ReportID,
                    logical_min: v.LogicalMin,
                    logical_max: v.LogicalMax,
                    bit_size: v.BitSize,
                    is_alias: v.IsAlias != 0,
                }
            })
            .collect();

        let mut raw_buttons: Vec<HIDP_BUTTON_CAPS> = vec![unsafe { std::mem::zeroed() }; caps.NumberInputButtonCaps as usize];
        let mut n_buttons = raw_buttons.len() as u16;
        // SAFETY: `raw_buttons` has `n_buttons` entries.
        if unsafe { HidP_GetButtonCaps(HidP_Input, raw_buttons.as_mut_ptr(), &mut n_buttons, p) } != HIDP_STATUS_SUCCESS {
            return Err("HidP_GetButtonCaps failed".into());
        }
        raw_buttons.truncate(n_buttons as usize);
        let buttons: Vec<ButtonCap> = raw_buttons
            .iter()
            .map(|b| {
                // SAFETY: as for the value caps.
                let (lo, hi) = unsafe {
                    if b.IsRange != 0 {
                        (b.Anonymous.Range.UsageMin, b.Anonymous.Range.UsageMax)
                    } else {
                        (b.Anonymous.NotRange.Usage, b.Anonymous.NotRange.Usage)
                    }
                };
                ButtonCap { usage_page: b.UsagePage, usage_min: lo, usage_max: hi, link: b.LinkCollection, report_id: b.ReportID }
            })
            .collect();

        let layout = layout_from_caps(&nodes, &values, &buttons)?;
        let reader = TouchReader::new(layout.ext).map_err(str::to_string)?;
        log(&format!(
            "device ready: {} finger slots, report id {}, extents x {}..{} y {}..{}",
            layout.fingers.len(),
            layout.report_id,
            layout.ext.x_min,
            layout.ext.x_max,
            layout.ext.y_min,
            layout.ext.y_max
        ));
        Ok(Ready { preparsed: pre, layout, asm: Assembler::default(), reader, unreadable: 0 })
    }

    /// The HID parser over one report.
    struct HidFields<'a> {
        pre: PHIDP_PREPARSED_DATA,
        report: &'a [u8],
    }

    impl ReportFields for HidFields<'_> {
        fn value(&self, page: u16, link: u16, usage: u16) -> Option<u32> {
            let mut v = 0u32;
            // SAFETY: preparsed data of the device this report came from, and the whole report with its length.
            let st = unsafe { HidP_GetUsageValue(HidP_Input, page, link, usage, &mut v, self.pre, self.report.as_ptr(), self.report.len() as u32) };
            (st == HIDP_STATUS_SUCCESS).then_some(v)
        }

        fn pressed(&self, page: u16, link: u16) -> Option<Vec<u16>> {
            // SAFETY: plain query on the device's preparsed data.
            let max = unsafe { HidP_MaxUsageListLength(HidP_Input, page, self.pre) };
            let mut list = vec![0u16; max.max(1) as usize];
            let mut len = list.len() as u32;
            // The call takes a mutable report; it does not change it.
            let mut copy = self.report.to_vec();
            // SAFETY: `list` has `len` entries; `copy` is the whole report.
            let st = unsafe { HidP_GetUsages(HidP_Input, page, link, list.as_mut_ptr(), &mut len, self.pre, copy.as_mut_ptr(), copy.len() as u32) };
            if st != HIDP_STATUS_SUCCESS {
                return None;
            }
            list.truncate(len as usize);
            Some(list)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: i32 = 1000;
    const HEIGHT: i32 = 600;

    /// What a Precision Touchpad's descriptor yields: link 0 is the touchpad, links 1..=slots the fingers;
    /// the count and scan time are at link 0; the click button is at link 0. Tip and confidence are one button
    /// cap each.
    fn caps(slots: u16, confidence: bool) -> (Vec<LinkNode>, Vec<ValueCap>, Vec<ButtonCap>) {
        caps_with(slots, confidence, false)
    }

    /// As `caps`; with `ranged`, tip and confidence are one cap covering the usages 0x42..=0x47, as the HID
    /// parser reports a run of adjacent usages declared together.
    fn caps_with(slots: u16, confidence: bool, ranged: bool) -> (Vec<LinkNode>, Vec<ValueCap>, Vec<ButtonCap>) {
        let mut nodes = vec![LinkNode { usage_page: PAGE_DIGITIZER, usage: USAGE_TOUCHPAD }];
        let mut values = vec![
            ValueCap { usage_page: PAGE_DIGITIZER, usage_min: 0x56, usage_max: 0x56, link: 0, report_id: 1, logical_min: 0, logical_max: 65535, bit_size: 16, is_alias: false },
            ValueCap { usage_page: PAGE_DIGITIZER, usage_min: USAGE_CONTACT_COUNT, usage_max: USAGE_CONTACT_COUNT, link: 0, report_id: 1, logical_min: 0, logical_max: 10, bit_size: 4, is_alias: false },
        ];
        let mut buttons = vec![ButtonCap { usage_page: 0x09, usage_min: 1, usage_max: 1, link: 0, report_id: 1 }];
        for link in 1..=slots {
            nodes.push(LinkNode { usage_page: PAGE_DIGITIZER, usage: USAGE_FINGER });
            let v = |page, usage, max| ValueCap { usage_page: page, usage_min: usage, usage_max: usage, link, report_id: 1, logical_min: 0, logical_max: max, bit_size: 16, is_alias: false };
            values.push(v(PAGE_GENERIC, USAGE_X, WIDTH));
            values.push(v(PAGE_GENERIC, USAGE_Y, HEIGHT));
            values.push(v(PAGE_DIGITIZER, USAGE_CONTACT_ID, 15));
            let button = |lo, hi| ButtonCap { usage_page: PAGE_DIGITIZER, usage_min: lo, usage_max: hi, link, report_id: 1 };
            if ranged {
                buttons.push(button(USAGE_TIP, if confidence { USAGE_CONFIDENCE } else { USAGE_TIP }));
            } else {
                buttons.push(button(USAGE_TIP, USAGE_TIP));
                if confidence {
                    buttons.push(button(USAGE_CONFIDENCE, USAGE_CONFIDENCE));
                }
            }
        }
        (nodes, values, buttons)
    }

    fn layout(slots: u16) -> Layout {
        let (n, v, b) = caps(slots, true);
        layout_from_caps(&n, &v, &b).unwrap()
    }

    /// A report as a table of what each field reads.
    #[derive(Default)]
    struct Fake {
        values: HashMap<(u16, u16, u16), u32>,
        down: HashMap<(u16, u16), Vec<u16>>,
    }

    impl ReportFields for Fake {
        fn value(&self, page: u16, link: u16, usage: u16) -> Option<u32> {
            self.values.get(&(page, link, usage)).copied()
        }
        fn pressed(&self, page: u16, link: u16) -> Option<Vec<u16>> {
            self.down.get(&(page, link)).cloned()
        }
    }

    /// A report with `count` as the frame total and `fingers` in the first slots: (id, x, y, tip, confident).
    fn report(l: &Layout, count: u32, fingers: &[(u32, i32, i32, bool, bool)]) -> Fake {
        let mut f = Fake::default();
        f.values.insert((PAGE_DIGITIZER, l.count_link, USAGE_CONTACT_COUNT), count);
        for (fin, &(id, x, y, tip, conf)) in l.fingers.iter().zip(fingers) {
            f.values.insert((PAGE_GENERIC, fin.link, USAGE_X), x as u32);
            f.values.insert((PAGE_GENERIC, fin.link, USAGE_Y), y as u32);
            f.values.insert((PAGE_DIGITIZER, fin.link, USAGE_CONTACT_ID), id);
            let mut d = Vec::new();
            if tip {
                d.push(USAGE_TIP);
            }
            if conf {
                d.push(USAGE_CONFIDENCE);
            }
            f.down.insert((PAGE_DIGITIZER, fin.link), d);
        }
        // Slots past the fingers hold whatever was there before: zeros that read fine.
        for fin in l.fingers.iter().skip(fingers.len()) {
            f.values.insert((PAGE_GENERIC, fin.link, USAGE_X), 0);
            f.values.insert((PAGE_GENERIC, fin.link, USAGE_Y), 0);
            f.values.insert((PAGE_DIGITIZER, fin.link, USAGE_CONTACT_ID), 0);
            f.down.insert((PAGE_DIGITIZER, fin.link), Vec::new());
        }
        f
    }

    const ID1: [u8; 1] = [1];

    #[test]
    fn the_layout_of_a_typical_touchpad() {
        let l = layout(5);
        assert_eq!(l.report_id, 1);
        assert_eq!(l.fingers.iter().map(|f| f.link).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
        assert!(l.fingers.iter().all(|f| f.has_confidence));
        assert_eq!(l.count_link, 0);
        assert_eq!(l.ext, Extents { x_min: 0, x_max: WIDTH, y_min: 0, y_max: HEIGHT });
        // A usage range that covers a usage counts for it.
        let (n, mut v, b) = caps(2, true);
        v[0].usage_min = 0x54;
        v[0].usage_max = 0x56;
        v.remove(1);
        assert_eq!(layout_from_caps(&n, &v, &b).unwrap().count_link, 0);
    }

    #[test]
    fn a_device_that_is_not_a_precision_touchpad_is_refused_with_a_reason() {
        let (n, v, b) = caps(3, true);
        assert!(layout_from_caps(&[], &v, &b).unwrap_err().contains("no finger link collection"));
        assert!(layout_from_caps(&n[..1], &v, &b).unwrap_err().contains("no finger link collection"));
        let no_tip: Vec<ButtonCap> = b.iter().copied().filter(|b| b.usage_min != USAGE_TIP).collect();
        assert!(layout_from_caps(&n, &v, &no_tip).unwrap_err().contains("no finger has"));
        let no_x: Vec<ValueCap> = v.iter().copied().filter(|v| v.usage_min != USAGE_X).collect();
        assert!(layout_from_caps(&n, &no_x, &b).unwrap_err().contains("no finger has"));
        let no_count: Vec<ValueCap> = v.iter().copied().filter(|v| v.usage_min != USAGE_CONTACT_COUNT).collect();
        assert!(layout_from_caps(&n, &no_count, &b).unwrap_err().contains("contact count"));
        let mut other_report = v.clone();
        other_report.iter_mut().filter(|v| v.link == 2).for_each(|v| v.report_id = 2);
        assert!(layout_from_caps(&n, &other_report, &b).is_err());
        let mut other_ext = v.clone();
        other_ext.iter_mut().filter(|v| v.link == 3 && v.usage_min == USAGE_X).for_each(|v| v.logical_max = 2000);
        assert!(layout_from_caps(&n, &other_ext, &b).unwrap_err().contains("extents"));
        let mut alias = v.clone();
        alias.iter_mut().filter(|v| v.usage_min == USAGE_CONTACT_COUNT).for_each(|v| v.is_alias = true);
        assert!(layout_from_caps(&n, &alias, &b).is_err(), "an alias is not the capability itself");
    }

    #[test]
    fn a_slot_without_confidence_is_confident() {
        let (n, v, b) = caps(2, false);
        let l = layout_from_caps(&n, &v, &b).unwrap();
        assert!(l.fingers.iter().all(|f| !f.has_confidence));
        let r = parse_report(&l, &ID1, &report(&l, 1, &[(4, 10, 20, true, false)])).unwrap();
        assert!(r.slots[0].confident && r.slots[0].tip);
    }

    #[test]
    fn a_report_parses_into_contacts() {
        let l = layout(5);
        let f = report(&l, 2, &[(3, 100, 200, true, true), (7, 900, 50, true, false)]);
        let r = parse_report(&l, &ID1, &f).unwrap();
        assert_eq!(r.count, 2);
        assert_eq!(r.slots.len(), 5);
        assert_eq!(r.slots[0], Contact { id: 3, x: 100, y: 200, tip: true, confident: true });
        assert_eq!(r.slots[1], Contact { id: 7, x: 900, y: 50, tip: true, confident: false }, "a palm");
    }

    #[test]
    fn other_reports_and_unreadable_ones_are_not_touch_reports() {
        let l = layout(3);
        let f = report(&l, 1, &[(1, 1, 1, true, true)]);
        assert_eq!(parse_report(&l, &[2], &f), None, "another report id");
        assert_eq!(parse_report(&l, &[], &f), None, "empty");
        let mut no_y = report(&l, 1, &[(1, 1, 1, true, true)]);
        no_y.values.remove(&(PAGE_GENERIC, 1, USAGE_Y));
        assert_eq!(parse_report(&l, &ID1, &no_y), None, "a field the parser cannot read");
        let mut no_count = report(&l, 1, &[(1, 1, 1, true, true)]);
        no_count.values.remove(&(PAGE_DIGITIZER, 0, USAGE_CONTACT_COUNT));
        assert_eq!(parse_report(&l, &ID1, &no_count), None);
    }

    fn slots(n: usize, first_id: u32) -> Vec<Contact> {
        (0..n).map(|i| Contact { id: first_id + i as u32, x: i as i32, y: 0, tip: true, confident: true }).collect()
    }

    #[test]
    fn a_frame_in_one_report() {
        let mut a = Assembler::default();
        let frame = a.push(&Report { count: 3, slots: [slots(3, 1), slots(2, 90)].concat() }).unwrap();
        assert_eq!(frame, slots(3, 1), "only as many slots as the count");
    }

    #[test]
    fn a_hybrid_frame_takes_its_continuation_reports() {
        let mut a = Assembler::default();
        // 7 contacts, 5 slots per report: the first report says 7, the second says 0.
        assert_eq!(a.push(&Report { count: 7, slots: slots(5, 1) }), None);
        let frame = a.push(&Report { count: 0, slots: slots(5, 6) }).unwrap();
        assert_eq!(frame.iter().map(|c| c.id).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6, 7]);
        // The next frame starts clean.
        assert_eq!(a.push(&Report { count: 2, slots: slots(5, 20) }).unwrap(), slots(2, 20));
    }

    #[test]
    fn all_fingers_up_is_an_empty_frame_and_a_new_count_restarts_a_frame_in_progress() {
        let mut a = Assembler::default();
        assert_eq!(a.push(&Report { count: 0, slots: slots(5, 0) }), Some(Vec::new()));
        assert_eq!(a.push(&Report { count: 7, slots: slots(5, 1) }), None);
        // The continuation never came; a new frame begins.
        assert_eq!(a.push(&Report { count: 2, slots: slots(5, 30) }).unwrap(), slots(2, 30));
    }

    #[test]
    fn a_report_with_no_slots_loses_the_frame_without_panicking() {
        let mut a = Assembler::default();
        assert_eq!(a.push(&Report { count: 2, slots: Vec::new() }), None);
        assert_eq!(a.push(&Report { count: 0, slots: Vec::new() }), Some(Vec::new()));
    }

    /// Reports, parsed and assembled, into the recognizer: a swipe on the wire is a gesture out.
    #[test]
    fn three_fingers_swiping_right_on_the_wire_is_space_left() {
        let l = layout(5);
        let mut asm = Assembler::default();
        let mut reader = TouchReader::new(l.ext).unwrap();
        let mut out = Vec::new();
        for step in 0..=10 {
            let dx = 30 * step;
            let fingers = [(1, 300 + dx, 300, true, true), (2, 400 + dx, 300, true, true), (3, 500 + dx, 300, true, true)];
            let rep = parse_report(&l, &ID1, &report(&l, 3, &fingers)).unwrap();
            if let Some(frame) = asm.push(&rep) {
                out.extend(recognize(&mut reader, 8 * step as u32, &frame));
            }
        }
        // Fingers up.
        let rep = parse_report(&l, &ID1, &report(&l, 0, &[])).unwrap();
        let frame = asm.push(&rep).unwrap();
        assert_eq!(recognize(&mut reader, 200, &frame), None);
        assert_eq!(out, [Gesture::SpaceLeft]);
    }

    #[test]
    fn a_palm_on_the_wire_does_not_make_a_third_finger() {
        let l = layout(5);
        let mut asm = Assembler::default();
        let mut reader = TouchReader::new(l.ext).unwrap();
        let mut out = Vec::new();
        for step in 0..=10 {
            let dx = 30 * step;
            let fingers = [(1, 300 + dx, 300, true, true), (2, 400 + dx, 300, true, true), (3, 100, 550, true, false)];
            let rep = parse_report(&l, &ID1, &report(&l, 3, &fingers)).unwrap();
            out.extend(asm.push(&rep).and_then(|frame| reader.feed(8 * step as u32, &frame)));
        }
        assert_eq!(out, []);
    }

    #[test]
    fn a_range_of_buttons_reads_like_single_ones() {
        let (n, v, b) = caps_with(5, true, true);
        assert_eq!(b.iter().filter(|b| b.usage_min != b.usage_max).count(), 5, "the helper really builds ranges");
        let ranged = layout_from_caps(&n, &v, &b).unwrap();
        assert_eq!(ranged, layout(5), "tip and confidence are found inside the range");
        // Tip only, as a one-usage range: a slot without confidence.
        let (n, v, b) = caps_with(2, false, true);
        let l = layout_from_caps(&n, &v, &b).unwrap();
        assert!(l.fingers.iter().all(|f| !f.has_confidence));
        // The usages between tip and confidence in the range are not confidence's absence: a range that stops
        // short of 0x47 has none.
        let (n, v, mut b) = caps_with(2, true, true);
        b.iter_mut().filter(|b| b.link == 1).for_each(|b| b.usage_max = 0x45);
        let l = layout_from_caps(&n, &v, &b).unwrap();
        assert_eq!(l.fingers.iter().map(|f| f.has_confidence).collect::<Vec<_>>(), [false, true]);
        // A range that does not reach the tip switch makes no finger.
        let (n, v, mut b) = caps_with(2, true, true);
        b.iter_mut().for_each(|b| b.usage_min = 0x43);
        assert!(layout_from_caps(&n, &v, &b).is_err());
    }

    #[test]
    fn a_signed_axis_is_extended_from_its_own_width() {
        let a = Axis { min: -500, bits: 16 };
        assert_eq!(a.read(0xFF38), -200, "-200 as the parser returns it, zero-extended");
        assert_eq!(a.read(300), 300);
        assert_eq!(a.read(0x8000), -32768);
        assert_eq!(Axis { min: -500, bits: 32 }.read(0xFFFF_FF38), -200);
        assert_eq!(Axis { min: 0, bits: 16 }.read(0xFF38), 0xFF38, "an unsigned axis keeps its high values");
        assert_eq!(Axis { min: -1, bits: 0 }.read(5), 5, "no width known: as it came");
    }

    #[test]
    fn a_negative_logical_minimum_reaches_the_contacts_through_the_parser() {
        let (n, mut v, b) = caps(3, true);
        for c in v.iter_mut().filter(|c| c.usage_page == PAGE_GENERIC) {
            c.logical_min = -500;
            c.logical_max = 500;
        }
        let l = layout_from_caps(&n, &v, &b).unwrap();
        assert_eq!(l.ext, Extents { x_min: -500, x_max: 500, y_min: -500, y_max: 500 });
        let mut f = report(&l, 2, &[(1, -200, -100, true, true), (2, 300, 250, true, true)]);
        // What HidP_GetUsageValue returns: the field's own 16 bits, no sign.
        f.values.iter_mut().filter(|(k, _)| k.0 == PAGE_GENERIC).for_each(|(_, raw)| *raw &= 0xFFFF);
        let r = parse_report(&l, &ID1, &f).unwrap();
        assert_eq!((r.slots[0].x, r.slots[0].y), (-200, -100));
        assert_eq!((r.slots[1].x, r.slots[1].y), (300, 250));
    }

    /// Two fingers spreading, on the wire, then up.
    fn pinch_frames(l: &Layout) -> Vec<Vec<Contact>> {
        let mut asm = Assembler::default();
        let mut frames = Vec::new();
        for step in 0..=10 {
            let d = 200 + 40 * step;
            let fingers = [(1, 500 - d / 2, 300, true, true), (2, 500 + d / 2, 300, true, true)];
            let rep = parse_report(l, &ID1, &report(l, 2, &fingers)).unwrap();
            frames.extend(asm.push(&rep));
        }
        frames
    }

    #[test]
    fn a_pinch_sends_no_zoom_because_windows_sends_it_as_ctrl_wheel() {
        let l = layout(5);
        let frames = pinch_frames(&l);
        // The recognizer alone does see a zoom...
        let mut bare = TouchReader::new(l.ext).unwrap();
        let zooms: Vec<Gesture> = frames.iter().enumerate().filter_map(|(i, f)| bare.feed(8 * i as u32, f)).collect();
        assert!(zooms.iter().any(|g| matches!(g, Gesture::Zoom(s) if *s > 0)), "{zooms:?}");
        // ...and this path forwards none of it.
        let mut reader = TouchReader::new(l.ext).unwrap();
        let sent: Vec<Gesture> = frames.iter().enumerate().filter_map(|(i, f)| recognize(&mut reader, 8 * i as u32, f)).collect();
        assert_eq!(sent, []);
        assert_eq!(forwardable(Gesture::Zoom(-2)), None);
        for g in [Gesture::SpaceLeft, Gesture::SpaceRight, Gesture::Overview, Gesture::AppWindows] {
            assert_eq!(forwardable(g), Some(g));
        }
    }

    #[test]
    fn a_device_is_retried_after_the_back_off() {
        assert!(!retry_due(1_000, 1_000));
        assert!(!retry_due(1_000, 1_000 + RETRY_AFTER_MS - 1));
        assert!(retry_due(1_000, 1_000 + RETRY_AFTER_MS));
        assert!(retry_due(1_000, u64::MAX));
        assert!(!retry_due(5_000, 1_000), "a clock that reads earlier is not due");
    }

    #[test]
    fn the_table_opens_retries_and_forgets() {
        let mut t: Table<u8> = Table::default();
        assert_eq!(t.want(7, 0), Want::Open);
        t.off(7, 100);
        assert_eq!(t.want(7, 100), Want::Skip);
        assert_eq!(t.want(7, 100 + RETRY_AFTER_MS - 1), Want::Skip);
        assert_eq!(t.want(7, 100 + RETRY_AFTER_MS), Want::Retry);
        // Failing again starts the wait over.
        t.off(7, 100 + RETRY_AFTER_MS);
        assert_eq!(t.want(7, 100 + RETRY_AFTER_MS + 1), Want::Skip);
        assert_eq!(t.want(7, 100 + 2 * RETRY_AFTER_MS), Want::Retry);
        // Working: read, and reachable.
        t.ready(7, 42);
        assert_eq!(t.want(7, 0), Want::Ready);
        assert_eq!(t.get_mut(7).copied(), Some(42));
        // Unplugged: gone, and the next packet opens it afresh.
        assert!(t.removed(7));
        assert!(!t.removed(7), "forgotten already");
        assert_eq!(t.want(7, 0), Want::Open);
        assert_eq!(t.get_mut(7), None);
    }

    #[test]
    fn a_plugged_in_device_is_tried_at_once_but_a_working_one_is_left_alone() {
        let mut t: Table<u8> = Table::default();
        t.off(1, 0);
        t.ready(2, 0);
        assert_eq!(t.want(1, 1), Want::Skip);
        assert!(t.arrived(1));
        assert_eq!(t.want(1, 1), Want::Open, "no waiting for the back-off");
        assert!(!t.arrived(2), "arrival of a device that works changes nothing");
        assert_eq!(t.want(2, 1), Want::Ready);
        assert!(!t.arrived(3));
    }

    #[test]
    fn only_the_first_touchpad_ever_ready_is_announced() {
        let mut t: Table<u8> = Table::default();
        t.off(1, 0);
        t.off(2, 0);
        assert!(t.ready(3, 0), "the first one that works");
        assert!(!t.ready(4, 0));
        assert!(!t.ready(3, 0), "the same again");
        t.removed(3);
        t.removed(4);
        assert!(!t.ready(3, 0), "plugged back in: said already");
        // A PC with no touchpad, or only devices that fail, never announces.
        let mut none: Table<u8> = Table::default();
        none.off(1, 0);
        assert!(!none.announced);
    }

    #[test]
    fn the_registry_verdict() {
        use RegValue::*;
        assert_eq!(handling(&Dword(0), &Dword(0)), Handling::Off);
        assert_eq!(handling(&Dword(1), &Dword(0)), Handling::Still(vec![REG_THREE]));
        assert_eq!(handling(&Dword(0), &Dword(2)), Handling::Still(vec![REG_FOUR]));
        assert_eq!(handling(&Absent, &Absent), Handling::Still(vec![REG_THREE, REG_FOUR]), "absent: Windows' default, on");
        assert!(matches!(handling(&Dword(0), &Unreadable("denied".into())), Handling::Unknown(m) if m.contains(REG_FOUR)));
        // Unreadable is not "still handled" and not "off": a third answer, even next to a setting that is on.
        assert!(matches!(handling(&Dword(1), &Unreadable("denied".into())), Handling::Unknown(_)));
    }
}
