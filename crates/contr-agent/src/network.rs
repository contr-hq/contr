use std::fs;
use std::net::Ipv4Addr;

const RTF_GATEWAY: u16 = 0x2;

/// IPv4 default gateway of the guest; under WSL's NAT networking it is the Windows host.
pub(crate) fn default_gateway() -> Option<Ipv4Addr> {
    let table = fs::read_to_string("/proc/net/route").ok()?;

    gateway_in(&table)
}

/// Default gateway in a `/proc/net/route` table, whose addresses are little-endian hexadecimal.
fn gateway_in(table: &str) -> Option<Ipv4Addr> {
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (destination, gateway, flags) = (fields.get(1)?, fields.get(2)?, fields.get(3)?);
        let is_gateway = u16::from_str_radix(flags, 16).ok()? & RTF_GATEWAY != 0;
        if *destination != "00000000" || !is_gateway {
            return None;
        }

        Some(Ipv4Addr::from(u32::from_str_radix(gateway, 16).ok()?.to_le_bytes()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t0010A8C0\t00000000\t0001\t0\t0\t0\t00F0FFFF\t0\t0\t0
eth0\t00000000\t0110A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0";

    #[test]
    fn reads_the_default_gateway() {
        assert_eq!(gateway_in(TABLE), Some(Ipv4Addr::new(192, 168, 16, 1)));
    }

    #[test]
    fn needs_a_default_route() {
        let table = TABLE.lines().take(2).collect::<Vec<_>>().join("\n");

        assert_eq!(gateway_in(&table), None);
    }
}
