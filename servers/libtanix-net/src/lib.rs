//! tanix-libnet — Phase 11 socket layer for the `net` server.
//!
//! A small IPv4 stack with real TCP and UDP sockets, designed to be
//! *host-testable*: nothing here touches the kernel or the NIC, all clocks
//! are injected, and no allocation is used (fixed tables).  The unit tests
//! in `tests/` (host, `cargo test -p tanix-libnet`) exercise the whole
//! layer against crafted frames on the wire.
//!
//! Responsibilities:
//!   • ARP (probe + reply + gateway/cache learning),
//!   • IPv4 (framing, checksums, demux by protocol),
//!   • ICMP echo (request reply + outbound ping),
//!   • UDP endpoints (bind / send / deliver),
//!   • TCP sockets (RFC-793 subset, see `tcp.rs`),
//!   • the application API the net server drives.
//!
//! The NIC driver (vring, virtio-pci), the IRQ loop and the logging live in
//! the server; frames cross the boundary through `OutQueue`.

#![no_std]

pub mod checksum;
pub mod ip;
pub mod out;
pub mod tcp;
pub mod udp;

pub use ip::{IPPROTO_ICMP, IPPROTO_TCP, IPPROTO_UDP};
pub use tcp::{
    TcpNotify, TcpSeg, TcpSocket, TcpState, TxCtx, FLAG_ACK, FLAG_FIN, FLAG_RST, FLAG_SYN,
};
pub use udp::{UdpEp, UdpRx};

use ip::*;
use out::OutQueue;
use tcp::push_tcp_frame;

pub const MAX_TCP: usize = tcp::TCP_MAX;
pub const MAX_UDP: usize = udp::UDP_MAX;

const ARP_CACHE_MAX: usize = 8;
const ICMP_ECHO_PAYLOAD: usize = 16;

/// The whole network stack: all sockets, the ARP cache and the IP identity.
pub struct NetStack {
    pub mac: [u8; 6],
    pub my_ip: [u8; 4],
    pub gw_ip: [u8; 4],
    /// MAC of the gateway (10.0.2.2), learned via ARP.
    pub gw_mac: Option<[u8; 6]>,
    ip_id: u16,
    tcp: [Option<TcpSocket>; MAX_TCP],
    udp: [Option<UdpEp>; MAX_UDP],
    cache: [([u8; 4], [u8; 6]); ARP_CACHE_MAX],
    cache_n: usize,
    ephemeral: u16,
    pub ping_id: u16,
    pub ping_seq: u16,
    /// The last ICMP echo reply seen (id, seq); (0,0) means none.
    pub pong_id: u16,
    pub pong_seq: u16,
}

impl NetStack {
    pub const fn new(my_ip: [u8; 4], gw_ip: [u8; 4]) -> Self {
        NetStack {
            mac: [0; 6],
            my_ip,
            gw_ip,
            gw_mac: None,
            ip_id: 0x1234,
            tcp: [const { None }; MAX_TCP],
            udp: [const { None }; MAX_UDP],
            cache: [([0; 4], [0; 6]); ARP_CACHE_MAX],
            cache_n: 0,
            ephemeral: 49152,
            ping_id: 0x1234,
            ping_seq: 0,
            pong_id: 0,
            pong_seq: 0,
        }
    }

    pub fn set_mac(&mut self, mac: [u8; 6]) {
        self.mac = mac;
    }

    pub fn gw_resolved(&self) -> bool {
        self.gw_mac.is_some()
    }

    /// Learn `ip → mac` (overwrites an existing entry in place).
    fn cache_add(&mut self, ip: [u8; 4], mac: [u8; 6]) {
        if mac == [0; 6] {
            return;
        }
        for e in self.cache.iter_mut() {
            if e.0 == ip {
                e.1 = mac;
                return;
            }
        }
        if self.cache_n < ARP_CACHE_MAX {
            let s = &mut self.cache[self.cache_n];
            s.0 = ip;
            s.1 = mac;
            self.cache_n += 1;
        } else {
            self.cache[0] = (ip, mac);
        }
    }

    fn cache_get(&self, ip: [u8; 4]) -> Option<[u8; 6]> {
        if ip == self.gw_ip {
            return self.gw_mac;
        }
        self.cache[..self.cache_n]
            .iter()
            .find(|(i, _)| *i == ip)
            .map(|(_, m)| *m)
    }

    /// The MAC to reach `ip`: gateway ARP entry first, then the learned
    /// cache (covers the slirp host for solicited traffic).
    fn dst_mac(&self, ip: [u8; 4]) -> Option<[u8; 6]> {
        self.cache_get(ip)
    }

    // ── ARP ────────────────────────────────────────────────────────────────

    /// Send an ARP who-has for the gateway (used until `gw_resolved`).
    pub fn arp_probe(&mut self, q: &mut OutQueue) -> bool {
        let start = match q.push(14 + 28) {
            Some(s) => s,
            None => return false,
        };
        let b = &mut q.buf[start..start + 42];
        b[0..6].fill(0xFF);
        b[6..12].copy_from_slice(&self.mac);
        put_u16(b, 12, ETHERTYPE_ARP);
        let a = 14;
        put_u16(b, a, 1); // hwtype ether
        put_u16(b, a + 2, 0x0800);
        b[a + 4] = 6;
        b[a + 5] = 4;
        put_u16(b, a + 6, 1); // who-has
        b[a + 8..a + 14].copy_from_slice(&self.mac);
        b[a + 14..a + 18].copy_from_slice(&self.my_ip);
        b[a + 18..a + 24].fill(0); // target mac unknown
        b[a + 24..a + 28].copy_from_slice(&self.gw_ip);
        true
    }

    fn arp_reply(&mut self, q: &mut OutQueue, dst_mac: [u8; 6], sender_ip: [u8; 4], target_mac: [u8; 6], target_ip: [u8; 4]) -> bool {
        let start = match q.push(14 + 28) {
            Some(s) => s,
            None => return false,
        };
        let b = &mut q.buf[start..start + 42];
        b[0..6].copy_from_slice(&dst_mac);
        b[6..12].copy_from_slice(&self.mac);
        put_u16(b, 12, ETHERTYPE_ARP);
        let a = 14;
        put_u16(b, a, 1);
        put_u16(b, a + 2, 0x0800);
        b[a + 4] = 6;
        b[a + 5] = 4;
        put_u16(b, a + 6, 2); // reply
        b[a + 8..a + 14].copy_from_slice(&self.mac);
        b[a + 14..a + 18].copy_from_slice(&self.my_ip);
        b[a + 18..a + 24].copy_from_slice(&target_mac);
        b[a + 24..a + 28].copy_from_slice(&target_ip);
        let _ = sender_ip;
        true
    }

    fn handle_arp(&mut self, frame: &[u8], q: &mut OutQueue) {
        let Some(peer) = crate::ip::parse_eth(frame) else { return };
        let (_, src_mac, _) = peer;
        let a = 14;
        if frame.len() < a + 28 {
            return;
        }
        if get_u16(frame, a) != 1 || get_u16(frame, a + 2) != 0x0800 || frame[a + 4] != 6 || frame[a + 5] != 4 {
            return;
        }
        let mut sender_mac = [0u8; 6];
        let mut sender_ip = [0u8; 4];
        let mut target_ip = [0u8; 4];
        sender_mac.copy_from_slice(&frame[a + 8..a + 14]);
        sender_ip.copy_from_slice(&frame[a + 14..a + 18]);
        target_ip.copy_from_slice(&frame[a + 24..a + 28]);
        self.cache_add(sender_ip, sender_mac);
        let op = get_u16(frame, a + 6);
        if op == 1 && target_ip == self.my_ip {
            self.arp_reply(q, src_mac, sender_ip, sender_mac, sender_ip);
        } else if op == 2 && sender_ip == self.gw_ip {
            self.gw_mac = Some(sender_mac);
        }
    }

    // ── ICMP ────────────────────────────────────────────────────────────────

    /// Send an ICMP echo request (ping) to `dst_ip` (gateway resolved
    /// first by the caller); bumps `ping_seq`.
    pub fn icmp_echo_send(&mut self, dst_ip: [u8; 4], q: &mut OutQueue) -> bool {
        let Some(dst_mac) = self.dst_mac(dst_ip) else {
            return false;
        };
        let payload_len = ICMP_ECHO_PAYLOAD;
        let total = 14 + 20 + 8 + payload_len;
        let start = match q.push(total) {
            Some(s) => s,
            None => return false,
        };
        let b = &mut q.buf[start..start + total];
        b[0..6].copy_from_slice(&dst_mac);
        b[6..12].copy_from_slice(&self.mac);
        put_u16(b, 12, ETHERTYPE_IPV4);
        let p = 14;
        b[p] = 0x45;
        put_u16(b, p + 2, total as u16 - 14);
        let id = self.ip_id;
        self.ip_id = id.wrapping_add(1);
        put_u16(b, p + 4, id);
        b[p + 8] = 64; // TTL
        b[p + 9] = IPPROTO_ICMP;
        b[p + 12..p + 16].copy_from_slice(&self.my_ip);
        b[p + 16..p + 20].copy_from_slice(&dst_ip);
        let c = p + 20;
        b[c] = 8; // echo request
        put_u16(b, c + 4, self.ping_id);
        let seq = self.ping_seq;
        self.ping_seq = self.ping_seq.wrapping_add(1);
        put_u16(b, c + 6, seq);
        for i in 0..payload_len {
            b[c + 8 + i] = b'a' + (i % 26) as u8;
        }
        let sum = checksum::checksum(&b[c..c + 8 + payload_len]);
        put_u16(b, c + 2, sum);
        // IPv4 checksum.
        let ip_sum = checksum::checksum(&b[p..p + 20]);
        put_u16(b, p + 10, ip_sum);
        true
    }

    fn icmp_reply(&mut self, q: &mut OutQueue, peer_mac: [u8; 6], peer_ip: [u8; 4], icmp: &[u8]) {
        if icmp.len() < 8 {
            return;
        }
        let payload_len = core::cmp::min(icmp.len() - 8, 64);
        let total = 14 + 20 + 8 + payload_len;
        let start = match q.push(total) {
            Some(s) => s,
            None => return,
        };
        let b = &mut q.buf[start..start + total];
        b[0..6].copy_from_slice(&peer_mac);
        b[6..12].copy_from_slice(&self.mac);
        put_u16(b, 12, ETHERTYPE_IPV4);
        let p = 14;
        b[p] = 0x45;
        put_u16(b, p + 2, total as u16 - 14);
        let id = self.ip_id;
        self.ip_id = id.wrapping_add(1);
        put_u16(b, p + 4, id);
        b[p + 8] = 64; // TTL
        b[p + 9] = IPPROTO_ICMP;
        b[p + 12..p + 16].copy_from_slice(&self.my_ip);
        b[p + 16..p + 20].copy_from_slice(&peer_ip);
        let c = p + 20;
        b[c] = 0; // echo reply
        b[c + 1] = 0;
        b[c + 4..c + 8].copy_from_slice(&icmp[4..8]); // id + seq
        b[c + 8..c + 8 + payload_len].copy_from_slice(&icmp[8..8 + payload_len]);
        let sum = checksum::checksum(&b[c..c + 8 + payload_len]);
        put_u16(b, c + 2, sum);
        let ip_sum = checksum::checksum(&b[p..p + 20]);
        put_u16(b, p + 10, ip_sum);
    }

    // ── UDP sockets ─────────────────────────────────────────────────────────

    /// Bind a fixed port; returns the endpoint index.
    pub fn udp_bind(&mut self, port: u16) -> Option<usize> {
        for s in self.udp.iter_mut() {
            if let Some(e) = s {
                if e.port == port {
                    return None;
                }
            }
        }
        for (i, s) in self.udp.iter_mut().enumerate() {
            if s.is_none() {
                *s = Some(UdpEp::bind(port));
                return Some(i);
            }
        }
        None
    }

    /// Grab an ephemeral port for an outbound endpoint.
    pub fn udp_open(&mut self) -> Option<usize> {
        let port = self.ephemeral;
        self.ephemeral = self.ephemeral.wrapping_add(1);
        self.udp_bind(port)
    }

    /// Send a datagram from endpoint `idx` to `dst_ip:dst_port`.
    pub fn udp_send(&mut self, idx: usize, dst_ip: [u8; 4], dst_port: u16, payload: &[u8], q: &mut OutQueue) -> bool {
        let Some(e) = self.udp.get(idx).and_then(|o| o.as_ref()) else {
            return false;
        };
        let src_port = e.port;
        let Some(dst_mac) = self.dst_mac(dst_ip) else {
            return false;
        };
        let mut ctx = TxCtx {
            mac: self.mac,
            my_ip: self.my_ip,
            ip_id: &mut self.ip_id,
            q,
        };
        udp::push_udp_frame(&mut ctx, dst_mac, dst_ip, src_port, dst_port, payload)
    }

    pub fn udp_rx_info(&self, idx: usize) -> Option<UdpRx> {
        self.udp
            .get(idx)
            .and_then(|o| o.as_ref())
            .and_then(|e| e.rx_info())
    }

    pub fn udp_unread(&mut self, idx: usize, buf: &mut [u8]) -> usize {
        match self.udp.get_mut(idx).and_then(|o| o.as_mut()) {
            Some(e) => e.unread(buf),
            None => 0,
        }
    }

    // ── TCP sockets ─────────────────────────────────────────────────────────

    /// Passive open: listen on `port`.
    pub fn tcp_listen(&mut self, port: u16) -> Option<usize> {
        for s in self.tcp.iter_mut() {
            if let Some(s) = s {
                if s.local_port == port && s.state != TcpState::Listen {
                    return None;
                }
            }
        }
        for (i, s) in self.tcp.iter_mut().enumerate() {
            if s.is_none() {
                *s = Some(TcpSocket::listen(port));
                return Some(i);
            }
        }
        None
    }

    /// Active open: SYN goes out immediately (gateway must be resolved).
    pub fn tcp_connect(&mut self, dst_ip: [u8; 4], dst_port: u16, now: u64, q: &mut OutQueue) -> Option<usize> {
        let Some(dst_mac) = self.dst_mac(dst_ip) else {
            return None;
        };
        for i in 0..MAX_TCP {
            if self.tcp[i].is_some() {
                continue;
            }
            let port = self.ephemeral;
            self.ephemeral = self.ephemeral.wrapping_add(1);
            let mut sock = TcpSocket::connect(dst_ip, dst_port, port, now);
            sock.peer_mac = dst_mac;
            let mut ctx = TxCtx {
                mac: self.mac,
                my_ip: self.my_ip,
                ip_id: &mut self.ip_id,
                q,
            };
            sock.send_syn(&mut ctx, dst_mac, dst_ip);
            self.tcp[i] = Some(sock);
            return Some(i);
        }
        None
    }

    pub fn tcp_write(&mut self, idx: usize, data: &[u8]) -> bool {
        match self.tcp.get_mut(idx) {
            Some(Some(s)) => s.app_write(data),
            _ => false,
        }
    }

    pub fn tcp_read(&mut self, idx: usize, buf: &mut [u8]) -> usize {
        match self.tcp.get_mut(idx) {
            Some(Some(s)) => s.app_read(buf),
            _ => 0,
        }
    }

    pub fn tcp_rx_pending(&self, idx: usize) -> usize {
        match self.tcp.get(idx) {
            Some(Some(s)) => s.rx_pending(),
            _ => 0,
        }
    }

    pub fn tcp_close(&mut self, idx: usize) {
        if let Some(Some(s)) = self.tcp.get_mut(idx) {
            s.app_close();
        }
    }

    pub fn tcp_take_event(&mut self, idx: usize) -> TcpNotify {
        match self.tcp.get_mut(idx) {
            Some(Some(s)) => s.take_notify(),
            _ => TcpNotify::None,
        }
    }

    pub fn tcp_is_established(&self, idx: usize) -> bool {
        match self.tcp.get(idx) {
            Some(Some(s)) => s.is_established(),
            _ => false,
        }
    }

    pub fn tcp_peer(&self, idx: usize) -> ([u8; 4], u16) {
        match self.tcp.get(idx) {
            Some(Some(s)) => s.peer(),
            _ => ([0; 4], 0),
        }
    }

    /// Socket has fully finished (both sides closed / reset / aborted).
    pub fn tcp_done(&self, idx: usize) -> bool {
        match self.tcp.get(idx) {
            Some(Some(s)) => s.can_reuse(),
            _ => false,
        }
    }

    /// Release a finished socket slot (e.g. to listen again).
    pub fn tcp_free(&mut self, idx: usize) {
        if let Some(s) = self.tcp.get_mut(idx) {
            *s = None;
        }
    }

    fn tcp_find(&self, dport: u16, src_ip: [u8; 4], sport: u16) -> Option<usize> {
        for (i, s) in self.tcp.iter().enumerate() {
            if let Some(s) = s {
                if s.local_port != dport || s.dead {
                    continue;
                }
                if s.state == TcpState::Listen {
                    return Some(i);
                }
                if s.peer_ip == src_ip && s.peer_port == sport {
                    return Some(i);
                }
            }
        }
        None
    }

    // ── Frame ingress ──────────────────────────────────────────────────────

    /// Handle one RX frame (eth payload after the virtio header).  Replies
    /// are queued into `q`; `now` drives retransmit deadlines.
    pub fn on_frame(&mut self, frame: &[u8], now: u64, q: &mut OutQueue) {
        let Some((dst, src_mac, etype)) = parse_eth(frame) else {
            return;
        };
        let broadcast = dst == [0xFF; 6];
        if dst != self.mac && !broadcast {
            return;
        }

        match etype {
            ETHERTYPE_ARP => self.handle_arp(frame, q),
            ETHERTYPE_IPV4 => self.handle_ip4(frame, src_mac, now, q),
            _ => {}
        }
    }

    fn handle_ip4(&mut self, frame: &[u8], src_mac: [u8; 6], now: u64, q: &mut OutQueue) {
        let Some(hdr) = parse_ip4(frame) else {
            return;
        };
        if hdr.dst != self.my_ip {
            return;
        }
        self.cache_add(hdr.src, src_mac);
        let payload = &frame[hdr.transport..core::cmp::min(frame.len(), 14 + hdr.total_len)];
        match hdr.proto {
            IPPROTO_ICMP => self.handle_icmp(payload, src_mac, hdr.src, q),
            IPPROTO_UDP => self.handle_udp(payload, hdr.src, src_mac),
            IPPROTO_TCP => self.handle_tcp(payload, hdr.src, src_mac, now, q),
            _ => {}
        }
    }

    fn handle_icmp(&mut self, seg: &[u8], src_mac: [u8; 6], src_ip: [u8; 4], q: &mut OutQueue) {
        if seg.len() < 8 {
            return;
        }
        match seg[0] {
            0 => {
                // Echo reply (our ping): record id/seq for the app log.
                self.pong_id = get_u16(seg, 4);
                self.pong_seq = get_u16(seg, 6);
            }
            8 => {
                self.icmp_reply(q, src_mac, src_ip, seg);
            }
            _ => {}
        }
    }

    fn handle_udp(&mut self, seg: &[u8], src_ip: [u8; 4], _src_mac: [u8; 6]) {
        if seg.len() < 8 {
            return;
        }
        let sport = get_u16(seg, 0);
        let dport = get_u16(seg, 2);
        let len = core::cmp::min(get_u16(seg, 4) as usize, seg.len());
        let data = &seg[8..len];
        for e in self.udp.iter_mut() {
            if let Some(e) = e {
                if e.port == dport {
                    e.deliver(src_ip, sport, data);
                }
            }
        }
    }

    fn handle_tcp(&mut self, seg: &[u8], src_ip: [u8; 4], src_mac: [u8; 6], now: u64, q: &mut OutQueue) {
        if seg.len() < 20 {
            return;
        }
        let sport = get_u16(seg, 0);
        let dport = get_u16(seg, 2);
        let seq = get_u32(seg, 4);
        let ack = get_u32(seg, 8);
        let off_flags = seg[12];
        let off = ((off_flags >> 4) as usize) * 4;
        let flags = seg[13];
        let window = get_u16(seg, 14);
        if off < 20 || off > seg.len() {
            return;
        }

        let payload = &seg[off..];

        let idx = self.tcp_find(dport, src_ip, sport);
        let Some(idx) = idx else {
            // RST a SYN to a port nobody listens on (smoothes host probes).
            if flags & FLAG_SYN != 0 {
                let ack_rst = seq.wrapping_add(1 + payload.len() as u32);
                let mut ctx = TxCtx {
                    mac: self.mac,
                    my_ip: self.my_ip,
                    ip_id: &mut self.ip_id,
                    q,
                };
                push_tcp_frame(&mut ctx, src_mac, src_ip, dport, sport, 0, ack_rst, FLAG_RST | FLAG_ACK, 0, &[]);
            }
            return;
        };
        let Some(s) = self.tcp[idx].as_mut() else {
            return;
        };
        let seg_in = TcpSeg {
            seq,
            ack,
            syn: flags & FLAG_SYN != 0,
            ack_flag: flags & FLAG_ACK != 0,
            fin: flags & FLAG_FIN != 0,
            rst: flags & FLAG_RST != 0,
            window,
            payload,
            peer_ip: src_ip,
            peer_port: sport,
            peer_mac: src_mac,
        };
        let mut ctx = TxCtx {
            mac: self.mac,
            my_ip: self.my_ip,
            ip_id: &mut self.ip_id,
            q,
        };
        s.rx_segment(&seg_in, now, &mut ctx);
    }

    // ── Poll (retransmits, queued data, FINs) ───────────────────────────────

    /// Called by the server every event-loop round.
    pub fn poll(&mut self, now: u64, q: &mut OutQueue) {
        for i in 0..MAX_TCP {
            let Some(s) = self.tcp[i].as_mut() else { continue };
            let mut ctx = TxCtx {
                mac: self.mac,
                my_ip: self.my_ip,
                ip_id: &mut self.ip_id,
                q,
            };
            s.poll(now, &mut ctx);
        }
    }
}