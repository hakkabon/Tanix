//! TCP socket layer (RFC 793 subset) — Phase 11.
//!
//! One sender state machine per connection.  The receiver model is simple
//! enough for an OS demo while staying wire-correct:
//!
//!   • passive open (LISTEN → SYN → SYN|ACK → ACK → ESTABLISHED) and
//!     active open (SYN_SENT → SYN|ACK → ACK → ESTABLISHED),
//!   • in-order data delivery with per-segment ACKs; anything not exactly
//!     at `rcv_nxt` is acked-but-dropped (duplicate ACK),
//!   • one outstanding segment at a time (stop-and-wait), window-clamped,
//!     RTO retransmit up to `MAX_RETRIES`, then RST-free teardown,
//!   • FIN/ACK closing in both orders (local-first and peer-first).
//!
//! The TX queue is a fixed byte buffer `[u8; TX_BUF]` with the sequence
//! numbers expressed as absolute offsets from `iss`; ACK progress compacts
//! the queue by memmove.  All clock input is injected (`now` ticks), so the
//! whole layer runs unmodified as a host-side unit test.

use crate::checksum::{checksum_seed, pseudo_seed};
use crate::ip::*;
use crate::out::OutQueue;

pub const TCP_MAX: usize = 4;
pub const RX_BUF: usize = 512;
pub const TX_BUF: usize = 512;
pub const TCP_MSS: usize = 512;
pub const WINDOW: u16 = 1024;
pub const RTO_TICKS: u64 = 800;
pub const MAX_RETRIES: u8 = 8;

pub const FLAG_FIN: u8 = 0x01;
pub const FLAG_SYN: u8 = 0x02;
pub const FLAG_RST: u8 = 0x04;
pub const FLAG_PSH: u8 = 0x08;
pub const FLAG_ACK: u8 = 0x10;

// ── Sequence-space helpers (RFC 1982) ─────────────────────────────────────────

#[inline]
pub fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
#[inline]
pub fn seq_gt(a: u32, b: u32) -> bool {
    seq_lt(b, a)
}
#[inline]
pub fn seq_ge(a: u32, b: u32) -> bool {
    a == b || seq_gt(a, b)
}

// ── Socket state ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpState {
    Listen,
    SynSent,
    SynRcvd,
    Established,
    CloseWait,
}

/// Events the app (net server) surfaces on the serial log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpNotify {
    None,
    /// Local or remote side finished the handshake.
    Accepted,
    Data,
    PeerFin,
    Closed,
}

// Pending-event bits (ordered: data before fin before closed).
const N_ACCEPTED: u8 = 1;
const N_DATA: u8 = 2;
const N_PEERFIN: u8 = 4;
const N_CLOSED: u8 = 8;

/// A parsed inbound TCP segment (header fields + payload window).
pub struct TcpSeg<'a> {
    pub seq: u32,
    pub ack: u32,
    pub syn: bool,
    pub ack_flag: bool,
    pub fin: bool,
    pub rst: bool,
    pub window: u16,
    pub payload: &'a [u8],
    /// Source host of the frame (eth sender + IPv4 source).
    pub peer_ip: [u8; 4],
    pub peer_port: u16,
    pub peer_mac: [u8; 6],
}

/// Everything a socket needs to emit frames (supplied by the stack).
pub struct TxCtx<'a> {
    pub mac: [u8; 6],
    pub my_ip: [u8; 4],
    pub ip_id: &'a mut u16,
    pub q: &'a mut OutQueue,
}

pub struct TcpSocket {
    pub state: TcpState,
    pub local_port: u16,
    pub peer_ip: [u8; 4],
    pub peer_port: u16,
pub peer_mac: [u8; 6],
    iss: u32,
    /// Sequence of `out[0]`; while the queue is empty, `snd_nxt`.
    data_base: u32,
    /// Sequence of the next fresh byte / FIN to place.
    snd_nxt: u32,
    /// Bytes of `out` already sent to the wire.
    sent: usize,
    /// Bytes of `out` currently unacknowledged (prefix of `out`).
    inflight: usize,
    rcv_nxt: u32,
    peer_wnd: u32,
    syn_acked: bool,
    fin_req: bool,
    fin_sent: bool,
    fin_seq: u32,
    fin_acked: bool,
    pub rx_fin: bool,
    out: [u8; TX_BUF],
    out_len: usize,
    rx: [u8; RX_BUF],
    rx_len: usize,
    rto_deadline: u64,
    retries: u8,
    notify: u8,
    /// Fatal (RST / retry exhaustion / both FINs acked) — slot is reusable.
    pub dead: bool,
}

fn gen_iss(port: u16, now: u64) -> u32 {
    let t = (now as u32).wrapping_mul(2654435761);
    t ^ ((port as u32) << 16 | port as u32)
    // ^ same ISS for a retransmitted SYN of the same connection, unique
    // across ports/connection attempts.
}

impl TcpSocket {
    fn blank(port: u16) -> TcpSocket {
        TcpSocket {
            state: TcpState::Listen,
            local_port: port,
            peer_ip: [0; 4],
            peer_port: 0,
            peer_mac: [0; 6],
            iss: 0,
            data_base: 0,
            snd_nxt: 0,
            sent: 0,
            inflight: 0,
            rcv_nxt: 0,
            peer_wnd: 4096,
            syn_acked: false,
            fin_req: false,
            fin_sent: false,
            fin_seq: 0,
            fin_acked: false,
            rx_fin: false,
            out: [0; TX_BUF],
            out_len: 0,
            rx: [0; RX_BUF],
            rx_len: 0,
            rto_deadline: 0,
            retries: 0,
            notify: 0,
            dead: false,
        }
    }

    /// A passive-open socket (app calls listen on the port).
    pub fn listen(port: u16) -> TcpSocket {
        TcpSocket::blank(port)
    }

    /// An active-open socket (app calls connect; the SYN goes out on the
    /// next poll / immediately via `send_syn`).
    pub fn connect(dst_ip: [u8; 4], dst_port: u16, src_port: u16, now: u64) -> TcpSocket {
        let mut s = TcpSocket::blank(src_port);
        s.state = TcpState::SynSent;
        s.peer_ip = dst_ip;
        s.peer_port = dst_port;
        s.iss = gen_iss(src_port, now);
        s.snd_nxt = s.iss.wrapping_add(1);
        s.data_base = s.snd_nxt;
        s.rto_deadline = now + RTO_TICKS;
        s
    }

    // ── App-facing API ────────────────────────────────────────────────────

    pub fn is_established(&self) -> bool {
        self.state == TcpState::Established
    }

    pub fn rx_pending(&self) -> usize {
        self.rx_len
    }

    pub fn take_notify(&mut self) -> TcpNotify {
        for (bit, ev) in [(N_ACCEPTED, TcpNotify::Accepted), (N_DATA, TcpNotify::Data), (N_PEERFIN, TcpNotify::PeerFin), (N_CLOSED, TcpNotify::Closed)] {
            if self.notify & bit != 0 {
                self.notify &= !bit;
                return ev;
            }
        }
        TcpNotify::None
    }

    /// Queue `data` for transmission (bounded; drops silently when full).
    pub fn app_write(&mut self, data: &[u8]) -> bool {
        if self.state != TcpState::Established && self.state != TcpState::CloseWait {
            return false;
        }
        if self.out_len + data.len() > TX_BUF {
            return false;
        }
        self.out[self.out_len..self.out_len + data.len()].copy_from_slice(data);
        self.out_len += data.len();
        self.snd_nxt = self.data_base.wrapping_add(self.out_len as u32);
        true
    }

    /// Copy the received stream into `buf`, consuming it.
    pub fn app_read(&mut self, buf: &mut [u8]) -> usize {
        let n = core::cmp::min(self.rx_len, buf.len());
        buf[..n].copy_from_slice(&self.rx[..n]);
        self.rx.copy_within(n..self.rx_len, 0);
        self.rx_len -= n;
        n
    }

    /// App asks to close: a FIN is sent once all queued data is out.
    pub fn app_close(&mut self) {
        self.fin_req = true;
    }

    pub fn peer(&self) -> ([u8; 4], u16) {
        (self.peer_ip, self.peer_port)
    }

    /// After `Closed`, the slot can be reused for a fresh listener/connect.
    pub fn can_reuse(&self) -> bool {
        self.dead
    }

    // ── Frame emission (all go through the shared push helper) ────────────

    fn push_tcp(
        &self,
        ctx: &mut TxCtx,
        dst_mac: [u8; 6],
        dst_ip: [u8; 4],
        seq: u32,
        ack: u32,
        flags: u8,
        wnd: u16,
        payload: &[u8],
    ) -> bool {
        push_tcp_frame(ctx, dst_mac, dst_ip, self.local_port, self.peer_port, seq, ack, flags, wnd, payload)
    }

    pub fn send_syn(&self, ctx: &mut TxCtx, dst_mac: [u8; 6], dst_ip: [u8; 4]) -> bool {
        self.push_tcp(ctx, dst_mac, dst_ip, self.iss, 0, FLAG_SYN, WINDOW, &[])
    }

    pub fn send_syn_ack(&self, ctx: &mut TxCtx, dst_mac: [u8; 6], dst_ip: [u8; 4]) -> bool {
        self.push_tcp(ctx, dst_mac, dst_ip, self.iss, self.rcv_nxt, FLAG_SYN | FLAG_ACK, WINDOW, &[])
    }

    fn send_ack(&self, ctx: &mut TxCtx, dst_mac: [u8; 6], dst_ip: [u8; 4]) -> bool {
        self.push_tcp(ctx, dst_mac, dst_ip, self.snd_nxt, self.rcv_nxt, FLAG_ACK, WINDOW, &[])
    }

    fn send_rst(&self, ctx: &mut TxCtx, dst_mac: [u8; 6], dst_ip: [u8; 4], ack: u32) -> bool {
        self.push_tcp(ctx, dst_mac, dst_ip, 0, ack, FLAG_RST | FLAG_ACK, 0, &[])
    }

    fn send_data_seg(&self, ctx: &mut TxCtx, dst_mac: [u8; 6], dst_ip: [u8; 4], seq: u32, data: &[u8]) -> bool {
        self.push_tcp(ctx, dst_mac, dst_ip, seq, self.rcv_nxt, FLAG_ACK | FLAG_PSH, WINDOW, data)
    }

    fn send_fin(&self, ctx: &mut TxCtx, dst_mac: [u8; 6], dst_ip: [u8; 4], seq: u32) -> bool {
        self.push_tcp(ctx, dst_mac, dst_ip, seq, self.rcv_nxt, FLAG_FIN | FLAG_ACK, WINDOW, &[])
    }

    // ── Inbound processing ────────────────────────────────────────────────

    /// Handle one inbound segment (peer fields already stamped).
    pub fn rx_segment(&mut self, s: &TcpSeg, now: u64, ctx: &mut TxCtx) {
        if self.dead {
            return;
        }
        if s.rst {
            self.dead = true;
            self.notify |= N_CLOSED;
            return;
        }
        if s.ack_flag {
            self.peer_wnd = s.window as u32;
            self.rx_ack(s.ack);
        }

        match self.state {
            TcpState::Listen => {
                if s.syn {
                    self.rx_syn(s, now, ctx);
                    return;
                }
                // SYN-less traffic to a listener: reset.
                self.dead = true;
                self.notify |= N_CLOSED;
            }
            TcpState::SynSent => {
                if s.syn {
                    self.rx_synack(s, ctx);
                    if self.syn_acked {
                        self.state = TcpState::Established;
                        self.notify |= N_ACCEPTED;
                    }
                    if !s.payload.is_empty() {
                        self.rx_data(s, ctx);
                    }
                    if s.fin {
                        self.rx_fin(s, ctx, true);
                    }
                }
            }
            TcpState::SynRcvd => {
                if s.syn {
                    // Retransmitted SYN: re-ack with the SAME ISS.
                    self.rcv_nxt = s.seq.wrapping_add(1);
                    self.rto_deadline = now + RTO_TICKS;
                    self.retries = 0;
                    let (m, ip) = (s.peer_mac, s.peer_ip);
                    self.send_syn_ack(ctx, m, ip);
                    return;
                }
                if self.syn_acked {
                    self.state = TcpState::Established;
                    self.notify |= N_ACCEPTED;
                    if !s.payload.is_empty() {
                        self.rx_data(s, ctx);
                    }
                    if s.fin {
                        self.rx_fin(s, ctx, true);
                    }
                }
            }
            TcpState::Established | TcpState::CloseWait => {
                if s.syn {
                    // Late/duplicate SYN: ack and continue.
                    let (m, ip) = (s.peer_mac, s.peer_ip);
                    self.send_ack(ctx, m, ip);
                }
                if !s.payload.is_empty() {
                    self.rx_data(s, ctx);
                }
                if s.fin {
                    self.rx_fin(s, ctx, false);
                }
            }
        }
    }

    /// SYN on a listening socket → SYN|ACK (same ISS on retransmission).
    fn rx_syn(&mut self, s: &TcpSeg, now: u64, ctx: &mut TxCtx) {
        self.iss = gen_iss(self.local_port, now);
        self.snd_nxt = self.iss.wrapping_add(1);
        self.data_base = self.snd_nxt;
        self.peer_ip = s.peer_ip;
        self.peer_port = s.peer_port;
        self.peer_mac = s.peer_mac;
        self.rcv_nxt = s.seq.wrapping_add(1);
        self.state = TcpState::SynRcvd;
        self.syn_acked = false;
        self.out_len = 0;
        self.sent = 0;
        self.inflight = 0;
        self.rx_len = 0;
        self.fin_req = false;
        self.fin_sent = false;
        self.fin_acked = false;
        self.rx_fin = false;
        self.retries = 0;
        self.rto_deadline = now + RTO_TICKS;
        let (m, ip) = (s.peer_mac, s.peer_ip);
        self.send_syn_ack(ctx, m, ip);
    }

    /// SYN (or SYN|ACK) while we are connecting.
    fn rx_synack(&mut self, s: &TcpSeg, ctx: &mut TxCtx) {
        let ack_ok = s.ack_flag && s.ack == self.iss.wrapping_add(1);
        if !ack_ok {
            // SYN without ACK (simultaneous open land): keep waiting.
            self.rcv_nxt = s.seq.wrapping_add(1);
            return;
        }
        self.rcv_nxt = s.seq.wrapping_add(1);
        self.syn_acked = true;
        self.rto_deadline = 0;
        let (m, ip) = (s.peer_mac, s.peer_ip);
        self.send_ack(ctx, m, ip);
    }

    /// ACK processing: consume the acked prefix of the TX queue and note
    /// SYN/FIN acknowledgements.
    fn rx_ack(&mut self, ack: u32) {
        // Everything between data_base and data_base+sent may be acked.
        let adv = ack.wrapping_sub(self.data_base) as usize;
        let adv = core::cmp::min(adv, self.sent);
        if adv > 0 {
            self.out.copy_within(adv..self.out_len, 0);
            self.out_len -= adv;
            self.data_base = self.data_base.wrapping_add(adv as u32);
            self.snd_nxt = self.data_base.wrapping_add(self.out_len as u32);
            self.sent -= adv;
            self.inflight = self.inflight.saturating_sub(adv);
            if self.out_len == 0 {
                self.data_base = self.snd_nxt;
            }
        }
        // SYN ack (only meaningful while handshaking).
        if !self.syn_acked
            && (self.state == TcpState::SynSent || self.state == TcpState::SynRcvd)
            && ack == self.snd_nxt
        {
            self.syn_acked = true;
            self.rto_deadline = 0;
        }
        // FIN ack: possible only once both sides are done on the wire.
        if self.fin_sent && seq_gt(ack, self.fin_seq) && !self.fin_acked {
            self.fin_acked = true;
            if self.rx_fin {
                self.dead = true;
                self.notify |= N_CLOSED;
            }
        }
    }

    /// Deliver a payload (in-order only; anything else draws a dup ACK).
    fn rx_data(&mut self, s: &TcpSeg, ctx: &mut TxCtx) {
        let (m, ip) = (s.peer_mac, s.peer_ip);
        if s.seq == self.rcv_nxt {
            let take = core::cmp::min(s.payload.len(), RX_BUF - self.rx_len);
            self.rx[self.rx_len..self.rx_len + take].copy_from_slice(&s.payload[..take]);
            self.rx_len += take;
            // Ack the whole payload even when truncated past the buffer.
            self.rcv_nxt = self.rcv_nxt.wrapping_add(s.payload.len() as u32);
            self.notify |= N_DATA;
            self.send_ack(ctx, m, ip);
        } else {
            self.send_ack(ctx, m, ip); // duplicate ACK
        }
    }

    fn rx_fin(&mut self, s: &TcpSeg, ctx: &mut TxCtx, syn_phase: bool) {
        let fin_seq = s.seq.wrapping_add(s.payload.len() as u32);
        let (m, ip) = (s.peer_mac, s.peer_ip);
        if seq_ge(fin_seq, self.rcv_nxt) {
            self.rcv_nxt = fin_seq.wrapping_add(1);
            self.rx_fin = true;
            // A data-bearing segment may carry FIN too (SHUT_WR coalescing);
            // deliver the payload first, then the close notification.
            self.notify |= N_PEERFIN;
            if self.state == TcpState::Established && !syn_phase {
                self.state = TcpState::CloseWait;
            }
            if self.fin_sent && self.fin_acked {
                self.dead = true;
                self.notify |= N_CLOSED;
            }
        }
        self.send_ack(ctx, m, ip);
    }

    // ── Outbound machinerary (polled by the stack every loop) ─────────────

    /// Advance transmission: retransmit, send fresh data, or send the FIN.
    /// Returns the number of frames queued (0 or 1).
    pub fn poll(&mut self, now: u64, ctx: &mut TxCtx) {
        if self.dead {
            return;
        }
        let (m, ip) = (self.peer_mac, self.peer_ip);
        let m = if m == [0; 6] {
            return; // peer not learned yet (no inbound segment seen)
        } else {
            m
        };

        // Handshake retransmission.
        if !self.syn_acked && (self.state == TcpState::SynSent || self.state == TcpState::SynRcvd) {
            if now >= self.rto_deadline {
                self.retries += 1;
                if self.retries > MAX_RETRIES {
                    self.dead = true;
                    self.notify |= N_CLOSED;
                    return;
                }
                if self.state == TcpState::SynSent {
                    self.send_syn(ctx, m, ip);
                } else {
                    self.send_syn_ack(ctx, m, ip);
                }
                self.rto_deadline = now + RTO_TICKS;
            }
            return;
        }

        // In-flight data retransmission.
        if self.inflight > 0 {
            if now >= self.rto_deadline {
                self.retries += 1;
                if self.retries > MAX_RETRIES {
                    self.dead = true;
                    self.notify |= N_CLOSED;
                    return;
                }
                let start = self.sent - self.inflight;
                let seq = self.data_base.wrapping_add(start as u32);
                self.send_data_seg(ctx, m, ip, seq, &self.out[start..start + self.inflight]);
                self.rto_deadline = now + RTO_TICKS;
            }
            return;
        }

        // Fresh data (window-clamped; no window-probe persistence).
        if self.sent < self.out_len {
            if self.peer_wnd == 0 {
                return;
            }
            let chunk = core::cmp::min(self.out_len - self.sent, TCP_MSS).min(self.peer_wnd as usize);
            if chunk == 0 {
                return;
            }
            let seq = self.data_base.wrapping_add(self.sent as u32);
            self.send_data_seg(ctx, m, ip, seq, &self.out[self.sent..self.sent + chunk]);
            self.sent += chunk;
            self.inflight = chunk;
            self.retries = 0;
            self.rto_deadline = now + RTO_TICKS;
            return;
        }

        // FIN.
        if self.fin_req && !self.fin_sent {
            let seq = self.snd_nxt;
            self.send_fin(ctx, m, ip, seq);
            self.fin_sent = true;
            self.fin_seq = seq;
            self.retries = 0;
            self.rto_deadline = now + RTO_TICKS;
            return;
        }
        if self.fin_sent && !self.fin_acked && now >= self.rto_deadline {
            self.retries += 1;
            if self.retries > MAX_RETRIES {
                self.dead = true;
                self.notify |= N_CLOSED;
                return;
            }
            self.send_fin(ctx, m, ip, self.fin_seq);
            self.rto_deadline = now + RTO_TICKS;
        }
    }
}

// ── Segment frame builder (eth + IPv4 + TCP) ─────────────────────────────────

/// Build one complete ethernet/IPv4/TCP frame into `ctx.q`.
pub fn push_tcp_frame(
    ctx: &mut TxCtx,
    dst_mac: [u8; 6],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    wnd: u16,
    payload: &[u8],
) -> bool {
    let total = 14 + 20 + 20 + payload.len();
    let start = match ctx.q.push(total) {
        Some(s) => s,
        None => return false,
    };
    let b = &mut ctx.q.buf[start..start + total];

    // Ethernet.
    b[0..6].copy_from_slice(&dst_mac);
    b[6..12].copy_from_slice(&ctx.mac);
    put_u16(b, 12, ETHERTYPE_IPV4);

    // IPv4 (no options).
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
    b[p + 9] = IPPROTO_TCP;
    put_u16(b, p + 10, 0); // checksum patched below
    b[p + 12..p + 16].copy_from_slice(&ctx.my_ip);
    b[p + 16..p + 20].copy_from_slice(&dst_ip);
    let ip_sum = checksum_seed(&b[p..p + 20], 0);
    put_u16(b, p + 10, ip_sum);

    // TCP (20-byte header, no options).
    let t = p + 20;
    put_u16(b, t, src_port);
    put_u16(b, t + 2, dst_port);
    put_u32(b, t + 4, seq);
    put_u32(b, t + 8, ack);
    b[t + 12] = 0x50; // data offset 5 words (upper nibble)
    b[t + 13] = flags; // control flags (full byte)
    put_u16(b, t + 14, wnd);
    put_u16(b, t + 16, 0); // checksum patched below
    put_u16(b, t + 18, 0); // urgent pointer
    if !payload.is_empty() {
        let d = t + 20;
        b[d..d + payload.len()].copy_from_slice(payload);
    }
    let tcp_len = 20 + payload.len();
    let sum = checksum_seed(&b[t..t + tcp_len], pseudo_seed(&ctx.my_ip, &dst_ip, IPPROTO_TCP, tcp_len as u16));
    put_u16(b, t + 16, sum);
    true
}