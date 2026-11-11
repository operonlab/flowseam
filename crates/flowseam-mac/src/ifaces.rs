//! Active macOS IPv4 interfaces and their SystemConfiguration kinds.
use flowseam_core::proto::Kind;
use std::collections::BTreeMap;
use std::ffi::{c_void, CStr};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

/// BSD name → kind, read from SystemConfiguration. That read walks the IORegistry and cost each dial loop
/// a few percent CPU at two calls a second, so it is kept and read again when the set of interface names
/// changes, and at least every [`KINDS_FOR`]: a read that missed an interface (configd still settling) or a
/// name reused by another kind of adapter is then corrected without a restart.
struct KindCache { names: Vec<String>, kinds: BTreeMap<String, Kind>, read_at: Option<Instant> }

const KINDS_FOR: Duration = Duration::from_secs(60);

impl KindCache {
    const fn new() -> Self { KindCache { names: Vec::new(), kinds: BTreeMap::new(), read_at: None } }

    /// `names` sorted and deduplicated.
    fn get(&mut self, names: Vec<String>, now: Instant, load: impl FnOnce() -> BTreeMap<String, Kind>) -> &BTreeMap<String, Kind> {
        if self.names != names || self.read_at.is_none_or(|at| now.saturating_duration_since(at) >= KINDS_FOR) {
            let mut fresh = load();
            // A read that comes back short (configd busy) must not drop a kind still true: keep the known kind of
            // every interface that still exists and the new read left out. Names that are gone are dropped.
            for (name, kind) in std::mem::take(&mut self.kinds) {
                if names.binary_search(&name).is_ok() { fresh.entry(name).or_insert(kind); }
            }
            self.kinds = fresh;
            self.names = names;
            self.read_at = Some(now);
        }
        &self.kinds
    }
}

static KINDS: Mutex<KindCache> = Mutex::new(KindCache::new());

fn load_kinds() -> BTreeMap<String, Kind> {
    let mut kinds = BTreeMap::new();
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
    kinds
}

pub fn local_interfaces() -> Vec<LocalIf> {
    let mut names = Vec::new();
    let mut up = Vec::new();
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        let mut first: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut first) != 0 { if fd >= 0 { libc::close(fd); } return Vec::new(); }
        let mut cur = first;
        while !cur.is_null() {
            let ifa = &*cur;
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            if ifa.ifa_flags & (libc::IFF_UP | libc::IFF_LOOPBACK) as u32 == libc::IFF_UP as u32 && !ifa.ifa_addr.is_null() && !ifa.ifa_netmask.is_null() && (*ifa.ifa_addr).sa_family == libc::AF_INET as u8 && media_active(fd, CStr::from_ptr(ifa.ifa_name)) {
                let addr = (*(ifa.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr;
                let mask = (*(ifa.ifa_netmask as *const libc::sockaddr_in)).sin_addr.s_addr;
                up.push((name.clone(), Ipv4Addr::from(addr.to_ne_bytes()), Ipv4Addr::from(mask.to_ne_bytes())));
            }
            names.push(name);
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(first);
        if fd >= 0 { libc::close(fd); }
    }
    names.sort();
    names.dedup();
    let mut cache = KINDS.lock().unwrap_or_else(|e| e.into_inner());
    let kinds = cache.get(names, Instant::now(), load_kinds);
    up.into_iter().filter_map(|(name, addr, mask)| kinds.get(&name).map(|&kind| LocalIf { name, addr, mask, kind })).collect()
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
    #[test] fn kinds_are_read_again_only_when_the_names_change() {
        let loads = std::cell::Cell::new(0);
        let load = || { loads.set(loads.get() + 1); BTreeMap::from([(String::from("en0"), Kind::Wired)]) };
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let t = Instant::now();
        let mut c = KindCache::new();
        assert_eq!(c.get(names(&["en0", "lo0"]), t, load).get("en0"), Some(&Kind::Wired));
        assert_eq!(c.get(names(&["en0", "lo0"]), t + Duration::from_secs(1), load).get("en0"), Some(&Kind::Wired));
        assert_eq!(loads.get(), 1, "same interfaces: no second SystemConfiguration read");
        c.get(names(&["en0", "en7", "lo0"]), t + Duration::from_secs(2), load);
        assert_eq!(loads.get(), 2, "an adapter plugged in");
        c.get(names(&["en0", "lo0"]), t + Duration::from_secs(3), load);
        assert_eq!(loads.get(), 3, "and pulled out");
    }
    #[test] fn kinds_are_read_again_after_a_while_with_the_same_names() {
        let loads = std::cell::Cell::new(0);
        let load = || { loads.set(loads.get() + 1); BTreeMap::new() };
        let names = || vec![String::from("en0"), String::from("lo0")];
        let t = Instant::now();
        let mut c = KindCache::new();
        c.get(names(), t, load);
        c.get(names(), t + KINDS_FOR - Duration::from_millis(1), load);
        assert_eq!(loads.get(), 1);
        c.get(names(), t + KINDS_FOR, load);
        assert_eq!(loads.get(), 2, "a read that missed an interface is not kept for good");
    }
    #[test] fn a_short_read_keeps_the_kinds_of_interfaces_that_still_exist() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let t = Instant::now();
        let mut c = KindCache::new();
        c.get(names(&["en0", "en1", "lo0"]), t, || BTreeMap::from([(String::from("en0"), Kind::Wired), (String::from("en1"), Kind::WiFi)]));
        let after = c.get(names(&["en0", "en1", "lo0"]), t + KINDS_FOR, BTreeMap::new).clone();
        assert_eq!(after.get("en0"), Some(&Kind::Wired), "an empty re-read does not drop a live interface");
        assert_eq!(after.get("en1"), Some(&Kind::WiFi));
        let gone = c.get(names(&["en0", "lo0"]), t + KINDS_FOR * 2, || BTreeMap::from([(String::from("en0"), Kind::WiFi)])).clone();
        assert_eq!(gone.get("en0"), Some(&Kind::WiFi), "a new read wins for a name it reports");
        assert_eq!(gone.get("en1"), None, "an interface that is gone is dropped");
    }
    #[test] fn cached_interfaces_match_a_fresh_read() {
        let fresh = load_kinds();
        let ifs = local_interfaces();
        assert_eq!(local_interfaces().iter().map(|i| (&i.name, i.addr, i.kind)).collect::<Vec<_>>(), ifs.iter().map(|i| (&i.name, i.addr, i.kind)).collect::<Vec<_>>());
        assert!(ifs.iter().all(|i| fresh.get(&i.name) == Some(&i.kind)));
    }
    /// CPU per call of the SystemConfiguration read against a cached `local_interfaces`, on this machine.
    #[test] #[ignore = "timing probe: cargo test -p flowseam-mac --lib ifaces::tests::cost -- --ignored --nocapture --test-threads=1"]
    fn cost() {
        fn cpu() -> f64 {
            let mut u: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
            t(u.ru_utime) + t(u.ru_stime)
        }
        // Spaced like the dial loop's turns: back to back, the read finds warm caches and looks cheaper.
        const N: u32 = 20;
        const GAP: std::time::Duration = std::time::Duration::from_millis(250);
        let mut sc = 0.0;
        for _ in 0..N { std::thread::sleep(GAP); let t0 = cpu(); let _ = load_kinds(); sc += cpu() - t0; }
        sc /= f64::from(N);
        let _ = local_interfaces();
        let mut cached = 0.0;
        for _ in 0..N { std::thread::sleep(GAP); let t0 = cpu(); let _ = local_interfaces(); let _ = local_addrs(); cached += cpu() - t0; }
        cached /= f64::from(N);
        println!("SystemConfiguration read {:.2} ms CPU; cached local_interfaces + local_addrs {:.2} ms CPU", sc * 1e3, cached * 1e3);
    }
}
