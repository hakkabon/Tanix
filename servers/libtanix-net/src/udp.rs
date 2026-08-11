//! UDP sockets — Phase 11.
//!
//! Fixed port table, one 256-byte datagram buffer per endpoint.  Replies
//! answer the sender captured from the inbound frame (so no ARP is needed
//! for solicited traffic); app-initiated sends go to the gateway.

use crate::checksum::{checksum_seed, pseudo_seed};
use crate::ip::*;
use crate::tcp::TxCtx;

pub const UDP_MAX: usize = 4;
pub const UDP_BUF: usize = 256;

/// The datagram that arrived at an endpoint, waiting for the app.
pub struct UdpRx {
    pub peer_ip: [u8; 4],
    pub peer_port: u16,
    pub len: u16,
}

pub struct UdpEp {
    pub port: u16,
    rx: [u8; UDP_BUF],
    rx_len: usize,
    peer_ip: [u8; 4],
    peer_port: u16,
    /// New-datagram indicator + peer, consumed by the app.
    pub pending: bool,
}

impl UdpEp {
    pub fn bind(port: u16) -> UdpEp {
        UdpEp {
            port,
            rx: [0; UDP_BUF],
            rx_len: 0,
            peer_ip: [0; 4],
            peer_port: 0,
            pending: false,
        }
    }

    /// Copy the pending datagram into `buf`; returns its length.
    pub fn unread(&mut self, buf: &mut [u8]) -> usize {
        let n = core::cmp::min(self.rx_len, buf.len());
        buf[..n].copy_from_slice(&self.rx[..n]);
        self.rx_len = 0;
        self.pending = false;
        n
    }

    pub fn rx_info(&self) -> Option<UdpRx> {
        if self.pending {
            Some(UdpRx {
                peer_ip: self.peer_ip,
                peer_port: self.peer_port,
                len: self.rx_len as u16,
            })
        } else {
            None
        }
    }

    /// Deliver an inbound datagram (overwrites an unconsumed one).
    pub fn deliver(&mut self, peer_ip: [u8; 4], peer_port: u16, data: &[u8]) {
        self.peer_ip = peer_ip;
        self.peer_port = peer_port;
        self.rx_len = core::cmp::min(data.len(), UDP_BUF);
        self.rx[..self.rx_len].copy_from_slice(&data[..self.rx_len]);
        self.pending = true;
    }
}

/// Build one ethernet/IPv4/UDP frame with a correct checksum into `q`.
pub fn push_udp_frame(
    ctx: &mut TxCtx,
    dst_mac: [u8; 6],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> bool {
    let total = 14 + 20 + 8 + payload.len();
    let start = match ctx.q.push(total) {
        Some(s) => s,
        None => return false,
    };
    let b = &mut ctx.q.buf[start..start + total];

    b[0..6].copy_from_slice(&dst_mac);
    b[6..12].copy_from_slice(&ctx.mac);
    put_u16(b, 12, ETHERTYPE_IPV4);

    let p = 14;
    b[p] = 0x45;
    b[p + 1] = 0;
    put_u16(b, p + 2, total as u16 - 14);
    let id = *ctx.ip_id;
    *ctx.ip_id = id.wrapping_add(1);
    put_u16(b, p + 4, id);
    b[p + 6] = 0;
    b[p + 7] = 0; // flags + fragment offset
    b[p + 8] = 64; // TTL
    b[p + 9] = IPPROTO_UDP;
    put_u16(b, p + 10, 0);
    b[p + 12..p + 16].copy_from_slice(&ctx.my_ip);
    b[p + 16..p + 20].copy_from_slice(&dst_ip);
    let ip_sum = checksum_seed(&b[p..p + 20], 0);
    put_u16(b, p + 10, ip_sum);

    let u = p + 20;
    put_u16(b, u, src_port);
    put_u16(b, u + 2, dst_port);
    put_u16(b, u + 4, total as u16 - 34);
    put_u16(b, u + 6, 0); // checksum patched below
    if !payload.is_empty() {
        let d = u + 8;
        b[d..d + payload.len()].copy_from_slice(payload);
    }
    let udp_len = 8 + payload.len();
    let sum = checksum_seed(&b[u..u + udp_len], pseudo_seed(&ctx.my_ip, &dst_ip, IPPROTO_UDP, udp_len as u16));
    put_u16(b, u + 6, sum);
    true
}