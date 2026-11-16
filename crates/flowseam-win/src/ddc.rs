//! Windows DDC/CI access and monitor identity matching.

use flowseam_core::ddc::{EdidId, Reply};

/// A physical monitor exposed through the Windows display configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub id: String,
    pub name: String,
    pub edid: Option<EdidId>,
}

pub fn monitor_id(instance: Option<&str>, edid: Option<EdidId>, adapter: &str, physical_index: usize) -> String {
    if let Some(instance) = instance {
        return format!("instance:{instance}");
    }
    if let Some(id) = edid.filter(|id| id.serial != 0) {
        return format!("edid:{:04X}:{:04X}:{:08X}", id.vendor, id.product, id.serial);
    }
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    adapter.hash(&mut hash);
    format!("unstable:{:016X}:{physical_index}", hash.finish())
}

pub fn openable_id(id: &str) -> bool {
    !id.starts_with("unstable:")
}

/// Convert an EDD device interface path to the SetupDi instance name for the same monitor.
pub fn interface_to_instance_id(path: &str) -> Option<String> {
    let path = path.strip_prefix(r"\\?\")?;
    let (device, guid) = path.rsplit_once("#{")?;
    if !guid.ends_with('}') || device.split('#').count() != 3 {
        return None;
    }
    Some(device.replace('#', "\\").to_ascii_uppercase())
}

/// A full Windows device instance match wins. A model-only match is accepted only when unique.
pub fn match_edid(instance: &str, registry: &[(String, EdidId)]) -> Option<EdidId> {
    let exact: Vec<_> = registry.iter().filter(|(name, _)| name.eq_ignore_ascii_case(instance)).collect();
    if exact.len() == 1 {
        return Some(exact[0].1);
    }
    if !exact.is_empty() {
        return None;
    }
    let model = instance.split('\\').nth(1)?;
    let matches: Vec<_> = registry.iter().filter(|(name, _)| name.split('\\').nth(1).is_some_and(|m| m.eq_ignore_ascii_case(model))).collect();
    (matches.len() == 1).then(|| matches[0].1)
}

/// Physical monitor order is unrelated to EnumDisplayDevicesW order when one HMONITOR has several.
pub fn paired_identity(physical_count: u32, interface_path: &str, registry: &[(String, EdidId)]) -> (Option<String>, Option<EdidId>) {
    if physical_count != 1 {
        return (None, None);
    }
    let instance = interface_to_instance_id(interface_path);
    let edid = instance.as_deref().and_then(|id| match_edid(id, registry));
    (instance, edid)
}

pub fn reply_values(current: u32, max: u32) -> Option<Reply> {
    Some(Reply { current: u16::try_from(current).ok()?, max: u16::try_from(max).ok()? })
}

#[cfg(windows)]
pub mod win {
    use super::{monitor_id, openable_id, paired_identity, reply_values, Monitor};
    use flowseam_core::ddc::{EdidId, Reply};
    use flowseam_core::proto::parse_edid;
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};
    use windows_sys::core::GUID;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiOpenDevRegKey, SP_DEVINFO_DATA, DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DEV};
    use windows_sys::Win32::Devices::Display::{CapabilitiesRequestAndCapabilitiesReply, DestroyPhysicalMonitors, GetCapabilitiesStringLength, GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR, GetVCPFeatureAndVCPFeatureReply, SetVCPFeature, PHYSICAL_MONITOR};
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, DISPLAY_DEVICEW, HDC, HMONITOR, MONITORINFOEXW};
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegQueryValueExW, KEY_READ, REG_BINARY};

    fn error() -> io::Error { io::Error::last_os_error() }

    fn wide_text(value: &[u16]) -> String {
        String::from_utf16_lossy(&value[..value.iter().position(|&c| c == 0).unwrap_or(value.len())])
    }

    /// Registry EDIDs with their SetupDi monitor instance names. The existing EDID parser supplies
    /// the same identity as `edid::present_monitors`, while this path retains device names.
    fn named_edids() -> Vec<(String, EdidId)> {
        let class = GUID { data1: 0x4d36e96e, data2: 0xe325, data3: 0x11ce, data4: [0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18] };
        let mut found = Vec::new();
        // SAFETY: SetupDi handles and registry keys are closed on every path below.
        unsafe {
            let devices = SetupDiGetClassDevsW(&class, null(), null_mut(), DIGCF_PRESENT);
            if devices == INVALID_HANDLE_VALUE as isize { return found; }
            let value = [b'E' as u16, b'D' as u16, b'I' as u16, b'D' as u16, 0];
            for index in 0.. {
                let mut device: SP_DEVINFO_DATA = zeroed();
                device.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
                if SetupDiEnumDeviceInfo(devices, index, &mut device) == 0 { break; }
                let mut name = [0u16; 512];
                if SetupDiGetDeviceInstanceIdW(devices, &device, name.as_mut_ptr(), name.len() as u32, null_mut()) == 0 { continue; }
                let key = SetupDiOpenDevRegKey(devices, &device, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_READ);
                if key == INVALID_HANDLE_VALUE { continue; }
                let mut kind = 0;
                let mut size = 0;
                if RegQueryValueExW(key, value.as_ptr(), null(), &mut kind, null_mut(), &mut size) == ERROR_SUCCESS && kind == REG_BINARY && (128..=32768).contains(&size) {
                    let mut bytes = vec![0u8; size as usize];
                    if RegQueryValueExW(key, value.as_ptr(), null(), &mut kind, bytes.as_mut_ptr(), &mut size) == ERROR_SUCCESS && kind == REG_BINARY && (size as usize) <= bytes.len() {
                        if let Some(id) = parse_edid(&bytes[..size as usize]) {
                            found.push((wide_text(&name), EdidId { vendor: id.vendor.into(), product: id.product.into(), serial: id.serial }));
                        }
                    }
                }
                RegCloseKey(key);
            }
            SetupDiDestroyDeviceInfoList(devices);
        }
        found
    }

    pub struct Display { physical: PHYSICAL_MONITOR }

    impl Display {
        fn handle(&self) -> HANDLE { self.physical.hPhysicalMonitor }

        pub fn get(&self, vcp: u8) -> io::Result<Reply> {
            let (mut code_type, mut current, mut max) = (0, 0, 0);
            // SAFETY: the physical handle is owned by self; output pointers are valid.
            if unsafe { GetVCPFeatureAndVCPFeatureReply(self.handle(), vcp, &mut code_type, &mut current, &mut max) } == 0 { return Err(error()); }
            reply_values(current, max).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "VCP value exceeds 16 bits"))
        }

        /// Writes a VCP value. This method is never called by a test or probe.
        pub fn set(&self, vcp: u8, value: u16) -> io::Result<()> {
            // SAFETY: the physical handle is owned by self and value is a valid DWORD.
            if unsafe { SetVCPFeature(self.handle(), vcp, value.into()) } == 0 { Err(error()) } else { Ok(()) }
        }

        pub fn capabilities(&self) -> io::Result<String> {
            let mut len = 0;
            // SAFETY: the physical handle is owned by self; len is writable.
            if unsafe { GetCapabilitiesStringLength(self.handle(), &mut len) } == 0 { return Err(error()); }
            if len == 0 || len > 65536 { return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid capabilities length")); }
            let mut bytes = vec![0u8; len as usize];
            // SAFETY: bytes holds len writable bytes and the handle is live.
            if unsafe { CapabilitiesRequestAndCapabilitiesReply(self.handle(), bytes.as_mut_ptr(), len) } == 0 { return Err(error()); }
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            Ok(String::from_utf8_lossy(&bytes[..end]).into_owned())
        }
    }

    impl Drop for Display {
        fn drop(&mut self) {
            // SAFETY: this one physical monitor handle is owned by self and destroyed once.
            unsafe { DestroyPhysicalMonitors(1, &self.physical); }
        }
    }

    unsafe extern "system" fn collect_monitor(hmonitor: HMONITOR, _hdc: HDC, _rect: *mut RECT, data: LPARAM) -> i32 {
        // SAFETY: data points to the Vec passed to EnumDisplayMonitors for the duration of this call.
        unsafe { &mut *(data as *mut Vec<HMONITOR>) }.push(hmonitor);
        1
    }

    fn enumerate() -> io::Result<Vec<(Monitor, Display)>> {
        let mut handles = Vec::<HMONITOR>::new();
        // SAFETY: callback data points to handles until enumeration completes.
        if unsafe { EnumDisplayMonitors(null_mut(), null(), Some(collect_monitor), &mut handles as *mut _ as LPARAM) } == 0 { return Err(error()); }
        let edids = named_edids();
        let present = crate::edid::present_monitors();
        let mut found = Vec::new();
        for hmonitor in handles {
            // SAFETY: the output structs are initialized with their required sizes.
            unsafe {
                let mut info: MONITORINFOEXW = zeroed();
                info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
                if GetMonitorInfoW(hmonitor, &mut info.monitorInfo) == 0 { continue; }
                let device_name = wide_text(&info.szDevice);
                let wide_name: Vec<u16> = device_name.encode_utf16().chain(Some(0)).collect();
                let mut count = 0;
                if GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut count) == 0 { continue; }
                if count == 0 || count > 64 { continue; }
                let mut physical = vec![zeroed::<PHYSICAL_MONITOR>(); count as usize];
                if GetPhysicalMonitorsFromHMONITOR(hmonitor, count, physical.as_mut_ptr()) == 0 { continue; }
                for (index, raw) in physical.into_iter().enumerate() {
                    let mut logical: DISPLAY_DEVICEW = zeroed();
                    logical.cb = size_of::<DISPLAY_DEVICEW>() as u32;
                    // EDD_GET_DEVICE_INTERFACE_NAME is 1; its windows-sys constant needs another feature.
                    let listed = count == 1 && EnumDisplayDevicesW(wide_name.as_ptr(), 0, &mut logical, 1) != 0;
                    let device_id = if listed { wide_text(&logical.DeviceID) } else { String::new() };
                    let name = if listed { wide_text(&logical.DeviceString) } else { device_name.clone() };
                    let (instance, edid) = paired_identity(count, &device_id, &edids);
                    let edid = edid
                        .filter(|id| present.iter().any(|p| u32::from(p.vendor) == id.vendor && u32::from(p.product) == id.product && p.serial == id.serial));
                    found.push((Monitor { id: monitor_id(instance.as_deref(), edid, &device_name, index), name, edid }, Display { physical: raw }));
                }
            }
        }
        Ok(found)
    }

    pub fn online() -> io::Result<Vec<Monitor>> {
        Ok(enumerate()?.into_iter().map(|(monitor, _)| monitor).collect())
    }

    pub fn open(id: &str) -> io::Result<Display> {
        if !openable_id(id) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "unstable monitor id cannot be opened safely"));
        }
        enumerate()?.into_iter().find_map(|(monitor, display)| (monitor.id == id).then_some(display))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("monitor {id} is not connected")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edid(serial: u32) -> EdidId { EdidId { vendor: 0x10ac, product: 0x4098, serial } }

    #[test]
    fn monitor_ids_use_instance_then_nonzero_edid_then_mark_unstable() {
        let instance = r"DISPLAY\DEL4098\5&10A58D&0&UID4352";
        assert_eq!(monitor_id(Some(instance), Some(edid(1)), r"\\.\DISPLAY1", 0), format!("instance:{instance}"));
        assert_eq!(monitor_id(None, Some(edid(1)), r"\\.\DISPLAY1", 0), "edid:10AC:4098:00000001");
        let unstable = monitor_id(None, Some(edid(0)), r"\\.\DISPLAY1", 0);
        assert!(unstable.starts_with("unstable:") && !unstable.contains("DISPLAY1"));
        assert!(!openable_id(&monitor_id(None, None, r"\\.\DISPLAY2", 1)));
        assert!(openable_id(&monitor_id(Some(instance), None, r"\\.\DISPLAY2", 1)));
    }

    #[test]
    fn interface_paths_match_the_right_same_model_instance() {
        let registry: Vec<(String, EdidId)> = vec![
            (r"DISPLAY\DEL4098\5&10A58D&0&UID4352".into(), edid(1)),
            (r"DISPLAY\DEL4098\5&10A58D&0&UID4353".into(), edid(2)),
        ];
        let path = |uid| format!(r"\\?\DISPLAY#DEL4098#5&10A58D&0&UID{uid}#{{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}}");
        assert_eq!(interface_to_instance_id(&path(4352)).as_deref(), Some(registry[0].0.as_str()));
        assert_eq!(paired_identity(1, &path(4352), &registry).1, Some(edid(1)));
        assert_eq!(paired_identity(1, &path(4353), &registry).1, Some(edid(2)));
        assert_eq!(paired_identity(1, &path(4354), &registry).1, None);
        assert_eq!(paired_identity(1, &path(4354), &registry[..1]).1, Some(edid(1)), "unique model fallback");
        assert_eq!(interface_to_instance_id(r"MONITOR\DEL4098\OLD"), None);
        assert_eq!(interface_to_instance_id(r"\\?\DISPLAY#DEL4098#UID4352"), None);
    }

    #[test]
    fn mirrored_physical_monitors_are_not_paired_by_undocumented_order() {
        let path = r"\\?\DISPLAY#DEL4098#5&10A58D&0&UID4352#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        let registry = [(r"DISPLAY\DEL4098\5&10A58D&0&UID4352".into(), edid(1))];
        assert_eq!(paired_identity(2, path, &registry), (None, None));
        assert!(monitor_id(None, None, r"\\.\DISPLAY1", 0).starts_with("unstable:"));
    }

    #[test]
    fn vcp_values_must_fit_the_shared_reply_type() {
        assert_eq!(reply_values(18, 21), Some(Reply { current: 18, max: 21 }));
        assert_eq!(reply_values(65536, 21), None);
        assert_eq!(reply_values(18, 65536), None);
    }
}
