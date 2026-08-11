//! Internet checksum (RFC 1071): sum of 16-bit big-endian words with
//! end-around carry, one's complement.  The transport checksums (TCP/UDP)
//! seed the fold with the IPv4 pseudo-header.

/// One's-complement 16-bit checksum over `data` (RFC 1071).
pub fn checksum(data: &[u8]) -> u16 {
    checksum_seed(data, 0)
}

/// `checksum` over `data` with a pre-folded `seed` (a partial sum, e.g. a
/// pseudo-header — see `pseudo_seed`).
pub fn checksum_seed(data: &[u8], mut sum: u32) -> u16 {
    let mut i = 0;
    while i + 1 < data.len() {
        sum += ((data[i] as u32) << 8) | data[i + 1] as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Fold the 12-byte IPv4 pseudo-header (src, dst, zero, proto, length) into
/// a seed for `checksum_seed` (RFC 768 §2 / RFC 793 §3.1).
///
/// The pseudo-header is a sequence of 16-bit big-endian words:
/// `src[0..2] src[2..4] dst[0..2] dst[2..4] (zero << 8 | proto) len`.
pub fn pseudo_seed(src: &[u8; 4], dst: &[u8; 4], proto: u8, len: u16) -> u32 {
    let mut sum: u32 = 0;
    for i in 0..2 {
        sum += ((src[2 * i] as u32) << 8) | src[2 * i + 1] as u32;
    }
    for i in 0..2 {
        sum += ((dst[2 * i] as u32) << 8) | dst[2 * i + 1] as u32;
    }
    sum += proto as u32;
    sum += len as u32;
    sum
}