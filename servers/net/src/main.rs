//! Phase-11 network server (`net`): the socket layer (TCP/UDP) from
//! `tanix-libnet` over virtio-net, driven by the NIC's INTx line through
//! the kernel's `SYS_IRQ_PENDING` poll.
//!
//! Wire layout (driven by `scripts/net-test.sh` through QEMU user-mode
//! networking / slirp):
//!
//!   • UDP 5555  — host→guest datagrams (hostfwd), logged on arrival
//!   • UDP 5557  — guest→host marker datagrams (to the gateway)
//!   • TCP 7777  — host→guest echo listener (hostfwd)
//!   • TCP 7778  — guest→host outbound connection (to the gateway)
//!
//! The old phase-10 ARP/ping health check is retained: the gateway is
//! resolved with ARP who-has probes, then pinged every 2 s, logging each
//! reply's id/seq.
//!
//! The event loop never parks: it polls the IRQ line, sleeps 50 ms
//! between rounds, drains the RX ring, flushes the stack's OutQueue, and
//! drives the demo apps on a tick cadence.

#![no_std]
#![no_main]

use tanix_libdrv::net::VirtioNet;
use tanix_libnet::out::OutQueue;
use tanix_libnet::{NetStack, TcpNotify};
use tanix_libsys::abi::BootInfo;
use tanix_libsys::fmt::StrBuf;
use tanix_libsys::sys;

// Slirp's guest subnet: us, and the host gateway/router.
const MY_IP: [u8; 4] = [10, 0, 2, 15];
const GW_IP: [u8; 4] = [10, 0, 2, 2];

// Wire ports (see module docs).
const UDP_IN_PORT: u16 = 5555;
const UDP_OUT_PORT: u16 = 5557;
const TCP_ECHO_PORT: u16 = 7777;
const TCP_GO_PORT: u16 = 7778;

// One event-loop round is 50 ms; long periods are multiples of that.
const LOOP_MS: u64 = 50;

static mut NET: NetStack = NetStack::new(MY_IP, GW_IP);
static mut RXBUF: [u8; 1518] = [0; 1518];
static mut ECHOBUF: [u8; 512] = [0; 512];

/// Log a line built from static text and `push_dec32`-able numbers.
macro_rules! klog {
    () => {};
    ($lit:literal) => {
        sys::log(0, $lit);
    };
    ($lit:literal, $($arg:expr),* $(,)?) => {{
        let mut b = StrBuf::new();
        b.push_str($lit);
        $(
            b.push_str(" ");
            b.push_dec32($arg as u32);
        )*
        sys::log(0, b.as_str());
    }};
}

/// Send everything the stack queued, frame by frame, through the NIC.
/// The NIC has a single TX slot: when it is still in flight, advance the
/// TX ring until the device consumes it, then send (never drop frames).
fn flush(net: &mut VirtioNet, q: &mut OutQueue) {
    for i in 0..q.len() {
        loop {
            if net.send(q.frame(i)) {
                break;
            }
            net.poll_tx();
        }
    }
    q.clear();
}

/// Append the decimal digits of `v` to `buf`; returns the bytes written.
fn put_dec(buf: &mut [u8], mut v: u32) -> usize {
    let mut d = [0u8; 10];
    let mut i = d.len();
    loop {
        i -= 1;
        d[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let n = d.len() - i;
    buf[..n].copy_from_slice(&d[i..]);
    n
}

/// `tanix-udp-<n>` marker sent to the host's UDP sink.
fn udp_marker(buf: &mut [u8], n: u32) -> usize {
    let head = b"tanix-udp-";
    buf[..head.len()].copy_from_slice(head);
    head.len() + put_dec(&mut buf[head.len()..], n)
}

/// `tanix-tcp-<n>\n` marker sent to the host's TCP sink.
fn tcp_marker(buf: &mut [u8], n: u32) -> usize {
    let head = b"tanix-tcp-";
    buf[..head.len()].copy_from_slice(head);
    let d = head.len() + put_dec(&mut buf[head.len()..], n);
    buf[d] = b'\n';
    d + 1
}

/// Append `a.b.c.d` to `b` (for log lines).
fn push_ip(b: &mut StrBuf, ip: &[u8; 4]) {
    b.push_dec32(ip[0] as u32);
    b.push_str(".");
    b.push_dec32(ip[1] as u32);
    b.push_str(".");
    b.push_dec32(ip[2] as u32);
    b.push_str(".");
    b.push_dec32(ip[3] as u32);
}

// ── Server main ──────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "net: up");

    let mut net = match VirtioNet::open() {
        Some(n) => n,
        None => {
            sys::log(1, "net: no virtio-net device — idling");
            loop {
                let _ = sys::receive(tanix_libsys::abi::M_ANY);
            }
        }
    };

    unsafe {
        NET.set_mac(net.mac);
    }
    let sock = unsafe { &mut *core::ptr::addr_of_mut!(NET) };

    // Socket layer up: the echo listener + both UDP endpoints.
    let mut echo_listener = sock.tcp_listen(TCP_ECHO_PORT);
    let udp_in = sock.udp_bind(UDP_IN_PORT);
    let udp_out = sock.udp_open();
    let mut s = StrBuf::new();
    s.push_str("net: socket layer up tcp_echo=");
    match echo_listener {
        Some(_) => s.push_str("yes"),
        None => s.push_str("NO_SLOT"),
    }
    s.push_str(" udp_in=");
    match udp_in {
        Some(_) => s.push_str("yes"),
        None => s.push_str("NO_SLOT"),
    }
    s.push_str(" udp_out=");
    match udp_out {
        Some(_) => s.push_str("yes"),
        None => s.push_str("NO_SLOT"),
    }
    sys::log(0, s.as_str());

    let irq = net.dev.irq;
    sys::log(0, "net: IRQ-armed, event loop started");

    // Demo-app state.
    let mut gw_resolved_logged = false;
    let mut ping_seq: u32 = 0;
    let mut last_pong: u16 = 0;
    let mut udp_marker_n: u32 = 0;
    let mut conn: Option<usize> = None;
    let mut conn_established_at: u64 = 0;
    let mut tcp_marker_n: u32 = 0;

    let mut ticks: u64 = 0;
    let mut loop_no: u64 = 0;
    loop {
        sys::sleep(50);
        ticks += LOOP_MS;
        loop_no += 1;

        // Deassert the INTx line.
        if sys::irq_pending(irq) == 1 {
            let _ = net.dev.read_isr();
        }
        net.poll_tx();

        // Drain the RX ring into the stack.
        let mut q = OutQueue::new();
        let rx = unsafe { &mut *core::ptr::addr_of_mut!(RXBUF) };
        while let Some(n) = net.recv(rx) {
            sock.on_frame(&rx[..n], ticks, &mut q);
            flush(&mut net, &mut q);
        }

        // Timers (retransmits, queued data, FINs).
        sock.poll(ticks, &mut q);
        flush(&mut net, &mut q);

        // ── ARP probe until the gateway resolves ──
        if !sock.gw_resolved() {
            if ticks % (10 * LOOP_MS) == 0 {
                if sock.arp_probe(&mut q) {
                    sys::log(0, "net: ARP who-has 10.0.2.2 sent");
                    flush(&mut net, &mut q);
                }
            }
        } else if !gw_resolved_logged {
            gw_resolved_logged = true;
            sys::log(0, "net: gateway resolved");
        }

        // ── Ping the gateway every 2 s; log replies on change ──
        if sock.gw_resolved() && ticks % (40 * LOOP_MS) == 0 {
            if sock.icmp_echo_send(GW_IP, &mut q) {
                klog!("net: ping sent seq", ping_seq);
                ping_seq = ping_seq.wrapping_add(1);
                flush(&mut net, &mut q);
            }
        }
        if sock.pong_seq != last_pong {
            last_pong = sock.pong_seq;
            if sock.pong_seq != 0 {
                let mut b = StrBuf::new();
                b.push_str("net: ping reply id ");
                b.push_dec32(sock.pong_id as u32);
                b.push_str(" seq ");
                b.push_dec32(sock.pong_seq as u32);
                sys::log(0, b.as_str());
            }
        }

        // ── UDP in: log every datagram on port 5555 ──
        if let Some(idx) = udp_in {
            if let Some(rx_info) = sock.udp_rx_info(idx) {
                let mut d = [0u8; 64];
                let n = sock.udp_unread(idx, &mut d);
                let mut b = StrBuf::new();
                b.push_str("net: UDP_RX port ");
                b.push_dec32(UDP_IN_PORT as u32);
                b.push_str(" len ");
                b.push_dec32(n as u32);
                b.push_str(" from");
                push_ip(&mut b, &rx_info.peer_ip);
                b.push_str(":");
                b.push_dec32(rx_info.peer_port as u32);
                sys::log(0, b.as_str());
            }
        }

        // ── UDP out: marker datagram to 127.0.0.1:5557 every 4 s ──
        if sock.gw_resolved() && ticks % (80 * LOOP_MS) == 0 {
            if let Some(idx) = udp_out {
                let mut m = [0u8; 32];
                let n = udp_marker(&mut m, udp_marker_n);
                if sock.udp_send(idx, GW_IP, UDP_OUT_PORT, &m[..n], &mut q) {
                    let mut b = StrBuf::new();
                    b.push_str("net: UDP_TX port ");
                    b.push_dec32(UDP_OUT_PORT as u32);
                    b.push_str(" len ");
                    b.push_dec32(n as u32);
                    b.push_str(" to");
                    push_ip(&mut b, &GW_IP);
                    b.push_str(":");
                    b.push_dec32(UDP_OUT_PORT as u32);
                    sys::log(0, b.as_str());
                    udp_marker_n = udp_marker_n.wrapping_add(1);
                    flush(&mut net, &mut q);
                }
            }
        }

        // ── TCP echo listener on 7777 ──
        if let Some(li) = echo_listener {
            loop {
                match sock.tcp_take_event(li) {
                    TcpNotify::Accepted => {
                        let (ip, p) = sock.tcp_peer(li);
                        let mut b = StrBuf::new();
                        b.push_str("net: TCP_ACCEPT port ");
                        b.push_dec32(TCP_ECHO_PORT as u32);
                        b.push_str(" from ");
                        push_ip(&mut b, &ip);
                        b.push_str(":");
                        b.push_dec32(p as u32);
                        sys::log(0, b.as_str());
                    }
                    TcpNotify::Data => {
                        let buf = unsafe { &mut *core::ptr::addr_of_mut!(ECHOBUF) };
                        let n = sock.tcp_read(li, buf);
                        if n > 0 && sock.tcp_write(li, &buf[..n]) {
                            klog!("net: TCP_ECHO n", n);
                        }
                    }
                    TcpNotify::PeerFin => {
                        sys::log(0, "net: TCP_PEER_FIN echo");
                        sock.tcp_close(li);
                    }
                    TcpNotify::Closed => {
                        sys::log(0, "net: TCP_CLOSED echo");
                        sock.tcp_free(li);
                        echo_listener = None;
                    }
                    TcpNotify::None => break,
                }
            }
        } else {
            echo_listener = sock.tcp_listen(TCP_ECHO_PORT);
            if echo_listener.is_some() {
                klog!("net: TCP_LISTEN port", TCP_ECHO_PORT);
            }
        }

        // ── TCP out: connect to 127.0.0.1:7778 every 15 s ──
        if conn.is_none() && sock.gw_resolved() && ticks % (300 * LOOP_MS) == 0 {
            if let Some(idx) = sock.tcp_connect(GW_IP, TCP_GO_PORT, ticks, &mut q) {
                conn = Some(idx);
                tcp_marker_n = 0;
                sys::log(0, "net: TCP_CONNECT to 10.0.2.2:7778");
            }
            flush(&mut net, &mut q);
        }
        if let Some(idx) = conn {
            loop {
                match sock.tcp_take_event(idx) {
                    TcpNotify::Accepted => {
                        conn_established_at = ticks;
                        sys::log(0, "net: TCP_ESTABLISHED out");
                    }
                    TcpNotify::Data => {
                        let buf = unsafe { &mut *core::ptr::addr_of_mut!(ECHOBUF) };
                        let n = sock.tcp_read(idx, buf);
                        klog!("net: TCP_RX n", n);
                    }
                    TcpNotify::PeerFin => {
                        sys::log(0, "net: TCP_PEER_FIN out");
                        sock.tcp_close(idx);
                    }
                    TcpNotify::Closed => {
                        sys::log(0, "net: TCP_CLOSED out");
                        sock.tcp_free(idx);
                        conn = None;
                    }
                    TcpNotify::None => break,
                }
            }
            // While established: one marker per round, then close after 5 s.
            if let Some(idx) = conn {
                if sock.tcp_is_established(idx) {
                    if tcp_marker_n < 3 {
                        let mut m = [0u8; 32];
                        let n = tcp_marker(&mut m, tcp_marker_n);
                        if sock.tcp_write(idx, &m[..n]) {
                            klog!("net: TCP_SENT n", n);
                            tcp_marker_n += 1;
                        }
                    } else if ticks >= conn_established_at + 5 * 1000 {
                        sys::log(0, "net: TCP_CLOSE_REQ out");
                        sock.tcp_close(idx);
                        tcp_marker_n = 100; // sent
                    }
                }
            }
        }

        // Nudge the CPU cycle counter the host may sample for liveness.
        if loop_no % 2000 == 0 {
            sys::log(0, "net: alive");
        }
    }
}