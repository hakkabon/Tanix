//! Host-side tests for the Phase-11 socket layer (`tanix-libnet`).
//!
//! The guest is a `NetStack` with MAC 52:54:00:12:34:56 at 10.0.2.15; the
//! "host" is 02:11:22:33:44:55 at 10.0.2.2.  Frames from the host are
//! built with the lib's own segment builders (checksums verified
//! independently in `checksum_vectors` against RFC 1071 reference sums),
//! pushed through `on_frame`, and the reply frames are parsed back.

use tanix_libnet::ip::*;
use tanix_libnet::out::OutQueue;
use tanix_libnet::tcp::{push_tcp_frame, FLAG_ACK, FLAG_FIN, FLAG_PSH, FLAG_RST, FLAG_SYN, TxCtx};
use tanix_libnet::udp::push_udp_frame;
use tanix_libnet::{NetStack, TcpNotify, IPPROTO_ICMP, IPPROTO_TCP, IPPROTO_UDP};

const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const HOST_MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const HOST_IP: [u8; 4] = [10, 0, 2, 2];

const RTO: u64 = tanix_libnet::tcp::RTO_TICKS;

fn guest() -> NetStack {
    let mut ns = NetStack::new(GUEST_IP, HOST_IP);
    ns.set_mac(GUEST_MAC);
    ns
}

/// Resolve the gateway the way the real net server does: feed an ARP reply.
fn resolve_gw(ns: &mut NetStack) {
    let mut frame = [0u8; 14 + 28];
    frame[0..6].copy_from_slice(&GUEST_MAC);
    frame[6..12].copy_from_slice(&HOST_MAC);
    put_u16(&mut frame, 12, 0x0806);
    put_u16(&mut frame, 14, 1);
    put_u16(&mut frame, 16, 0x0800);
    frame[18] = 6;
    frame[19] = 4;
    put_u16(&mut frame, 20, 2); // reply
    frame[22..28].copy_from_slice(&HOST_MAC);
    frame[28..32].copy_from_slice(&HOST_IP);
    frame[32..38].copy_from_slice(&GUEST_MAC);
    frame[38..42].copy_from_slice(&GUEST_IP);
    let mut q = OutQueue::new();
    ns.on_frame(&frame, 0, &mut q);
    assert!(ns.gw_resolved(), "gateway must resolve from the ARP reply");
}

fn host_tcp(flags: u8, seq: u32, ack: u32, sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let mut q = OutQueue::new();
    let mut ipid: u16 = 7;
    {
        let mut ctx = TxCtx {
            mac: HOST_MAC,
            my_ip: HOST_IP,
            ip_id: &mut ipid,
            q: &mut q,
        };
        push_tcp_frame(&mut ctx, GUEST_MAC, GUEST_IP, sport, dport, seq, ack, flags, 1024, payload);
    }
    q.frame(0).to_vec()
}

fn host_udp(sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let mut q = OutQueue::new();
    let mut ipid: u16 = 7;
    {
        let mut ctx = TxCtx {
            mac: HOST_MAC,
            my_ip: HOST_IP,
            ip_id: &mut ipid,
            q: &mut q,
        };
        push_udp_frame(&mut ctx, GUEST_MAC, GUEST_IP, sport, dport, payload);
    }
    q.frame(0).to_vec()
}

// ── Frame field extractors (parser mirrors of ip.rs) ─────────────────────────

fn eth_type(f: &[u8]) -> u16 {
    get_u16(f, 12)
}
fn ip_proto(f: &[u8]) -> u8 {
    f[14 + 9]
}
fn ip_src(f: &[u8]) -> [u8; 4] {
    [f[14 + 12], f[14 + 13], f[14 + 14], f[14 + 15]]
}
fn ip_dst(f: &[u8]) -> [u8; 4] {
    [f[14 + 16], f[14 + 17], f[14 + 18], f[14 + 19]]
}
fn t_flags(f: &[u8]) -> u8 {
    f[34 + 13]
}
fn t_sport(f: &[u8]) -> u16 {
    get_u16(f, 34)
}
fn t_dport(f: &[u8]) -> u16 {
    get_u16(f, 36)
}
fn t_seq(f: &[u8]) -> u32 {
    get_u32(f, 38)
}
fn t_ack(f: &[u8]) -> u32 {
    get_u32(f, 42)
}
fn t_payload(f: &[u8]) -> &[u8] {
    &f[54..]
}
fn t_window(f: &[u8]) -> u16 {
    get_u16(f, 48)
}

fn tcp_verify(f: &[u8]) {
    assert_eq!(eth_type(f), 0x0800);
    // IPv4 header layout: v4/IHL, no flags/frag, TTL 64, proto TCP.
    assert_eq!(f[14], 0x45);
    assert_eq!(get_u16(f, 14 + 6), 0, "flags+fragment offset must be zero");
    assert_eq!(f[14 + 8], 64, "TTL must be 64");
    assert_eq!(ip_proto(f), IPPROTO_TCP);
    // IP + TCP checksums must fold to zero.
    let ip_sum = tanix_libnet::checksum::checksum(&f[14..34]);
    assert_eq!(ip_sum, 0, "IPv4 checksum must verify");
    let tsegs = f[34..].len() as u16;
    let seed = tanix_libnet::checksum::pseudo_seed(&ip_src(f), &ip_dst(f), IPPROTO_TCP, tsegs);
    let sum = tanix_libnet::checksum::checksum_seed(&f[34..], seed);
    assert_eq!(sum, 0, "TCP checksum must verify");
    // TCP header layout: data offset 5 in the upper nibble of byte 12,
    // the flags occupy the full byte 13.
    assert_eq!(f[34 + 12], 0x50, "data offset must be 5 dwords");
}

// ── Checksums (RFC 1071 reference vectors, computed independently) ───────────

#[test]
fn checksum_vectors() {
    use tanix_libnet::checksum::{checksum, checksum_seed, pseudo_seed};
    assert_eq!(checksum(b"hello"), 0xBC2D);

    // UDP 10.0.2.15:5555 → 10.0.2.2:5557, payload "hello".
    let mut udp = [0u8; 8 + 5];
    put_u16(&mut udp, 0, 5555);
    put_u16(&mut udp, 2, 5557);
    put_u16(&mut udp, 4, 13);
    udp[8..].copy_from_slice(b"hello");
    let seed = pseudo_seed(&GUEST_IP, &HOST_IP, IPPROTO_UDP, 13);
    assert_eq!(checksum_seed(&udp, seed), 0x7889);

    // TCP handshake SYN|ACK: 7778→7777, seq 0x11223344, ack 0, wnd 1024.
    let mut t1 = [0u8; 20];
    put_u16(&mut t1, 0, 7778);
    put_u16(&mut t1, 2, 7777);
    put_u32(&mut t1, 4, 0x11223344);
    t1[12] = 0x50; // data offset 5 words
    t1[13] = FLAG_SYN | FLAG_ACK;
    put_u16(&mut t1, 14, 1024);
    let seed = pseudo_seed(&GUEST_IP, &HOST_IP, IPPROTO_TCP, 20);
    assert_eq!(checksum_seed(&t1, seed), 0x1299);

    // TCP data ACK|PSH "ping": 7777→7778, seq 0x55667788, ack 0x99AABBCC.
    let mut t2 = [0u8; 20 + 4];
    put_u16(&mut t2, 0, 7777);
    put_u16(&mut t2, 2, 7778);
    put_u32(&mut t2, 4, 0x55667788);
    put_u32(&mut t2, 8, 0x99AABBCC);
    t2[12] = 0x50; // data offset 5 words
    t2[13] = FLAG_ACK | FLAG_PSH;
    put_u16(&mut t2, 14, 1024);
    t2[20..].copy_from_slice(b"ping");
    let seed = pseudo_seed(&GUEST_IP, &HOST_IP, IPPROTO_TCP, 24);
    assert_eq!(checksum_seed(&t2, seed), 0x55BE);

    // IPv4 header: v4/iHL 0x45, total 32, id 0x1234, ttl 64, proto 17.
    let mut ih = [0u8; 20];
    ih[0] = 0x45;
    put_u16(&mut ih, 2, 32);
    put_u16(&mut ih, 4, 0x1234);
    ih[8] = 64;
    ih[9] = IPPROTO_UDP;
    ih[12..16].copy_from_slice(&GUEST_IP);
    ih[16..20].copy_from_slice(&HOST_IP);
    assert_eq!(checksum(&ih), 0x5089);
}

// ── UDP ──────────────────────────────────────────────────────────────────────

#[test]
fn udp_outbound_frame_is_correct() {
    let mut ns = guest();
    resolve_gw(&mut ns);
    let idx = ns.udp_bind(5555).expect("bind");
    let mut q = OutQueue::new();
    assert!(ns.udp_send(idx, HOST_IP, 5557, b"hello-udp", &mut q));
    assert_eq!(q.len(), 1);
    let f = q.frame(0);
    assert_eq!(eth_type(f), 0x0800);
    assert_eq!(f[14], 0x45);
    assert_eq!(get_u16(f, 14 + 6), 0, "flags+fragment offset must be zero");
    assert_eq!(f[14 + 8], 64, "TTL must be 64");
    assert_eq!(ip_proto(f), IPPROTO_UDP);
    assert_eq!(f[6..12], GUEST_MAC);
    assert_eq!(f[0..6], HOST_MAC);
    assert_eq!(get_u16(f, 34), 5555, "src port");
    assert_eq!(get_u16(f, 36), 5557, "dst port");
    assert_eq!(get_u16(f, 38), 8 + 9, "udp len");
    assert_eq!(&f[42..51], b"hello-udp");
    let seed = tanix_libnet::checksum::pseudo_seed(&GUEST_IP, &HOST_IP, IPPROTO_UDP, 17);
    assert_eq!(tanix_libnet::checksum::checksum_seed(&f[34..51], seed), 0);
}

#[test]
fn udp_inbound_delivers_to_bound_port_only() {
    let mut ns = guest();
    let idx = ns.udp_bind(5555).expect("bind");
    let mut q = OutQueue::new();
    let f = host_udp(30000, 5555, b"from-host");
    ns.on_frame(&f, 0, &mut q);
    let info = ns.udp_rx_info(idx).expect("pending");
    assert_eq!(info.peer_ip, HOST_IP);
    assert_eq!(info.peer_port, 30000);
    assert_eq!(info.len, 9);
    let mut buf = [0u8; 64];
    assert_eq!(ns.udp_unread(idx, &mut buf), 9);
    assert_eq!(&buf[..9], b"from-host");

    // Unbound port: silently dropped.
    let f = host_udp(30001, 9999, b"nowhere");
    ns.on_frame(&f, 0, &mut q);
    assert!(ns.udp_rx_info(idx).is_none(), "unbound port must not deliver");
}

// ── TCP: passive open (listener) ─────────────────────────────────────────────

#[test]
fn tcp_listener_handshake() {
    let mut ns = guest();
    let li = ns.tcp_listen(7777).expect("listen");
    let mut q = OutQueue::new();

    ns.on_frame(&host_tcp(FLAG_SYN, 1000, 0, 44444, 7777, &[]), 0, &mut q);
    assert_eq!(q.len(), 1, "SYN must produce exactly one SYN|ACK");
    let f = q.frame(0);
    tcp_verify(f);
    assert_eq!(t_flags(f), FLAG_SYN | FLAG_ACK);
    assert_eq!(t_sport(f), 7777);
    assert_eq!(t_dport(f), 44444);
    assert_eq!(t_ack(f), 1001, "SYN|ACK acks the incoming SYN");
    let iss = t_seq(f);

    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK, 1001, iss + 1, 44444, 7777, &[]), 0, &mut q);
    assert!(ns.tcp_is_established(li), "ACK completes the handshake");
    assert_eq!(ns.tcp_take_event(li), TcpNotify::Accepted);
    assert_eq!(ns.tcp_peer(li), (HOST_IP, 44444));
}

#[test]
fn tcp_retransmitted_syn_keeps_iss() {
    let mut ns = guest();
    let _ = ns.tcp_listen(7777).expect("listen");
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_SYN, 1000, 0, 44444, 7777, &[]), 0, &mut q);
    let iss = t_seq(q.frame(0));
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_SYN, 2000, 0, 44444, 7777, &[]), 50, &mut q);
    assert_eq!(t_seq(q.frame(0)), iss, "duplicate SYN must answer with the same ISS");
    assert_eq!(t_ack(q.frame(0)), 2001);
}

#[test]
fn tcp_syn_to_closed_port_gets_rst() {
    let mut ns = guest();
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_SYN, 999, 0, 55555, 9000, &[]), 0, &mut q);
    assert_eq!(q.len(), 1);
    let f = q.frame(0);
    tcp_verify(f);
    assert_eq!(t_flags(f), FLAG_RST | FLAG_ACK);
    assert_eq!(t_sport(f), 9000);
    assert_eq!(t_ack(f), 1000, "RST acks the SYN");
}

// ── TCP: active open (connect) ───────────────────────────────────────────────

#[test]
fn tcp_connect_handshake() {
    let mut ns = guest();
    resolve_gw(&mut ns);
    let mut q = OutQueue::new();

    let ci = ns.tcp_connect(HOST_IP, 7778, 10, &mut q).expect("connect");
    assert_eq!(q.len(), 1, "connect must emit the SYN immediately");
    let f = q.frame(0);
    tcp_verify(f);
    assert_eq!(t_flags(f), FLAG_SYN);
    assert_eq!(t_dport(f), 7778);
    assert_eq!(t_ack(f), 0);
    let iss = t_seq(f);

    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_SYN | FLAG_ACK, 50000, iss + 1, 7778, t_sport(&f), &[]), 20, &mut q);
    assert_eq!(q.len(), 1, "SYN|ACK must be acknowledged");
    let a = q.frame(0);
    tcp_verify(a);
    assert_eq!(t_flags(a), FLAG_ACK);
    assert_eq!(t_seq(a), iss + 1);
    assert_eq!(t_ack(a), 50001);
    assert!(ns.tcp_is_established(ci));
    assert_eq!(ns.tcp_take_event(ci), TcpNotify::Accepted);
}

// ── TCP: data exchange ───────────────────────────────────────────────────────

/// Drive a listener socket to ESTABLISHED and return its index.
fn established_listener(ns: &mut NetStack) -> usize {
    let li = ns.tcp_listen(7777).expect("listen");
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_SYN, 1000, 0, 44444, 7777, &[]), 0, &mut q);
    let iss = t_seq(q.frame(0));
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK, 1001, iss + 1, 44444, 7777, &[]), 0, &mut q);
    assert_eq!(ns.tcp_take_event(li), TcpNotify::Accepted, "helper drains the accept");
    li
}

#[test]
fn tcp_data_roundtrip() {
    let mut ns = guest();
    let li = established_listener(&mut ns);
    let mut q = OutQueue::new();

    // Host → guest: data at rcv_nxt.
    ns.on_frame(&host_tcp(FLAG_ACK | FLAG_PSH, 1001, 0, 44444, 7777, b"ping"), 0, &mut q);
    assert_eq!(ns.tcp_rx_pending(li), 4);
    assert_eq!(ns.tcp_take_event(li), TcpNotify::Data);
    assert_eq!(q.len(), 1);
    let a = q.frame(0);
    tcp_verify(a);
    assert_eq!(t_flags(a), FLAG_ACK);
    assert_eq!(t_ack(a), 1005, "ACK advances over the 4 payload bytes");

    let mut buf = [0u8; 64];
    assert_eq!(ns.tcp_read(li, &mut buf), 4);
    assert_eq!(&buf[..4], b"ping");

    // Guest → host: queued, sent on poll, acked.
    assert!(ns.tcp_write(li, b"pong"));
    let mut q = OutQueue::new();
    ns.poll(30, &mut q);
    assert_eq!(q.len(), 1);
    let d = q.frame(0);
    tcp_verify(d);
    assert_eq!(t_flags(d), FLAG_ACK | FLAG_PSH);
    assert_eq!(t_payload(d), b"pong");
    let data_seq = t_seq(d);

    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK, 1005, data_seq + 4, 44444, 7777, &[]), 40, &mut q);
    assert!(q.is_empty(), "ACK of the data must not draw a reply");
    let mut q = OutQueue::new();
    ns.poll(900, &mut q);
    assert!(q.is_empty(), "nothing left to send after the full ACK");
}

#[test]
fn tcp_out_of_order_payload_gets_dup_ack() {
    let mut ns = guest();
    let li = established_listener(&mut ns);
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK | FLAG_PSH, 1001 + 5, 0, 44444, 7777, b"late"), 0, &mut q);
    assert_eq!(ns.tcp_rx_pending(li), 0, "out-of-order data must be dropped");
    assert_eq!(q.len(), 1);
    let a = q.frame(0);
    assert_eq!(t_ack(a), 1001, "dup ACK still points at rcv_nxt");
}

#[test]
fn tcp_rx_stream_is_capped_at_buffer() {
    let mut ns = guest();
    let li = established_listener(&mut ns);
    let big = vec![0xABu8; 600];
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK | FLAG_PSH, 1001, 0, 44444, 7777, &big), 0, &mut q);
    assert_eq!(ns.tcp_rx_pending(li), 512, "bounded by RX_BUF");
    assert_eq!(t_ack(q.frame(0)), 1001 + 600, "ack covers the whole payload");
}

// ── TCP: closing ─────────────────────────────────────────────────────────────

#[test]
fn tcp_peer_fin_then_local_close() {
    let mut ns = guest();
    let li = established_listener(&mut ns);
    let mut q = OutQueue::new();

    ns.on_frame(&host_tcp(FLAG_ACK | FLAG_FIN, 1001, 0, 44444, 7777, &[]), 0, &mut q);
    assert_eq!(q.len(), 1);
    assert_eq!(t_ack(q.frame(0)), 1002, "FIN is acked");
    assert_eq!(ns.tcp_take_event(li), TcpNotify::PeerFin);

    ns.tcp_close(li);
    let mut q = OutQueue::new();
    ns.poll(10, &mut q);
    assert_eq!(q.len(), 1);
    let fin = q.frame(0);
    tcp_verify(fin);
    assert_eq!(t_flags(fin), FLAG_FIN | FLAG_ACK);
    let fin_seq = t_seq(fin);

    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK, 1002, fin_seq + 1, 44444, 7777, &[]), 20, &mut q);
    assert!(ns.tcp_done(li), "both FINs done → socket reusable");
    assert_eq!(ns.tcp_take_event(li), TcpNotify::Closed);
    ns.tcp_free(li);
}

#[test]
fn tcp_local_close_first_then_peer_fin() {
    let mut ns = guest();
    resolve_gw(&mut ns);
    let mut q = OutQueue::new();
    let ci = ns.tcp_connect(HOST_IP, 7778, 10, &mut q).expect("connect");
    let iss = t_seq(q.frame(0));
    let sport = t_sport(q.frame(0));
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_SYN | FLAG_ACK, 50000, iss + 1, 7778, sport, &[]), 20, &mut q);
    assert_eq!(ns.tcp_take_event(ci), TcpNotify::Accepted, "connect completes");

    ns.tcp_close(ci);
    let mut q = OutQueue::new();
    ns.poll(30, &mut q);
    let fin = q.frame(0);
    assert_eq!(t_flags(fin), FLAG_FIN | FLAG_ACK);
    let fin_seq = t_seq(fin);

    // Peer acknowledges our FIN only (half-close persists).
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK, 50001, fin_seq + 1, 7778, sport, &[]), 40, &mut q);
    assert!(!ns.tcp_done(ci), "half-closed socket stays alive for peer FIN");
    assert_eq!(ns.tcp_take_event(ci), TcpNotify::None);

    // Peer closes now: whole connection finishes.
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK | FLAG_FIN, 50001, fin_seq + 1, 7778, sport, &[]), 50, &mut q);
    assert!(ns.tcp_done(ci));
    assert_eq!(ns.tcp_take_event(ci), TcpNotify::PeerFin, "peer's FIN surfaces first");
    assert_eq!(ns.tcp_take_event(ci), TcpNotify::Closed);
}

// ── TCP: retransmission & reset ──────────────────────────────────────────────

#[test]
fn tcp_retransmits_on_rto_and_aborts_exhausted() {
    let mut ns = guest();
    let li = established_listener(&mut ns);
    assert!(ns.tcp_write(li, b"data"));
    let mut q = OutQueue::new();
    ns.poll(100, &mut q);
    assert_eq!(q.len(), 1);
    let first_seq = t_seq(q.frame(0));

    // No ACK: after RTO the same segment (same seq) must go out again.
    let mut q = OutQueue::new();
    ns.poll(100 + RTO, &mut q);
    assert_eq!(q.len(), 1);
    assert_eq!(t_seq(q.frame(0)), first_seq, "retransmit keeps the original seq");

    // Keep failing → retry exhaustion marks the socket dead.
    let mut done = false;
    for i in 0..10u64 {
        let mut q = OutQueue::new();
        ns.poll(100 + RTO * (i + 2), &mut q);
        if ns.tcp_done(li) {
            done = true;
            break;
        }
    }
    assert!(done, "retry exhaustion must close the socket");
}

#[test]
fn tcp_reset_closes_socket() {
    let mut ns = guest();
    let li = established_listener(&mut ns);
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_RST | FLAG_ACK, 1001, 0, 44444, 7777, &[]), 0, &mut q);
    assert!(ns.tcp_done(li));
    assert_eq!(ns.tcp_take_event(li), TcpNotify::Closed);
}

// ── ARP / ICMP ───────────────────────────────────────────────────────────────

#[test]
fn arp_probe_and_gateway_resolution() {
    let mut ns = guest();
    let mut q = OutQueue::new();
    assert!(ns.arp_probe(&mut q));
    assert_eq!(q.len(), 1);
    let f = q.frame(0);
    assert_eq!(eth_type(f), 0x0806);
    assert_eq!(get_u16(f, 20), 1, "who-has");
    assert_eq!(&f[38..42], &HOST_IP, "probe targets the gateway");
    assert!(f[0..6].iter().all(|&b| b == 0xFF), "probe goes to broadcast");

    let mut q = OutQueue::new();
    resolve_gw(&mut ns);
    assert!(ns.gw_resolved());
}

#[test]
fn arp_who_has_for_us_gets_reply() {
    let mut ns = guest();
    let mut f = [0u8; 14 + 28];
    f[0..6].fill(0xFF);
    f[6..12].copy_from_slice(&HOST_MAC);
    put_u16(&mut f, 12, 0x0806);
    put_u16(&mut f, 14, 1);
    put_u16(&mut f, 16, 0x0800);
    f[18] = 6;
    f[19] = 4;
    put_u16(&mut f, 20, 1); // who-has
    f[22..28].copy_from_slice(&HOST_MAC);
    f[28..32].copy_from_slice(&HOST_IP);
    f[38..42].copy_from_slice(&GUEST_IP);
    let mut q = OutQueue::new();
    ns.on_frame(&f, 0, &mut q);
    assert_eq!(q.len(), 1);
    let r = q.frame(0);
    assert_eq!(eth_type(r), 0x0806);
    assert_eq!(get_u16(r, 20), 2, "reply");
    assert_eq!(&r[22..28], &GUEST_MAC);
    assert_eq!(&r[28..32], &GUEST_IP);
}

#[test]
fn icmp_echo_request_is_replied() {
    let mut ns = guest();
    let mut req = [0u8; 14 + 20 + 8 + 4];
    req[0..6].copy_from_slice(&GUEST_MAC);
    req[6..12].copy_from_slice(&HOST_MAC);
    put_u16(&mut req, 12, 0x0800);
    req[14] = 0x45;
    put_u16(&mut req, 16, 32);
    req[21] = 64;
    req[23] = IPPROTO_ICMP;
    req[26..30].copy_from_slice(&HOST_IP);
    req[30..34].copy_from_slice(&GUEST_IP);
    req[34] = 8; // echo request
    put_u16(&mut req, 38, 0x4321);
    put_u16(&mut req, 40, 7);
    req[42..46].copy_from_slice(b"ping");
    let sum = tanix_libnet::checksum::checksum(&req[34..46]);
    put_u16(&mut req, 36, sum);
    let ip_sum = tanix_libnet::checksum::checksum(&req[14..34]);
    put_u16(&mut req, 24, ip_sum);

    let mut q = OutQueue::new();
    ns.on_frame(&req, 0, &mut q);
    assert_eq!(q.len(), 1);
    let r = q.frame(0);
    assert_eq!(ip_proto(r), IPPROTO_ICMP);
    assert_eq!(ip_src(r), GUEST_IP);
    assert_eq!(ip_dst(r), HOST_IP);
    assert_eq!(r[34], 0, "echo reply type");
    assert_eq!(get_u16(r, 38), 0x4321);
    assert_eq!(get_u16(r, 40), 7);
    assert_eq!(&r[42..46], b"ping");
    assert_eq!(tanix_libnet::checksum::checksum(&r[34..46]), 0);
}

#[test]
fn ping_send_and_pong_stats() {
    let mut ns = guest();
    resolve_gw(&mut ns);
    let mut q = OutQueue::new();
    assert!(ns.icmp_echo_send(HOST_IP, &mut q));
    let f = q.frame(0);
    assert_eq!(ip_proto(f), IPPROTO_ICMP);
    assert_eq!(f[34], 8);
    assert_eq!(get_u16(f, 38), 0x1234);
    assert_eq!(get_u16(f, 40), 0);
    let payload_start = 42;

    // Echo reply from the gateway.
    let mut rep = f.to_vec();
    rep[0..6].copy_from_slice(&GUEST_MAC);
    rep[6..12].copy_from_slice(&HOST_MAC);
    rep[26..30].copy_from_slice(&HOST_IP);
    rep[30..34].copy_from_slice(&GUEST_IP);
    rep[34] = 0;
    get_u16(&rep, 36); // checksum, recomputed below
    put_u16(&mut rep, 36, 0);
    let sum = tanix_libnet::checksum::checksum(&rep[34..42 + 16]);
    put_u16(&mut rep, 36, sum);
    let ip_sum = tanix_libnet::checksum::checksum(&rep[14..34]);
    put_u16(&mut rep, 24, ip_sum);
    assert_eq!(payload_start, 42);
    assert_eq!(&rep[42..42 + 16], &f[42..42 + 16]);

    let mut q = OutQueue::new();
    ns.on_frame(&rep, 0, &mut q);
    assert_eq!(ns.pong_id, 0x1234);
    assert_eq!(ns.pong_seq, 0);
}

// ── Frame filtering ──────────────────────────────────────────────────────────

#[test]
fn frames_not_for_us_are_ignored() {
    let mut ns = guest();
    let other: [u8; 6] = [0xAA; 6];
    let mut q = OutQueue::new();
    let mut ctx = TxCtx {
        mac: HOST_MAC,
        my_ip: HOST_IP,
        ip_id: &mut 42,
        q: &mut q,
    };
    push_tcp_frame(&mut ctx, other, GUEST_IP, 1111, 2222, 1, 0, FLAG_SYN, 1024, b"x");
    let f = q.frame(0).to_vec();
    let mut q = OutQueue::new();
    ns.on_frame(&f, 0, &mut q);
    assert!(q.is_empty(), "frame for another MAC must be dropped");
    assert!(!ns.gw_resolved());
}

// ── Socket table limits ──────────────────────────────────────────────────────

#[test]
fn socket_table_bounds() {
    let mut ns = guest();
    for port in [7777u16, 7778, 7779, 7780] {
        assert!(ns.tcp_listen(port).is_some());
    }
    assert!(ns.tcp_listen(8888).is_none(), "TCP table is exhausted");
    for port in [5555u16, 5556, 5557, 5558] {
        assert!(ns.udp_bind(port).is_some());
    }
    assert!(ns.udp_bind(9999).is_none(), "UDP table is exhausted");
    assert!(ns.udp_bind(5555).is_none(), "port already bound");
    assert!(ns.tcp_listen(7777).is_none(), "port already listening");
}

#[test]
fn tcp_write_rejects_invalid_buffer_drop_is_silent() {
    let mut ns = guest();
    let li = ns.tcp_listen(7777).expect("listen");
    // Not established: write must fail.
    assert!(!ns.tcp_write(li, b"early"));
    let mut q = OutQueue::new();
    assert!(q.is_empty());
}
#[test]
fn tcp_ack_then_separate_data_segment() {
    let mut ns = guest();
    resolve_gw(&mut ns);
    let mut q = OutQueue::new();
    let li = ns.tcp_listen(7777).expect("listen");
    ns.on_frame(&host_tcp(FLAG_SYN, 9000, 0, 44444, 7777, &[]), 10, &mut q);
    let synack = q.frame(0);
    let iss = t_seq(synack);
    let mut q = OutQueue::new();

    // Client ACK alone (no payload) completes the handshake.
    ns.on_frame(&host_tcp(FLAG_ACK, 9001, iss + 1, 44444, 7777, &[]), 20, &mut q);
    assert_eq!(ns.tcp_take_event(li), TcpNotify::Accepted);
    assert_eq!(q.len(), 0, "nothing to reply to a bare ACK");

    // Client data in a separate segment.
    ns.on_frame(&host_tcp(FLAG_ACK | FLAG_PSH, 9001, iss + 1, 44444, 7777, b"ping"), 30, &mut q);
    assert_eq!(ns.tcp_take_event(li), TcpNotify::Data, "data must be delivered");
    let mut buf = [0u8; 16];
    assert_eq!(ns.tcp_read(li, &mut buf), 4);
    assert_eq!(&buf[..4], b"ping");
}

#[test]
fn tcp_data_and_fin_in_one_segment() {
    // macOS nc (SHUT_WR) coalesces payload + FIN into a single segment.
    let mut ns = guest();
    resolve_gw(&mut ns);
    let mut q = OutQueue::new();
    let li = ns.tcp_listen(7777).expect("listen");
    ns.on_frame(&host_tcp(FLAG_SYN, 9000, 0, 44444, 7777, &[]), 10, &mut q);
    let iss = t_seq(q.frame(0));
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK, 9001, iss + 1, 44444, 7777, &[]), 20, &mut q);
    assert_eq!(ns.tcp_take_event(li), TcpNotify::Accepted);

    // Data + FIN in one segment: Data must be delivered BEFORE PeerFin.
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK | FLAG_PSH | FLAG_FIN, 9001, iss + 1, 44444, 7777, b"tanix-hello-tcp\n"), 30, &mut q);
    assert_eq!(ns.tcp_take_event(li), TcpNotify::Data, "data first");
    let mut buf = [0u8; 64];
    assert_eq!(ns.tcp_read(li, &mut buf), 16);
    assert_eq!(&buf[..16], b"tanix-hello-tcp\n");
    assert_eq!(ns.tcp_take_event(li), TcpNotify::PeerFin, "then FIN");
    ns.tcp_close(li); // app replies by closing (FIN goes out post-echo-ACK)
    assert!(ns.tcp_write(li, b"echo"), "echo still writable (CloseWait)");
    let mut q = OutQueue::new();
    ns.poll(40, &mut q);
    assert_eq!(q.len(), 1, "echo frame goes out");
    assert_eq!(t_payload(q.frame(0)), b"echo");
    // Peer ACKs the echo; only then does the app's FIN leave.
    let mut q = OutQueue::new();
    ns.on_frame(&host_tcp(FLAG_ACK, 9018, iss + 1 + 4, 44444, 7777, &[]), 45, &mut q);
    let mut q = OutQueue::new();
    ns.poll(50, &mut q);
    assert_eq!(q.len(), 1, "app FIN follows the acked echo");
    assert_eq!(t_flags(q.frame(0)), FLAG_FIN | FLAG_ACK, "app FIN follows");
}
