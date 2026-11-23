//! Where an allowed name may lead: the resolved-address check. A `connect` entry, pi's trusted hosts, and the user's
//! answer allow a name, but whoever controls that name's DNS chooses its addresses, so the gate resolves it once and
//! drops the addresses that reach this Mac or its link: loopback, unspecified, link-local, multicast, broadcast, the
//! cloud instance-metadata endpoints outside link-local, and every address of this Mac's own interfaces (a service bound
//! to `0.0.0.0` answers on them as it does on loopback). IPv4 inside an IPv6 form is judged as the IPv4 it carries. An
//! address an IP entry lists is exempt (listing `127.0.0.1:3000` is a choice). Same list as Claude Code's sandbox.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Why an address isn't dialed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nearby {
    Loopback,
    Unspecified,
    LinkLocal,
    Multicast,
    Broadcast,
    Metadata,
    ThisMac,
}

impl std::fmt::Display for Nearby {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Nearby::Loopback => "a loopback address",
            Nearby::Unspecified => "an unspecified address",
            Nearby::LinkLocal => "a link-local address",
            Nearby::Multicast => "a multicast address",
            Nearby::Broadcast => "a broadcast address",
            Nearby::Metadata => "a cloud metadata address",
            Nearby::ThisMac => "one of this Mac's addresses",
        })
    }
}

/// Cloud instance-metadata and platform endpoints outside link-local.
const METADATA_V4: [Ipv4Addr; 3] = [
    Ipv4Addr::new(100, 100, 100, 200),
    Ipv4Addr::new(168, 63, 129, 16),
    Ipv4Addr::new(192, 0, 0, 192),
];

/// Why `ip` isn't dialed, if it isn't. `own` are this Mac's interface addresses.
pub fn local(ip: IpAddr, own: &[IpAddr]) -> Option<Nearby> {
    let ip = match ip {
        IpAddr::V6(v6) => embedded_v4(v6).map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    };
    let class = match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                Some(Nearby::Loopback)
            } else if v4.octets()[0] == 0 {
                Some(Nearby::Unspecified)
            } else if v4.is_link_local() {
                Some(Nearby::LinkLocal)
            } else if v4.is_multicast() {
                Some(Nearby::Multicast)
            } else if v4.is_broadcast() {
                Some(Nearby::Broadcast)
            } else if METADATA_V4.contains(&v4) {
                Some(Nearby::Metadata)
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if v6.is_loopback() {
                Some(Nearby::Loopback)
            } else if v6.is_unspecified() {
                Some(Nearby::Unspecified)
            } else if s[0] & 0xffc0 == 0xfe80 {
                Some(Nearby::LinkLocal)
            } else if v6.is_multicast() {
                Some(Nearby::Multicast)
            } else if metadata_v6(v6) {
                Some(Nearby::Metadata)
            } else {
                None
            }
        }
    };
    class.or_else(|| own.contains(&ip).then_some(Nearby::ThisMac))
}

/// The IPv4 address an IPv6 form carries: mapped, compatible, translated, NAT64's well-known prefix, 6to4.
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    let tail = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    let mapped = s[..5] == [0; 5] && s[5] == 0xffff;
    let translated = s[..4] == [0; 4] && s[4] == 0xffff && s[5] == 0;
    let compatible = s[..6] == [0; 6] && !(s[6] == 0 && s[7] <= 1);
    let nat64 = s[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
    if mapped || translated || compatible || nat64 {
        return Some(tail);
    }
    if s[0] == 0x2002 {
        return Some(Ipv4Addr::new(
            (s[1] >> 8) as u8,
            s[1] as u8,
            (s[2] >> 8) as u8,
            s[2] as u8,
        ));
    }
    None
}

fn metadata_v6(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    (s[0] == 0xfd00 && s[1] == 0x0ec2)
        || v6 == Ipv6Addr::new(0xfd20, 0xce, 0, 0, 0, 0, 0, 0x254)
        || v6 == Ipv6Addr::new(0xfd00, 0xc1, 0, 0, 0, 0, 0xa9fe, 0xa9fe)
        || v6 == Ipv6Addr::new(0xfd00, 0x42, 0, 0, 0, 0, 0, 0x42)
}

/// This Mac's interface addresses, read now (`getifaddrs(3)`).
pub fn own() -> Vec<IpAddr> {
    let Ok(addresses) = nix::ifaddrs::getifaddrs() else {
        return Vec::new();
    };
    addresses
        .filter_map(|a| a.address)
        .filter_map(|a| {
            a.as_sockaddr_in()
                .map(|v4| IpAddr::V4(v4.ip()))
                .or_else(|| a.as_sockaddr_in6().map(|v6| IpAddr::V6(v6.ip())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses_are_refused() {
        let own: Vec<IpAddr> = vec!["192.168.1.20".parse().unwrap()];
        let check = |text: &str| local(text.parse().unwrap(), &own);
        for (text, why) in [
            ("127.0.0.1", Nearby::Loopback),
            ("127.8.8.8", Nearby::Loopback),
            ("::1", Nearby::Loopback),
            ("0.0.0.0", Nearby::Unspecified),
            ("::", Nearby::Unspecified),
            ("169.254.169.254", Nearby::LinkLocal),
            ("fe80::1", Nearby::LinkLocal),
            ("224.0.0.1", Nearby::Multicast),
            ("255.255.255.255", Nearby::Broadcast),
            ("100.100.100.200", Nearby::Metadata),
            ("fd00:ec2::254", Nearby::Metadata),
            ("192.168.1.20", Nearby::ThisMac),
            // IPv4 inside IPv6 forms.
            ("::ffff:127.0.0.1", Nearby::Loopback),
            ("64:ff9b::7f00:1", Nearby::Loopback),
            ("2002:7f00:1::", Nearby::Loopback),
            ("::ffff:192.168.1.20", Nearby::ThisMac),
        ] {
            assert_eq!(check(text), Some(why), "{text}");
        }
        for text in ["140.82.112.3", "2606:4700::6810:84e5", "10.0.0.5", "192.168.1.21"] {
            assert_eq!(check(text), None, "{text}");
        }
    }

    #[test]
    fn this_mac_has_addresses() {
        assert!(own().iter().any(IpAddr::is_loopback));
    }
}
