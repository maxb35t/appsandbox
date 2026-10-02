//! Addresses a "block private" policy refuses: anything that isn't a routable public
//! unicast address, including the host, the LAN and the documentation/benchmark ranges.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn v4_in(ip: Ipv4Addr, net: [u8; 4], prefix: u32) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    (u32::from(ip) & mask) == (u32::from(Ipv4Addr::from(net)) & mask)
}

pub fn v4_blocked(ip: Ipv4Addr) -> bool {
    const NETS: &[([u8; 4], u32)] = &[
        ([0, 0, 0, 0], 8),        // "this network"
        ([10, 0, 0, 0], 8),       // private
        ([100, 64, 0, 0], 10),    // carrier-grade NAT
        ([127, 0, 0, 0], 8),      // loopback
        ([169, 254, 0, 0], 16),   // link-local
        ([172, 16, 0, 0], 12),    // private
        ([192, 0, 0, 0], 24),     // IETF protocol assignments
        ([192, 0, 2, 0], 24),     // documentation
        ([192, 88, 99, 0], 24),   // 6to4 relay anycast (deprecated)
        ([192, 168, 0, 0], 16),   // private
        ([198, 18, 0, 0], 15),    // benchmarking
        ([198, 51, 100, 0], 24),  // documentation
        ([203, 0, 113, 0], 24),   // documentation
        ([224, 0, 0, 0], 4),      // multicast
        ([240, 0, 0, 0], 4),      // reserved + broadcast
    ];
    NETS.iter().any(|(net, p)| v4_in(ip, *net, *p))
}

pub fn v6_blocked(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // IPv4-mapped / -compatible / NAT64: judge the embedded IPv4 address.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_blocked(v4);
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return v4_blocked(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
    }
    ip.is_unspecified()
        || ip.is_loopback()
        || (s[0] & 0xfe00) == 0xfc00          // unique local fc00::/7
        || (s[0] & 0xffc0) == 0xfe80          // link-local fe80::/10
        || (s[0] & 0xffc0) == 0xfec0          // site-local (deprecated)
        || (s[0] & 0xff00) == 0xff00          // multicast
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x2002)                   // 6to4: embeds arbitrary IPv4
        || (s[0] == 0 && s[1..6] == [0, 0, 0, 0, 0]) // ::/80 incl. IPv4-compatible
        || (s[0] == 0x0100 && s[1..4] == [0, 0, 0])  // discard-only 100::/64
}

pub fn blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => v6_blocked(v6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> bool {
        blocked(s.parse().unwrap())
    }

    #[test]
    fn private_and_special_v4_are_blocked() {
        for a in ["10.1.2.3", "127.0.0.1", "192.168.1.1", "172.16.0.1", "172.31.255.255", "169.254.1.1",
                  "100.64.0.1", "0.0.0.0", "224.0.0.1", "255.255.255.255", "198.18.0.1", "192.0.2.5"] {
            assert!(b(a), "{a} should be blocked");
        }
    }

    #[test]
    fn public_v4_is_allowed() {
        for a in ["8.8.8.8", "1.1.1.1", "140.82.112.3", "172.32.0.1", "100.128.0.1", "192.169.0.1"] {
            assert!(!b(a), "{a} should be allowed");
        }
    }

    #[test]
    fn v6_rules() {
        for a in ["::1", "::", "fe80::1", "fd00::1", "ff02::1", "2001:db8::1", "::ffff:192.168.1.1",
                  "64:ff9b::a00:1", "2002:c0a8:101::1", "::192.168.1.1"] {
            assert!(b(a), "{a} should be blocked");
        }
        for a in ["2606:4700:4700::1111", "2a00:1450:4009:81f::200e", "::ffff:8.8.8.8", "64:ff9b::808:808"] {
            assert!(!b(a), "{a} should be allowed");
        }
    }
}
