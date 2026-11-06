//! Active macOS IPv4 interfaces and their SystemConfiguration kinds.
use flowseam_core::proto::Kind;
use std::ffi::{c_void, CStr};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[allow(dead_code)]
pub struct LocalIf { pub name: String, pub addr: Ipv4Addr, pub mask: Ipv4Addr, pub kind: Kind }

type CFRef = *const c_void;
#[repr(C, packed(4))]
struct IfMediaReq {
    name: [libc::c_char; libc::IFNAMSIZ],
    current: libc::c_int,
    mask: libc::c_int,
    status: libc::c_int,
    active: libc::c_int,
    count: libc::c_int,
    list: *mut libc::c_int,
}
const IFM_AVALID: libc::c_int = 1;
const IFM_ACTIVE: libc::c_int = 2;

fn media_active(fd: libc::c_int, name: &CStr) -> bool {
    if fd < 0 { return true; }
    let mut req = IfMediaReq { name: [0; libc::IFNAMSIZ], current: 0, mask: 0, status: 0, active: 0, count: 0, list: std::ptr::null_mut() };
    let bytes = name.to_bytes_with_nul();
    if bytes.len() > req.name.len() { return true; }
    for (dst, src) in req.name.iter_mut().zip(bytes) { *dst = *src as libc::c_char; }
    if unsafe { libc::ioctl(fd, libc::SIOCGIFMEDIA, &mut req) } != 0 { return true; }
    req.status & (IFM_AVALID | IFM_ACTIVE) == IFM_AVALID | IFM_ACTIVE
}
#[link(name = "SystemConfiguration", kind = "framework")]
extern "C" {
    fn SCNetworkInterfaceCopyAll() -> CFRef;
    fn SCNetworkInterfaceGetBSDName(interface: CFRef) -> CFRef;
    fn SCNetworkInterfaceGetInterfaceType(interface: CFRef) -> CFRef;
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFArrayGetCount(array: CFRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFRef, index: isize) -> CFRef;
    fn CFStringGetCString(string: CFRef, buffer: *mut i8, size: isize, encoding: u32) -> u8;
    fn CFRelease(value: CFRef);
}

fn cf_string(s: CFRef) -> Option<String> {
    if s.is_null() { return None; }
    let mut buf = [0i8; 256];
    if unsafe { CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, 0x0800_0100) } == 0 { return None; }
    Some(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

pub fn kind_for_type(t: &str) -> Option<Kind> {
    match t { "Ethernet" | "Bridge" => Some(Kind::Wired), "IEEE80211" => Some(Kind::WiFi), _ => None }
}

pub fn local_interfaces() -> Vec<LocalIf> {
    let mut kinds = std::collections::HashMap::new();
    unsafe {
        let all = SCNetworkInterfaceCopyAll();
        if !all.is_null() {
            for i in 0..CFArrayGetCount(all) {
                let iface = CFArrayGetValueAtIndex(all, i);
                if let (Some(name), Some(t)) = (cf_string(SCNetworkInterfaceGetBSDName(iface)), cf_string(SCNetworkInterfaceGetInterfaceType(iface))) {
                    if let Some(kind) = kind_for_type(&t) { kinds.insert(name, kind); }
                }
            }
            CFRelease(all);
        }
    }
    let mut out = Vec::new();
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        let mut first: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut first) != 0 { if fd >= 0 { libc::close(fd); } return out; }
        let mut cur = first;
        while !cur.is_null() {
            let ifa = &*cur;
            if ifa.ifa_flags & (libc::IFF_UP | libc::IFF_LOOPBACK) as u32 == libc::IFF_UP as u32 && !ifa.ifa_addr.is_null() && !ifa.ifa_netmask.is_null() && (*ifa.ifa_addr).sa_family == libc::AF_INET as u8 && media_active(fd, CStr::from_ptr(ifa.ifa_name)) {
                let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                if let Some(&kind) = kinds.get(&name) {
                    let addr = (*(ifa.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr;
                    let mask = (*(ifa.ifa_netmask as *const libc::sockaddr_in)).sin_addr.s_addr;
                    out.push(LocalIf { name, addr: Ipv4Addr::from(addr.to_ne_bytes()), mask: Ipv4Addr::from(mask.to_ne_bytes()), kind });
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(first);
        if fd >= 0 { libc::close(fd); }
    }
    out
}

pub fn local_addrs() -> Vec<IpAddr> {
    let mut out = Vec::new();
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        let mut first: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut first) != 0 { if fd >= 0 { libc::close(fd); } return out; }
        let mut cur = first;
        while !cur.is_null() {
            let ifa = &*cur;
            if ifa.ifa_flags & libc::IFF_UP as u32 != 0 && !ifa.ifa_addr.is_null() && media_active(fd, CStr::from_ptr(ifa.ifa_name)) {
                match (*ifa.ifa_addr).sa_family as libc::c_int {
                    libc::AF_INET => out.push(IpAddr::V4(Ipv4Addr::from((*(ifa.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr.to_ne_bytes()))),
                    libc::AF_INET6 => out.push(IpAddr::V6(Ipv6Addr::from((*(ifa.ifa_addr as *const libc::sockaddr_in6)).sin6_addr.s6_addr))),
                    _ => {}
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(first);
        if fd >= 0 { libc::close(fd); }
    }
    out
}

pub fn kind_toward(ifs: &[LocalIf], dest: IpAddr) -> Option<Kind> {
    let IpAddr::V4(dest) = dest else { return None };
    ifs.iter().filter(|i| u32::from(i.addr) & u32::from(i.mask) == u32::from(dest) & u32::from(i.mask))
        .max_by_key(|i| i.mask.octets().iter().map(|b| b.count_ones()).sum::<u32>()).map(|i| i.kind)
}

pub fn kind_of_local(ifs: &[LocalIf], local: IpAddr) -> Kind {
    let IpAddr::V4(local) = local else { return Kind::Other };
    ifs.iter().find(|i| i.addr == local).map_or(Kind::Other, |i| i.kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Vec<LocalIf> { vec![
        LocalIf { name: "bridge0".into(), addr: "169.254.192.119".parse().unwrap(), mask: "255.255.0.0".parse().unwrap(), kind: Kind::Wired },
        LocalIf { name: "en1".into(), addr: "192.0.2.60".parse().unwrap(), mask: "255.255.255.0".parse().unwrap(), kind: Kind::WiFi },
    ] }
    #[test] fn types() { assert_eq!(kind_for_type("Ethernet"), Some(Kind::Wired)); assert_eq!(kind_for_type("Bridge"), Some(Kind::Wired)); assert_eq!(kind_for_type("IEEE80211"), Some(Kind::WiFi)); assert_eq!(kind_for_type("VPN"), None); }
    #[test] fn toward() { let ifs = sample(); assert_eq!(kind_toward(&ifs, "169.254.1.2".parse().unwrap()), Some(Kind::Wired)); assert_eq!(kind_toward(&ifs, "192.0.2.2".parse().unwrap()), Some(Kind::WiFi)); assert_eq!(kind_toward(&ifs, "10.0.0.1".parse().unwrap()), None); }
    #[test] fn local() { let ifs = sample(); assert_eq!(kind_of_local(&ifs, "169.254.192.119".parse().unwrap()), Kind::Wired); assert_eq!(kind_of_local(&ifs, "192.0.2.60".parse().unwrap()), Kind::WiFi); assert_eq!(kind_of_local(&ifs, "10.0.0.1".parse().unwrap()), Kind::Other); }
    #[test] fn real_interfaces() {
        let ifs = local_interfaces();
        assert!(!ifs.is_empty());
        assert!(ifs.iter().all(|i| !i.addr.is_loopback()));
        assert!(ifs.iter().any(|i| i.name == "en1"));
        let bridge = std::ffi::CString::new("bridge0").unwrap();
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        assert!(fd >= 0);
        let active = media_active(fd, &bridge);
        unsafe { libc::close(fd); }
        assert_eq!(ifs.iter().any(|i| i.name == "bridge0"), active);
    }
    #[test] fn real_addrs_include_loopback() { assert!(local_addrs().contains(&IpAddr::V4(Ipv4Addr::LOCALHOST))); }
}
