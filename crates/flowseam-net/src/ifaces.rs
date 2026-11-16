//! This machine's network interfaces as the link layer reads them. Listing them is the platform's job
//! (`links::LinkHost`); what kind of network a local or a remote address is on is worked out here.

use flowseam_core::proto::Kind;
use std::net::{IpAddr, Ipv4Addr};

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct LocalIf { pub name: String, pub addr: Ipv4Addr, pub mask: Ipv4Addr, pub kind: Kind }

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
    #[test] fn toward() { let ifs = sample(); assert_eq!(kind_toward(&ifs, "169.254.1.2".parse().unwrap()), Some(Kind::Wired)); assert_eq!(kind_toward(&ifs, "192.0.2.2".parse().unwrap()), Some(Kind::WiFi)); assert_eq!(kind_toward(&ifs, "10.0.0.1".parse().unwrap()), None); }
    #[test] fn local() { let ifs = sample(); assert_eq!(kind_of_local(&ifs, "169.254.192.119".parse().unwrap()), Kind::Wired); assert_eq!(kind_of_local(&ifs, "192.0.2.60".parse().unwrap()), Kind::WiFi); assert_eq!(kind_of_local(&ifs, "10.0.0.1".parse().unwrap()), Kind::Other); }
}
