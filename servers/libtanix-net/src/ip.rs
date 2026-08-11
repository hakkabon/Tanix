//! Ethernet / IPv4 framing constants and parsers.

pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV4: u16 = 0x0800;

pub const IPPROTO_ICMP: u8 = 1;
pub const IPPROTO_TCP: u8 = 6;
pub const IPPROTO_UDP: u8 = 17;

/// Parse the 14-byte ethernet header.  Returns `(dst, src, ethertype)`.
pub fn parse_eth(frame: &[u8]) -> Option<([u8; 6], [u8; 6], u16)> {
    if frame.len() < 14 {
        return None;
    }
    let mut dst = [0u8; 6];
    let mut src = [0u8; 6];
    dst.copy_from_slice(&frame[0..6]);
    src.copy_from_slice(&frame[6..12]);
    let etype = ((frame[12] as u16) << 8) | frame[13] as u16;
    Some((dst, src, etype))
}

/// The decoded IPv4 header of a frame whose ethernet header starts at 0.
pub struct Ip4Hdr {
    pub src: [u8; 4],
    pub dst: [u8; 4],
    pub proto: u8,
    /// Total IP payload length (from the header).
    pub total_len: usize,
    /// Byte offset of the transport header.
    pub transport: usize,
}

/// Parse the IPv4 header at byte 14 of `frame` (no options supported on
/// rx; IHL is honored for the transport offset).
pub fn parse_ip4(frame: &[u8]) -> Option<Ip4Hdr> {
    let p = 14;
    if frame.len() < p + 20 {
        return None;
    }
    let vihl = frame[p];
    if (vihl >> 4) != 4 {
        return None;
    }
    let total = ((frame[p + 2] as usize) << 8) | frame[p + 3] as usize;
    if frame.len() < p + total {
        return None; // truncated
    }
    let ihl = (vihl & 0x0F) as usize * 4;
    if ihl < 20 || p + ihl > frame.len() {
        return None;
    }
    let mut src = [0u8; 4];
    let mut dst = [0u8; 4];
    src.copy_from_slice(&frame[p + 12..p + 16]);
    dst.copy_from_slice(&frame[p + 16..p + 20]);
    Some(Ip4Hdr {
        src,
        dst,
        proto: frame[p + 9],
        total_len: total,
        transport: p + ihl,
    })
}

/// Put a 16-bit big-endian field.
pub fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off] = (v >> 8) as u8;
    buf[off + 1] = v as u8;
}

/// Put a 32-bit big-endian field.
pub fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    put_u16(buf, off, (v >> 16) as u16);
    put_u16(buf, off + 2, v as u16);
}

pub fn get_u16(buf: &[u8], off: usize) -> u16 {
    ((buf[off] as u16) << 8) | buf[off + 1] as u16
}

pub fn get_u32(buf: &[u8], off: usize) -> u32 {
    ((get_u16(buf, off) as u32) << 16) | get_u16(buf, off + 2) as u32
}