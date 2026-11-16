//! This PC's network interfaces, for the shared link layer: which local addresses a live link can still be
//! on, and what kind of network each interface is (wired links are preferred over Wi-Fi).

use flowseam_core::proto::Kind;
use flowseam_net::ifaces::LocalIf;
use std::net::{IpAddr, Ipv4Addr};

/// One unicast address of an adapter that is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterAddr {
    pub adapter: String,
    /// IANA ifType, as GetAdaptersAddresses reports it.
    pub if_type: u32,
    pub addr: IpAddr,
    pub prefix: u8,
}

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_IEEE80211: u32 = 71;

pub fn kind_of(if_type: u32) -> Kind {
    match if_type {
        IF_TYPE_ETHERNET_CSMACD => Kind::Wired,
        IF_TYPE_IEEE80211 => Kind::WiFi,
        _ => Kind::Other,
    }
}

pub fn mask(prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::MAX.checked_shl(32 - u32::from(prefix.min(32))).unwrap_or(0))
}

/// The interfaces the links rank paths by: every IPv4 address but loopback, with its network's kind.
pub fn interfaces_of(addrs: &[AdapterAddr]) -> Vec<LocalIf> {
    addrs
        .iter()
        .filter_map(|a| match a.addr {
            IpAddr::V4(v4) if !v4.is_loopback() => Some(LocalIf { name: a.adapter.clone(), addr: v4, mask: mask(a.prefix), kind: kind_of(a.if_type) }),
            _ => None,
        })
        .collect()
}

#[cfg(windows)]
pub fn local_interfaces() -> Vec<LocalIf> {
    addresses().map(|a| interfaces_of(&a)).unwrap_or_default()
}

/// Every local address, loopback too; `None` when Windows would not list them.
#[cfg(windows)]
pub fn local_addrs() -> Option<Vec<IpAddr>> {
    addresses().ok().map(|a| a.into_iter().map(|a| a.addr).collect())
}

/// The unicast addresses of every adapter that is up (GetAdaptersAddresses).
#[cfg(windows)]
pub fn addresses() -> Result<Vec<AdapterAddr>, String> {
    use std::ffi::CStr;
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH};
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC};
    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    // u64 words: the adapter records need 8-byte alignment.
    let mut buf: Vec<u64>;
    let mut tries = 0;
    loop {
        buf = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: `buf` is `size` bytes or more, aligned for IP_ADAPTER_ADDRESSES_LH; `size` is in and out.
        let rc = unsafe { GetAdaptersAddresses(u32::from(AF_UNSPEC), flags, std::ptr::null(), buf.as_mut_ptr().cast(), &mut size) };
        match rc {
            0 => break,
            ERROR_BUFFER_OVERFLOW if tries < 3 => tries += 1,
            rc => return Err(format!("GetAdaptersAddresses failed: {rc}")),
        }
    }
    let mut out = Vec::new();
    let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    // SAFETY: a linked list Windows wrote into `buf`, which outlives the walk; each address points into it.
    unsafe {
        while !a.is_null() {
            let ad = &*a;
            if ad.OperStatus == IfOperStatusUp {
                let name = if ad.AdapterName.is_null() { String::new() } else { CStr::from_ptr(ad.AdapterName.cast()).to_string_lossy().into_owned() };
                let mut u = ad.FirstUnicastAddress;
                while !u.is_null() {
                    let ua = &*u;
                    let sa = ua.Address.lpSockaddr;
                    if !sa.is_null() {
                        let bytes = sa as *const u8;
                        let addr = match (*sa).sa_family {
                            AF_INET => {
                                let mut v = [0u8; 4];
                                std::ptr::copy_nonoverlapping(bytes.add(4), v.as_mut_ptr(), 4);
                                Some(IpAddr::from(v))
                            }
                            AF_INET6 => {
                                let mut v = [0u8; 16];
                                std::ptr::copy_nonoverlapping(bytes.add(8), v.as_mut_ptr(), 16);
                                Some(IpAddr::from(v))
                            }
                            _ => None,
                        };
                        if let Some(addr) = addr {
                            out.push(AdapterAddr { adapter: name.clone(), if_type: ad.IfType, addr, prefix: ua.OnLinkPrefixLength });
                        }
                    }
                    u = ua.Next;
                }
            }
            a = ad.Next;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_and_kinds() {
        assert_eq!(mask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(mask(16), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(mask(0), Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(mask(32), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(mask(40), Ipv4Addr::new(255, 255, 255, 255), "an IPv6-sized prefix on a v4 address is the whole address");
        assert_eq!(kind_of(6), Kind::Wired);
        assert_eq!(kind_of(71), Kind::WiFi);
        assert_eq!(kind_of(24), Kind::Other, "loopback");
    }

    #[test]
    fn the_links_see_every_ipv4_address_but_loopback() {
        let a = |adapter: &str, if_type, addr: &str, prefix| AdapterAddr { adapter: adapter.into(), if_type, addr: addr.parse().unwrap(), prefix };
        let all = [a("eth", 6, "192.0.2.61", 24), a("eth", 6, "fe80::1", 64), a("wifi", 71, "198.51.100.7", 22), a("lo", 24, "127.0.0.1", 8), a("lo", 24, "::1", 128)];
        let ifs = interfaces_of(&all);
        assert_eq!(ifs.iter().map(|i| (i.name.as_str(), i.addr.to_string(), i.mask.to_string(), i.kind)).collect::<Vec<_>>(), [
            ("eth", "192.0.2.61".to_string(), "255.255.255.0".to_string(), Kind::Wired),
            ("wifi", "198.51.100.7".to_string(), "255.255.252.0".to_string(), Kind::WiFi),
        ]);
    }

    /// This PC's real adapters: loopback is up, and an address of an up adapter is among the addresses.
    #[cfg(windows)]
    #[test]
    fn real_addresses_include_loopback_and_an_interface() {
        let addrs = local_addrs().expect("Windows lists its addresses");
        assert!(addrs.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)), "{addrs:?}");
        let ifs = local_interfaces();
        assert!(ifs.iter().all(|i| addrs.contains(&IpAddr::V4(i.addr))), "{ifs:?} vs {addrs:?}");
        assert!(!ifs.is_empty(), "a PC on a network has a non-loopback IPv4 address");
    }
}
