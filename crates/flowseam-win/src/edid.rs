use flowseam_core::proto::MonitorId;

#[cfg(windows)]
pub fn present_monitors() -> Vec<MonitorId> {
    use flowseam_core::proto::parse_edid;
    use windows_sys::core::GUID;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiOpenDevRegKey, SP_DEVINFO_DATA, DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DEV};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegQueryValueExW, KEY_READ, REG_BINARY};

    let class = GUID { data1: 0x4d36e96e, data2: 0xe325, data3: 0x11ce, data4: [0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18] };
    let mut found = Vec::new();
    unsafe {
        let devices = SetupDiGetClassDevsW(&class, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT);
        // windows-sys 0.59 types HDEVINFO as isize, not as the HANDLE pointer.
        if devices == INVALID_HANDLE_VALUE as isize {
            flowseam_core::log::line(&format!("flowseam: monitor enumeration failed: {}", GetLastError()));
            return found;
        }
        let value: [u16; 5] = [b'E' as u16, b'D' as u16, b'I' as u16, b'D' as u16, 0];
        let mut index = 0;
        loop {
            let mut device: SP_DEVINFO_DATA = std::mem::zeroed();
            device.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(devices, index, &mut device) == 0 {
                let error = GetLastError();
                if error != ERROR_NO_MORE_ITEMS {
                    flowseam_core::log::line(&format!("flowseam: monitor enumeration failed: {error}"));
                }
                break;
            }
            index += 1;
            let key = SetupDiOpenDevRegKey(devices, &device, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_READ);
            if key == INVALID_HANDLE_VALUE { continue; }
            let mut kind = 0;
            let mut size = 0;
            if RegQueryValueExW(key, value.as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut size) == ERROR_SUCCESS && kind == REG_BINARY && (128..=32768).contains(&size) {
                let mut bytes = vec![0; size as usize];
                if RegQueryValueExW(key, value.as_ptr(), std::ptr::null(), &mut kind, bytes.as_mut_ptr(), &mut size) == ERROR_SUCCESS && kind == REG_BINARY && (size as usize) <= bytes.len() {
                    if let Some(id) = parse_edid(&bytes[..size as usize]) {
                        if !found.contains(&id) { found.push(id); }
                    }
                }
            }
            RegCloseKey(key);
        }
        SetupDiDestroyDeviceInfoList(devices);
    }
    found
}

#[cfg(not(windows))]
pub fn present_monitors() -> Vec<MonitorId> { Vec::new() }

#[cfg(all(test, windows))]
mod tests {
    /// Real hardware: set FLOWSEAM_EDID_EXPECT to `vendor:product:serial` in hex of a monitor attached here.
    #[test]
    #[ignore = "needs an attached monitor: set FLOWSEAM_EDID_EXPECT=vvvv:pppp:ssssssss (hex)"]
    fn an_attached_monitor_is_listed() {
        let want = std::env::var("FLOWSEAM_EDID_EXPECT").expect("FLOWSEAM_EDID_EXPECT");
        let found = super::present_monitors();
        let listed: Vec<String> = found.iter().map(|m| format!("{:04x}:{:04x}:{:08x}", m.vendor, m.product, m.serial)).collect();
        println!("present monitors: {listed:?}");
        assert!(listed.contains(&want.to_ascii_lowercase()), "{want} not in {listed:?}");
    }
}
