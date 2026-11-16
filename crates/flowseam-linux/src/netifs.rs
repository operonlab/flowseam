//! This machine's network interfaces for the shared link layer: which local addresses a live link can still
//! be on, and what kind of network each interface is (wired links are preferred over Wi-Fi).

use flowseam_core::proto::Kind;
use flowseam_net::ifaces::LocalIf;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

/// One address of an interface, as getifaddrs reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfAddr {
    pub name: String,
    pub addr: IpAddr,
    /// The netmask, when the interface has one for this address.
    pub mask: Option<IpAddr>,
}

pub const SYS_CLASS_NET: &str = "/sys/class/net";

/// Wi-Fi when sysfs shows a wireless interface; Wired for an Ethernet-type link (`type` = 1, ARPHRD_ETHER)
/// backed by hardware (`device` exists); else Other. Bridges, veth pairs and docker0 are Ethernet-typed but
/// have no `device`, so they are Other. An alias label (`eth0:1`) is read as its interface (`eth0`).
pub fn kind_of(sys_class_net: &Path, name: &str) -> Kind {
    let dir = sys_class_net.join(name.split(':').next().unwrap_or(name));
    if dir.join("wireless").exists() || dir.join("phy80211").exists() {
        return Kind::WiFi;
    }
    if !dir.join("device").exists() {
        return Kind::Other;
    }
    match std::fs::read_to_string(dir.join("type")) {
        Ok(t) if t.trim() == "1" => Kind::Wired,
        _ => Kind::Other,
    }
}

/// An interface whose addresses can carry traffic: administratively up and with a carrier. An unplugged cable
/// clears IFF_RUNNING, so its addresses drop out of the list and a link on it is judged lost.
#[cfg(target_os = "linux")]
pub fn is_live(flags: u32) -> bool {
    let want = (libc::IFF_UP | libc::IFF_RUNNING) as u32;
    flags & want == want
}

/// The v4 netmask, or the single-address mask when the interface reports none (point-to-point links).
fn v4_mask(m: Option<IpAddr>) -> Ipv4Addr {
    match m {
        Some(IpAddr::V4(m)) => m,
        _ => Ipv4Addr::BROADCAST,
    }
}

/// The interfaces the links rank paths by: every IPv4 address but loopback, with its network's kind.
pub fn interfaces_of(addrs: &[IfAddr], sys_class_net: &Path) -> Vec<LocalIf> {
    addrs
        .iter()
        .filter_map(|a| match a.addr {
            IpAddr::V4(v4) if !v4.is_loopback() => Some(LocalIf { name: a.name.clone(), addr: v4, mask: v4_mask(a.mask), kind: kind_of(sys_class_net, &a.name) }),
            _ => None,
        })
        .collect()
}

/// Every address of every live interface, loopback included. `Err` when getifaddrs fails (or the host
/// is not Linux), which is not the same thing as an empty list.
#[cfg(target_os = "linux")]
pub fn addresses() -> Result<Vec<IfAddr>, String> {
    use std::ffi::CStr;
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `head` is an out-pointer; on success the list is freed below with freeifaddrs.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(format!("getifaddrs failed: {}", std::io::Error::last_os_error()));
    }
    let mut out = Vec::new();
    let mut cur = head;
    // SAFETY: a list getifaddrs built, alive until freeifaddrs; every pointer is checked before use.
    unsafe {
        while !cur.is_null() {
            let ifa = &*cur;
            if is_live(ifa.ifa_flags as u32) && !ifa.ifa_addr.is_null() {
                let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                if let Some(addr) = sockaddr_ip(ifa.ifa_addr) {
                    out.push(IfAddr { name, addr, mask: if ifa.ifa_netmask.is_null() { None } else { sockaddr_ip(ifa.ifa_netmask) } });
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    Ok(out)
}

/// # Safety
/// `sa` points to a valid sockaddr.
#[cfg(target_os = "linux")]
unsafe fn sockaddr_ip(sa: *const libc::sockaddr) -> Option<IpAddr> {
    match (*sa).sa_family as i32 {
        libc::AF_INET => {
            let s = &*(sa as *const libc::sockaddr_in);
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(s.sin_addr.s_addr))))
        }
        libc::AF_INET6 => {
            let s = &*(sa as *const libc::sockaddr_in6);
            Some(IpAddr::V6(s.sin6_addr.s6_addr.into()))
        }
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn addresses() -> Result<Vec<IfAddr>, String> {
    Err("listing interfaces is implemented for Linux only".into())
}

/// Every local address, loopback too; `None` when they cannot be listed (never an empty list for a failure).
pub fn local_addrs() -> Option<Vec<IpAddr>> {
    addresses().ok().map(|a| a.into_iter().map(|a| a.addr).collect())
}

pub fn local_interfaces() -> Vec<LocalIf> {
    addresses().map(|a| interfaces_of(&a, Path::new(SYS_CLASS_NET))).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fake_sys(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("flowseam-linux-sys-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mk = |name: &str, ty: &str, extra: Option<&str>| {
            let d = root.join(name);
            fs::create_dir_all(&d).unwrap();
            if !["lo", "tun0", "docker0", "veth1a2b", "virbr0", "br0"].contains(&name) {
                fs::create_dir_all(d.join("device")).unwrap();
            }
            fs::write(d.join("type"), format!("{ty}\n")).unwrap();
            if let Some(x) = extra {
                fs::create_dir_all(d.join(x)).unwrap();
            }
        };
        mk("enp3s0", "1", None);
        mk("wlp2s0", "1", Some("wireless"));
        mk("wlan9", "1", Some("phy80211"));
        mk("lo", "772", None);
        mk("tun0", "65534", None);
        mk("docker0", "1", None);
        mk("veth1a2b", "1", None);
        mk("virbr0", "1", None);
        mk("br0", "1", None);
        root
    }

    #[test]
    fn kinds_come_from_sysfs() {
        let root = fake_sys("kinds");
        assert_eq!(kind_of(&root, "enp3s0"), Kind::Wired);
        assert_eq!(kind_of(&root, "wlp2s0"), Kind::WiFi, "wireless dir beats type 1");
        assert_eq!(kind_of(&root, "wlan9"), Kind::WiFi, "phy80211 alone is enough");
        assert_eq!(kind_of(&root, "lo"), Kind::Other);
        assert_eq!(kind_of(&root, "tun0"), Kind::Other);
        assert_eq!(kind_of(&root, "gone0"), Kind::Other, "no sysfs entry is Other, not a panic");
        for virt in ["docker0", "veth1a2b", "virbr0", "br0"] {
            assert_eq!(kind_of(&root, virt), Kind::Other, "{virt} is Ethernet-typed but has no hardware behind it");
        }
        assert_eq!(kind_of(&root, "enp3s0:1"), Kind::Wired, "an alias label reads its interface");
        assert_eq!(kind_of(&root, "wlp2s0:0"), Kind::WiFi);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_links_see_every_ipv4_address_but_loopback() {
        let root = fake_sys("ifs");
        let a = |name: &str, addr: &str, mask: Option<&str>| IfAddr { name: name.into(), addr: addr.parse().unwrap(), mask: mask.map(|m| m.parse().unwrap()) };
        let all = [
            a("enp3s0", "192.0.2.61", Some("255.255.255.0")),
            a("enp3s0", "fe80::1", Some("ffff:ffff:ffff:ffff::")),
            a("wlp2s0", "198.51.100.7", Some("255.255.252.0")),
            a("lo", "127.0.0.1", Some("255.0.0.0")),
            a("lo", "::1", None),
            a("tun0", "10.9.0.2", None),
            a("docker0", "172.17.0.1", Some("255.255.0.0")),
            a("enp3s0:1", "192.0.2.62", Some("255.255.255.0")),
        ];
        let ifs = interfaces_of(&all, &root);
        assert_eq!(ifs.iter().map(|i| (i.name.as_str(), i.addr.to_string(), i.mask.to_string(), i.kind)).collect::<Vec<_>>(), [
            ("enp3s0", "192.0.2.61".to_string(), "255.255.255.0".to_string(), Kind::Wired),
            ("wlp2s0", "198.51.100.7".to_string(), "255.255.252.0".to_string(), Kind::WiFi),
            ("tun0", "10.9.0.2".to_string(), "255.255.255.255".to_string(), Kind::Other),
            ("docker0", "172.17.0.1".to_string(), "255.255.0.0".to_string(), Kind::Other),
            ("enp3s0:1", "192.0.2.62".to_string(), "255.255.255.0".to_string(), Kind::Wired),
        ]);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_interfaces_with_a_carrier_are_live() {
        let (up, running) = ((libc::IFF_UP) as u32, (libc::IFF_RUNNING) as u32);
        assert!(is_live(up | running | libc::IFF_BROADCAST as u32));
        assert!(!is_live(up), "up with the cable out");
        assert!(!is_live(running));
        assert!(!is_live(0));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn off_linux_listing_fails_loudly_not_empty() {
        assert!(addresses().is_err());
        assert_eq!(local_addrs(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_addresses_include_loopback() {
        let addrs = local_addrs().expect("getifaddrs works");
        assert!(addrs.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)), "{addrs:?}");
        let ifs = local_interfaces();
        assert!(ifs.iter().all(|i| addrs.contains(&IpAddr::V4(i.addr))), "{ifs:?} vs {addrs:?}");
        assert!(ifs.iter().all(|i| !i.addr.is_loopback()));
    }
}
