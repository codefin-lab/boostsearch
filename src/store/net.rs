//! Addresses, and the ranges written as one.

/// The span a CIDR block covers, written the way a range is: the first address
/// it contains, and the first one it does not.
pub fn cidr_bounds(mask: &str) -> Option<(String, String)> {
    let (lo, hi) = canonical_cidr(mask)?;
    let mut octets = [0u8; 16];
    for (i, o) in octets.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hi[i * 2..i * 2 + 2], 16).ok()?;
    }
    // one past the last address in the block
    for byte in octets.iter_mut().rev() {
        match byte.checked_add(1) {
            Some(next) => {
                *byte = next;
                break;
            }
            None => *byte = 0,
        }
    }
    let past: String = octets.iter().map(|b| format!("{b:02x}")).collect();
    Some((ip_from_canonical(&lo)?, ip_from_canonical(&past)?))
}

/// An IP in a form that sorts the way addresses do.
///
/// Text comparison puts "192.168.0.10" below "192.168.0.9", so ranges and
/// subnet queries need the fixed-width binary form. IPv4 is widened to its
/// IPv6-mapped shape so both families share one ordering.
pub fn canonical_ip(s: &str) -> Option<String> {
    let octets = match s.parse::<std::net::IpAddr>().ok()? {
        std::net::IpAddr::V4(v) => v.to_ipv6_mapped().octets(),
        std::net::IpAddr::V6(v) => v.octets(),
    };
    Some(octets.iter().map(|b| format!("{b:02x}")).collect())
}

/// Read an address back out of the fixed-width form it is stored in.
pub fn ip_from_canonical(hex: &str) -> Option<String> {
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut octets = [0u8; 16];
    for (i, o) in octets.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    let addr = std::net::Ipv6Addr::from(octets);
    Some(match addr.to_ipv4_mapped() {
        Some(v4) => v4.to_string(),
        None => addr.to_string(),
    })
}

/// The first and last address of a CIDR block, canonicalised.
pub fn canonical_cidr(s: &str) -> Option<(String, String)> {
    let (addr, bits) = s.split_once('/')?;
    let bits: u32 = bits.trim().parse().ok()?;
    let (mut lo, family_bits) = match addr.trim().parse::<std::net::IpAddr>().ok()? {
        std::net::IpAddr::V4(v) => (v.to_ipv6_mapped().octets(), 32u32),
        std::net::IpAddr::V6(v) => (v.octets(), 128u32),
    };
    if bits > family_bits {
        return None;
    }
    // an IPv4 prefix addresses the low 32 bits of the mapped form
    let prefix = bits + (128 - family_bits);
    let mut hi = lo;
    for i in 0..16u32 {
        let keep = prefix.saturating_sub(i * 8).min(8);
        let mask = if keep == 0 { 0u8 } else { (!0u8) << (8 - keep) };
        lo[i as usize] &= mask;
        hi[i as usize] |= !mask;
    }
    let hex = |o: [u8; 16]| -> String { o.iter().map(|b| format!("{b:02x}")).collect() };
    Some((hex(lo), hex(hi)))
}
