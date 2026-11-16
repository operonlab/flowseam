//! DDC/CI monitor control: read and set MCCS VCP features (input source 0x60, luminance 0x10)
//! over Apple Silicon's private IOAVService I²C path. Groundwork for switching a monitor's input
//! together with the keyboard and mouse.
//!
//! Frame bytes, checksum seed, write repetition and waits, and the display → AV-service lookup
//! are ported from m1ddc (MIT, github.com/waydabber/m1ddc, `sources/i2c.m`,
//! `sources/ioregistry.m`). One deliberate difference: m1ddc decodes whatever comes back, so a
//! failed read prints `0`. Here a reply is validated, and a failed read is an error, never a value.
//!
//! Frames and the readback policy are pure and compile everywhere; `mac` holds the I/O.

pub use flowseam_core::ddc::*;

#[cfg(target_os = "macos")]
pub mod mac {
    use std::time::Duration;
    use super::*;
    use crate::ffi::{kCFStringEncodingUTF8, CFDictionaryGetValue, CFRelease, CFStringCreateWithCString, CFTypeRef, CGDirectDisplayID};
    use std::ffi::{c_char, c_void, CStr};
    use std::sync::OnceLock;
    use std::thread::sleep;

    const CHIP_DEFAULT: u32 = 0x37;
    /// MCDP29xx DisplayPort bridges route DDC through 0xB7.
    const CHIP_MCDP29XX: u32 = 0xB7;
    const DDC_WAIT: Duration = Duration::from_millis(10);
    /// m1ddc: 10 ms returned empty MCDP29xx replies.
    const MCDP_READ_WAIT: Duration = Duration::from_millis(50);
    const CAPS_WAIT: Duration = Duration::from_millis(50);
    /// Every frame is sent twice, as m1ddc does; some displays drop a single write.
    const WRITE_REPEATS: u32 = 2;
    const MAX_DISPLAYS: usize = 16;

    type IoObject = u32;
    const MAIN_PORT_DEFAULT: u32 = 0;
    const ITERATE_RECURSIVELY: u32 = 1;
    const SERVICE_PLANE: &CStr = c"IOService";

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IORegistryEntryCopyFromPath(main_port: u32, path: CFTypeRef) -> IoObject;
        fn IORegistryEntryGetRegistryEntryID(entry: IoObject, id: *mut u64) -> i32;
        fn IORegistryGetRootEntry(main_port: u32) -> IoObject;
        fn IORegistryEntryCreateIterator(entry: IoObject, plane: *const c_char, options: u32, iter: *mut IoObject) -> i32;
        fn IOIteratorNext(iter: IoObject) -> IoObject;
        fn IOObjectConformsTo(obj: IoObject, class: *const c_char) -> i32;
        fn IORegistryEntryGetName(entry: IoObject, name: *mut c_char) -> i32;
        fn IORegistryEntrySearchCFProperty(entry: IoObject, plane: *const c_char, key: CFTypeRef, alloc: *const c_void, options: u32) -> CFTypeRef;
        fn IORegistryEntryCreateCFProperty(entry: IoObject, key: CFTypeRef, alloc: *const c_void, options: u32) -> CFTypeRef;
        fn IORegistryEntryGetParentEntry(entry: IoObject, plane: *const c_char, parent: *mut IoObject) -> i32;
        fn IOObjectRelease(obj: IoObject) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        // Returns Boolean (u8), declared as in `ifaces.rs`.
        fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, len: isize, encoding: u32) -> u8;
        fn CFGetTypeID(cf: CFTypeRef) -> usize;
        fn CFStringGetTypeID() -> usize;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGGetOnlineDisplayList(max: u32, displays: *mut CGDirectDisplayID, count: *mut u32) -> i32;
        fn CGDisplayVendorNumber(display: CGDirectDisplayID) -> u32;
        fn CGDisplayModelNumber(display: CGDirectDisplayID) -> u32;
        fn CGDisplaySerialNumber(display: CGDirectDisplayID) -> u32;
    }

    /// An online display: its CoreGraphics UUID, its EDID name, and its EDID identity.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Monitor {
        pub id: CGDirectDisplayID,
        pub uuid: String,
        pub name: Option<String>,
        pub edid: EdidId,
    }

    type CreateWithService = unsafe extern "C" fn(*const c_void, IoObject) -> CFTypeRef;
    type WriteI2c = unsafe extern "C" fn(CFTypeRef, u32, u32, *const c_void, u32) -> i32;
    type ReadI2c = unsafe extern "C" fn(CFTypeRef, u32, u32, *mut c_void, u32) -> i32;
    type InfoDictionary = unsafe extern "C" fn(CGDirectDisplayID) -> CFTypeRef;

    /// Private symbols, looked up at run time so a macOS that drops one reports which.
    struct Private {
        create: CreateWithService,
        write: WriteI2c,
        read: ReadI2c,
        info: InfoDictionary,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum OpenError {
        MissingSymbol(&'static str),
        /// `CGGetOnlineDisplayList` failed with this CGError.
        DisplayList(i32),
        /// No online display has this UUID.
        NotFound,
        /// The display has no external DCPAVServiceProxy: a built-in panel, or a virtual or
        /// AirPlay display.
        NoAvService,
    }

    fn private() -> Result<&'static Private, OpenError> {
        static P: OnceLock<Result<Private, OpenError>> = OnceLock::new();
        P.get_or_init(|| unsafe {
            // Load it explicitly rather than rely on something else in the process having done so.
            libc::dlopen(c"/System/Library/Frameworks/CoreDisplay.framework/CoreDisplay".as_ptr(), libc::RTLD_LAZY);
            let sym = |name: &'static CStr| {
                let p = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
                if p.is_null() { Err(OpenError::MissingSymbol(name.to_str().unwrap_or("?"))) } else { Ok(p) }
            };
            Ok(Private {
                create: std::mem::transmute::<*mut c_void, CreateWithService>(sym(c"IOAVServiceCreateWithService")?),
                write: std::mem::transmute::<*mut c_void, WriteI2c>(sym(c"IOAVServiceWriteI2C")?),
                read: std::mem::transmute::<*mut c_void, ReadI2c>(sym(c"IOAVServiceReadI2C")?),
                info: std::mem::transmute::<*mut c_void, InfoDictionary>(sym(c"CoreDisplay_DisplayCreateInfoDictionary")?),
            })
        })
        .as_ref()
        .map_err(Clone::clone)
    }

    /// An open DDC channel to one external display.
    pub struct Display {
        pub id: CGDirectDisplayID,
        service: CFTypeRef,
        chip: u32,
        p: &'static Private,
    }

    impl Drop for Display {
        fn drop(&mut self) {
            // SAFETY: `service` came from IOAVServiceCreateWithService (a +1 reference) and is released once.
            unsafe { CFRelease(self.service) }
        }
    }

    impl Display {
        pub fn get(&self, vcp: u8) -> Result<Reply, DdcError> {
            parse_reply(vcp, &self.get_raw(vcp)?)
        }

        /// The unparsed reply bytes, for diagnosing a display whose replies do not parse.
        pub fn get_raw(&self, vcp: u8) -> Result<[u8; REPLY_LEN], DdcError> {
            self.write(&get_request(vcp))?;
            sleep(if self.chip == CHIP_MCDP29XX { MCDP_READ_WAIT } else { DDC_WAIT });
            let mut b = [0u8; REPLY_LEN];
            // SAFETY: `b` is REPLY_LEN writable bytes; `service` is live for `self`'s lifetime.
            let ret = unsafe { (self.p.read)(self.service, self.chip, SUB_ADDR.into(), b.as_mut_ptr().cast(), REPLY_LEN as u32) };
            if ret != 0 {
                return Err(DdcError::Io(ret));
            }
            Ok(b)
        }

        pub fn set(&self, vcp: u8, value: u16) -> Result<(), DdcError> {
            self.write(&set_request(vcp, value))
        }

        /// The MCCS capabilities string, fragment by fragment. A fragment that fails is asked for again,
        /// up to three times (ddc-rs waits 40 ms for a reply and 50 ms between commands; this waits 50 ms).
        pub fn capabilities(&self) -> Result<String, DdcError> {
            let mut caps = Vec::new();
            loop {
                let offset = caps.len() as u16;
                let mut last = DdcError::Io(0);
                let fragment = (0..3).find_map(|_| match self.caps_fragment(offset) {
                    Ok(data) => Some(data),
                    Err(e) => {
                        last = e;
                        sleep(CAPS_WAIT);
                        None
                    }
                });
                let Some(data) = fragment else { return Err(last) };
                if data.is_empty() {
                    break;
                }
                caps.extend(data);
                if caps.len() > CAPS_MAX {
                    return Err(DdcError::CapsTooLong);
                }
                sleep(CAPS_WAIT);
            }
            Ok(String::from_utf8_lossy(&caps).trim_end_matches('\0').to_string())
        }

        fn caps_fragment(&self, offset: u16) -> Result<Vec<u8>, DdcError> {
            parse_caps_reply(offset, &self.caps_raw(offset)?)
        }

        /// One unparsed capabilities fragment, for diagnosing a display whose replies do not parse.
        pub fn caps_raw(&self, offset: u16) -> Result<[u8; CAPS_REPLY_LEN], DdcError> {
            self.write(&caps_request(offset))?;
            sleep(CAPS_WAIT);
            let mut b = [0u8; CAPS_REPLY_LEN];
            // SAFETY: `b` is CAPS_REPLY_LEN writable bytes; `service` is live for `self`'s lifetime.
            let ret = unsafe { (self.p.read)(self.service, self.chip, SUB_ADDR.into(), b.as_mut_ptr().cast(), CAPS_REPLY_LEN as u32) };
            if ret != 0 {
                return Err(DdcError::Io(ret));
            }
            Ok(b)
        }

        fn write(&self, frame: &[u8]) -> Result<(), DdcError> {
            for _ in 0..WRITE_REPEATS {
                sleep(DDC_WAIT);
                // SAFETY: `frame` is readable for its length; `service` is live for `self`'s lifetime.
                let ret = unsafe { (self.p.write)(self.service, self.chip, SUB_ADDR.into(), frame.as_ptr().cast(), frame.len() as u32) };
                if ret != 0 {
                    return Err(DdcError::Io(ret));
                }
            }
            Ok(())
        }
    }

    /// CoreGraphics UUIDs of the online displays CoreDisplay describes, in m1ddc's `list` order.
    pub fn online() -> Result<Vec<Monitor>, OpenError> {
        let p = private()?;
        Ok(online_ids()?
            .into_iter()
            .filter_map(|id| {
                identify(p, id).map(|(uuid, adapter)| {
                    let name = product_name(adapter);
                    // SAFETY: `adapter` is a +1 registry reference from `identify`.
                    unsafe { IOObjectRelease(adapter) };
                    // SAFETY: plain queries on a display id CoreGraphics just listed.
                    let edid = unsafe { EdidId { vendor: CGDisplayVendorNumber(id), product: CGDisplayModelNumber(id), serial: CGDisplaySerialNumber(id) } };
                    Monitor { id, uuid, name, edid }
                })
            })
            .collect())
    }

    /// The UUID of every display online here (active, mirrored or asleep), in the form `open` takes;
    /// `None` for one CoreDisplay does not describe, so a caller can tell "not ours" from "unknown".
    pub fn online_uuids() -> Result<Vec<Option<String>>, OpenError> {
        let p = private()?;
        Ok(online_ids()?
            .into_iter()
            .map(|id| {
                identify(p, id).map(|(uuid, adapter)| {
                    // SAFETY: `adapter` is a +1 registry reference from `identify`.
                    unsafe { IOObjectRelease(adapter) };
                    uuid
                })
            })
            .collect())
    }

    /// The EDID name the framebuffer reports ("BenQ EW2780Q"), from `DisplayAttributes` (m1ddc reads it the same way).
    fn product_name(adapter: IoObject) -> Option<String> {
        // SAFETY: the keys and the attributes dictionary are +1 references released here; the name is
        // copied out before the release.
        unsafe {
            let key = cfstr(c"DisplayAttributes");
            let attrs = IORegistryEntrySearchCFProperty(adapter, SERVICE_PLANE.as_ptr(), key, std::ptr::null(), ITERATE_RECURSIVELY);
            CFRelease(key);
            if attrs.is_null() {
                return None;
            }
            let (k_product, k_name) = (cfstr(c"ProductAttributes"), cfstr(c"ProductName"));
            let product = CFDictionaryGetValue(attrs, k_product);
            let name = if product.is_null() { None } else { to_string(CFDictionaryGetValue(product, k_name)) };
            CFRelease(k_product);
            CFRelease(k_name);
            CFRelease(attrs);
            name
        }
    }

    /// Opens the display whose CoreGraphics UUID is `uuid` (what `m1ddc display list` prints).
    pub fn open(uuid: &str) -> Result<Display, OpenError> {
        let p = private()?;
        for id in online_ids()? {
            let Some((found, adapter)) = identify(p, id) else { continue };
            let service = found.eq_ignore_ascii_case(uuid).then(|| av_service(p, adapter));
            // SAFETY: `adapter` is a +1 registry reference from `identify`.
            unsafe { IOObjectRelease(adapter) };
            match service {
                None => continue,
                Some(None) => return Err(OpenError::NoAvService),
                Some(Some((service, chip))) => return Ok(Display { id, service, chip, p }),
            }
        }
        Err(OpenError::NotFound)
    }

    fn online_ids() -> Result<Vec<CGDirectDisplayID>, OpenError> {
        let mut ids = [0; MAX_DISPLAYS];
        let mut n = 0u32;
        // SAFETY: `ids` holds MAX_DISPLAYS entries and the call is told so.
        let err = unsafe { CGGetOnlineDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut n) };
        if err != 0 {
            return Err(OpenError::DisplayList(err));
        }
        Ok(ids[..n as usize].to_vec())
    }

    /// The display's CoreGraphics UUID and its framebuffer's registry entry (+1, caller releases),
    /// from CoreDisplay's info dictionary. `None` for displays it does not describe.
    fn identify(p: &Private, id: CGDirectDisplayID) -> Option<(String, IoObject)> {
        // SAFETY: the info dictionary is a +1 reference released here; values read from it are
        // used before that release.
        unsafe {
            let info = (p.info)(id);
            if info.is_null() {
                return None;
            }
            let (k_uuid, k_location) = (cfstr(c"kCGDisplayUUID"), cfstr(c"IODisplayLocation"));
            let uuid = to_string(CFDictionaryGetValue(info, k_uuid));
            let location = CFDictionaryGetValue(info, k_location);
            let adapter = if location.is_null() { 0 } else { IORegistryEntryCopyFromPath(MAIN_PORT_DEFAULT, location) };
            CFRelease(k_uuid);
            CFRelease(k_location);
            CFRelease(info);
            match uuid {
                Some(uuid) if adapter != 0 => Some((uuid, adapter)),
                _ => {
                    if adapter != 0 {
                        IOObjectRelease(adapter);
                    }
                    None
                }
            }
        }
    }

    /// Walks the service plane in order; the external DCPAVServiceProxy that follows the display's
    /// framebuffer is its AV service (m1ddc `getDisplayDDCTransport`).
    fn av_service(p: &Private, adapter: IoObject) -> Option<(CFTypeRef, u32)> {
        // SAFETY: every registry object obtained here is released exactly once before returning.
        unsafe {
            let mut want = 0u64;
            if IORegistryEntryGetRegistryEntryID(adapter, &mut want) != 0 {
                return None;
            }
            let root = IORegistryGetRootEntry(MAIN_PORT_DEFAULT);
            let mut iter: IoObject = 0;
            let ret = IORegistryEntryCreateIterator(root, SERVICE_PLANE.as_ptr(), ITERATE_RECURSIVELY, &mut iter);
            IOObjectRelease(root);
            if ret != 0 {
                return None;
            }
            let mut after_framebuffer = false;
            let mut found = None;
            loop {
                let s = IOIteratorNext(iter);
                if s == 0 {
                    break;
                }
                if IOObjectConformsTo(s, c"IOMobileFramebuffer".as_ptr()) != 0 {
                    let mut id = 0u64;
                    after_framebuffer = IORegistryEntryGetRegistryEntryID(s, &mut id) == 0 && id == want;
                } else if after_framebuffer && name(s).as_deref() == Some("DCPAVServiceProxy") && is_external(s) {
                    let service = (p.create)(std::ptr::null(), s);
                    if !service.is_null() {
                        found = Some((service, if is_mcdp29xx(s) { CHIP_MCDP29XX } else { CHIP_DEFAULT }));
                    }
                }
                IOObjectRelease(s);
                if found.is_some() {
                    break;
                }
            }
            IOObjectRelease(iter);
            found
        }
    }

    fn name(s: IoObject) -> Option<String> {
        let mut buf = [0 as c_char; 128]; // io_name_t
        // SAFETY: `buf` is an io_name_t-sized buffer; on success it holds a NUL-terminated name.
        unsafe { (IORegistryEntryGetName(s, buf.as_mut_ptr()) == 0).then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()) }
    }

    fn is_external(s: IoObject) -> bool {
        // SAFETY: the key and the property value are +1 references released here.
        unsafe {
            let key = cfstr(c"Location");
            let v = IORegistryEntrySearchCFProperty(s, SERVICE_PLANE.as_ptr(), key, std::ptr::null(), ITERATE_RECURSIVELY);
            CFRelease(key);
            let external = to_string(v).as_deref() == Some("External");
            if !v.is_null() {
                CFRelease(v);
            }
            external
        }
    }

    fn is_mcdp29xx(s: IoObject) -> bool {
        // SAFETY: the parent, the key and the property value are +1 references released here.
        unsafe {
            let mut parent: IoObject = 0;
            if IORegistryEntryGetParentEntry(s, SERVICE_PLANE.as_ptr(), &mut parent) != 0 {
                return false;
            }
            let key = cfstr(c"EPICProviderClass");
            let v = IORegistryEntryCreateCFProperty(parent, key, std::ptr::null(), 0);
            CFRelease(key);
            IOObjectRelease(parent);
            let yes = to_string(v).as_deref() == Some("AppleDCPMCDP29XX");
            if !v.is_null() {
                CFRelease(v);
            }
            yes
        }
    }

    /// A +1 CFString; the caller releases it.
    fn cfstr(s: &CStr) -> CFTypeRef {
        // SAFETY: `s` is NUL-terminated UTF-8.
        unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), kCFStringEncodingUTF8) }
    }

    fn to_string(s: CFTypeRef) -> Option<String> {
        // SAFETY: `s` is checked to be a CFString before it is read into a bounded buffer.
        unsafe {
            if s.is_null() || CFGetTypeID(s) != CFStringGetTypeID() {
                return None;
            }
            let mut buf = [0 as c_char; 256];
            (CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, kCFStringEncodingUTF8) != 0)
                .then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Read-only against real hardware: never switches the input.
        #[test]
        #[ignore = "needs a DDC display: set FLOWSEAM_DDC_UUID and FLOWSEAM_DDC_EXPECT_INPUT"]
        fn live_readback() {
            let uuid = std::env::var("FLOWSEAM_DDC_UUID").expect("FLOWSEAM_DDC_UUID");
            let want: u16 = std::env::var("FLOWSEAM_DDC_EXPECT_INPUT").expect("FLOWSEAM_DDC_EXPECT_INPUT").parse().expect("a number");
            let d = open(&uuid).expect("open");
            let input = confirm(|| d.get(VCP_INPUT).map(|r| r.current), want, 5, || sleep(Duration::from_millis(200)));
            assert_eq!(input, Readback::Confirmed(want));
            let lum = d.get(VCP_LUMINANCE).expect("luminance");
            assert!(lum.max > 0 && lum.current <= lum.max, "{lum:?}");
        }
    }
}
