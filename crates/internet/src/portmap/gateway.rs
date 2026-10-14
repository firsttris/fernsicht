//! The router: the default route's gateway, as the kernel's routing table
//! (`/proc/net/route`) has it.

use std::net::Ipv4Addr;

/// The gateway of the default route in `/proc/net/route`'s text: the row
/// whose destination and mask are 0, flags up and gateway (`RTF_UP |
/// RTF_GATEWAY`), its gateway written as little-endian hex.
pub fn default_gateway(route: &str) -> Option<Ipv4Addr> {
    route.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let [_, dest, gateway, flags, ..] = f.as_slice() else {
            return None;
        };
        let flags = u16::from_str_radix(flags, 16).ok()?;
        let mask = f.get(7)?;
        let default = *dest == "00000000" && *mask == "00000000";
        let up_gateway = flags & 0x3 == 0x3;
        if !(default && up_gateway) {
            return None;
        }
        let g = u32::from_str_radix(gateway, 16).ok()?;
        Some(Ipv4Addr::from(g.swap_bytes()))
    })
}

/// This machine's default gateway (Linux).
pub fn find() -> Option<Ipv4Addr> {
    default_gateway(&std::fs::read_to_string("/proc/net/route").ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_route_names_the_router() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                     wg0\t0000640A\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
                     enp5s0\t00000000\t01B2A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
                     enp5s0\t00B2A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(
            default_gateway(table),
            Some(Ipv4Addr::new(192, 168, 178, 1))
        );
        assert_eq!(default_gateway("Iface\tDestination\n"), None);
        let down = "h\nenp5s0\t00000000\t01B2A8C0\t0001\t0\t0\t100\t00000000\n";
        assert_eq!(default_gateway(down), None, "not a gateway route");
    }
}
