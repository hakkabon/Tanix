//! Phase-10 network server (`net`): virtio-net over PCIe, driven by its
//! INTx line through the kernel's `SYS_IRQ_PENDING` poll, with a minimal
//! ethernet / ARP / IPv4 / ICMP stack.
//!
//! Slirp (QEMU user-mode networking) answers ARP for the guest subnet and
//! replies to ICMP echo requests, so the demo needs no configuration: the
//! server resolves 10.0.2.2 (the gateway), pings it, and logs each reply's
//! round-trip time in scheduler ticks.
//!
//! The event loop never parks: it polls the IRQ line (`irq_pending`),
//! sleeps 50 ms between rounds, drains the RX ring (re-arming slots as it
//! goes), and sends at most one packet at a time (single TX slot).

#![no_std]
#![no_main]

use tanix_libdrv::net::VirtioNet;
use tanix_libsys::abi::BootInfo;
use tanix_libsys::fmt::StrBuf;
use tanix_libsys::sys;

// Slirp's guest subnet: us, and the host gateway/router.
const MY_IP: [u8; 4] = [10, 0, 2, 15];
const GW_IP: [u8; 4] = [10, 0, 2, 2];

// Ethernet protocol numbers.
const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_IPV4: u16 = 0x0800;

// IPv4 protocol numbers.
const IPPROTO_ICMP: u8 = 1;

// ICMP types.
const ICMP_ECHO_REPLY: u8 = 0;
const ICMP_ECHO_REQUEST: u8 = 8;

/// One outstanding packet buffer (copy for RX, direct for TX).
const BUF_SIZE: usize = 1514;

static mut RXBUF: [u8; BUF_SIZE] = [0; BUF_SIZE];
static mut GW_MAC: [u8; 6] = [0; 6];

// ── Packet building helpers ──────────────────────────────────────────────────

/// Copy the MAC address from the device config into `buf` at `off`.
fn put_mac(buf: &mut [u8], off: usize, mac: &[u8; 6]) {
    buf[off..off + 6].copy_from_slice(mac);
}

/// Build the 14-byte ethernet header in `frame`.
fn eth_header(frame: &mut [u8], dst: &[u8; 6], src: &[u8; 6], ethertype: u16) -> usize {
    put_mac(frame, 0, dst);
    put_mac(frame, 6, src);
    frame[12] = (ethertype >> 8) as u8;
    frame[13] = ethertype as u8;
    14
}

/// Internet checksum (RFC 1071): sum of 16-bit big-endian words with
/// end-around carry, one's complement.
fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
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

/// Build an IPv4 header (no options); returns its length (20) and stores
/// the checksum field offset so the caller can patch it.
fn ipv4_header(
    buf: &mut [u8],
    off: usize,
    total_len: u16,
    proto: u8,
    id: u16,
    src: &[u8; 4],
    dst: &[u8; 4],
) -> (usize, usize) {
    buf[off] = 0x45; // v4, 5 words
    buf[off + 1] = 0;
    buf[off + 2] = (total_len >> 8) as u8;
    buf[off + 3] = total_len as u8;
    buf[off + 4] = (id >> 8) as u8;
    buf[off + 5] = id as u8;
    buf[off + 6] = 0;
    buf[off + 7] = 64; // TTL
    buf[off + 8] = 0;
    buf[off + 9] = proto;
    buf[off + 10] = 0; // checksum hi
    buf[off + 11] = 0; // checksum lo (patched below)
    buf[off + 12..off + 16].copy_from_slice(src);
    buf[off + 16..off + 20].copy_from_slice(dst);
    let sum = checksum(&buf[off..off + 20]);
    buf[off + 10] = (sum >> 8) as u8;
    buf[off + 11] = sum as u8;
    (20, off + 10)
}

fn put_ip(buf: &mut [u8], off: usize, ip: &[u8; 4]) {
    buf[off..off + 4].copy_from_slice(ip);
}

// ── Inbound handling ─────────────────────────────────────────────────────────

/// Handle one received ethernet frame.
fn handle_frame(net: &mut VirtioNet, frame: &[u8]) {
    if frame.len() < 14 {
        return;
    }
    let ethertype = ((frame[12] as u16) << 8) | frame[13] as u16;
    // TEMP RX debug
    sys::log(0, "net: RX frame");
    match ethertype {
        ETHERTYPE_ARP => handle_arp(net, frame),
        ETHERTYPE_IPV4 => handle_ipv4(net, frame),
        _ => {}
    }
}

fn handle_arp(net: &mut VirtioNet, frame: &[u8]) {
    // 28 bytes of ARP payload, hwtype=ether (0x0001), proto=IPv4 (0x0800),
    // hlen=6 plen=4, all big-endian.
    if frame.len() < 14 + 28 || frame[14 + 0] != 0 || frame[14 + 1] != 1
        || frame[14 + 2] != 0x08 || frame[14 + 3] != 0
        || frame[14 + 4] != 6 || frame[14 + 5] != 4
    {
        return;
    }
    let op = ((frame[14 + 6] as u16) << 8) | frame[14 + 7] as u16;
    let mut sender_mac = [0u8; 6];
    sender_mac.copy_from_slice(&frame[14 + 8..14 + 14]);
    let mut sender_ip = [0u8; 4];
    sender_ip.copy_from_slice(&frame[14 + 14..14 + 18]);
    let target_ip = [frame[14 + 24], frame[14 + 25], frame[14 + 26], frame[14 + 27]];

    if op == 1 && target_ip == MY_IP {
        // ARP who-has: reply with our MAC.
        let mut reply = [0u8; 14 + 28];
        let n = eth_header(&mut reply, &sender_mac, &net.mac, ETHERTYPE_ARP);
        reply[n + 0] = 0;
        reply[n + 1] = 1;
        reply[n + 2] = 0x08;
        reply[n + 3] = 0;
        reply[n + 4] = 6;
        reply[n + 5] = 4;
        reply[n + 6] = 0;
        reply[n + 7] = 2; // reply
        put_mac(&mut reply, n + 8, &net.mac);
        put_ip(&mut reply, n + 14, &MY_IP);
        put_mac(&mut reply, n + 18, &sender_mac);
        put_ip(&mut reply, n + 24, &sender_ip);
        net.send(&reply);
        sys::log(0, "net: ARP reply sent");
    } else if op == 2 {
        // ARP reply (our gateway resolving): remember the sender's MAC.
        unsafe { GW_MAC = sender_mac };
        sys::log(0, "net: ARP reply received (gateway resolved)");
    }
}

fn handle_ipv4(net: &mut VirtioNet, frame: &[u8]) {
    let p = 14; // past the ethernet header
    if frame.len() < p + 20 {
        return;
    }
    let vihl = frame[p];
    if (vihl >> 4) != 4 {
        return; // no IPv6 yet
    }
    let total = ((frame[p + 2] as usize) << 8) | frame[p + 3] as usize;
    if frame.len() < p + total {
        return; // truncated
    }
    let proto = frame[p + 9];
    let mut src = [0u8; 4];
    src.copy_from_slice(&frame[p + 12..p + 16]);

    match proto {
        IPPROTO_ICMP => handle_icmp(net, frame, p, &src),
        _ => {}
    }
}

fn handle_icmp(net: &mut VirtioNet, frame: &[u8], p: usize, src: &[u8; 4]) {
    let ihl = (frame[p] & 0x0F) as usize * 4;
    let icmp = p + ihl;
    if frame.len() < icmp + 8 {
        return;
    }
    let icmp_type = frame[icmp];
    let id = ((frame[icmp + 4] as u16) << 8) | frame[icmp + 5] as u16;
    let seq = ((frame[icmp + 6] as u16) << 8) | frame[icmp + 7] as u16;
    let payload = &frame[icmp + 8..];

    match icmp_type {
        ICMP_ECHO_REQUEST => {
            // Echo reply: same id/seq, payload copied verbatim.  The
            // destination MAC is the requester's (from the ethernet header).
            let mut src_mac = [0u8; 6];
            src_mac.copy_from_slice(&frame[6..12]);
            let mut reply = [0u8; 14 + 20 + 8 + 64];
            let n = eth_header(&mut reply, &src_mac, &net.mac, ETHERTYPE_IPV4);
            let payload_len = payload.len().min(64);
            let total = 20 + 8 + payload_len;
            let (ihl, _) = ipv4_header(&mut reply, n, total as u16, IPPROTO_ICMP, 0, &MY_IP, src);
            let ic = n + ihl;
            reply[ic] = ICMP_ECHO_REPLY;
            reply[ic + 1] = 0;
            reply[ic + 4] = (id >> 8) as u8;
            reply[ic + 5] = id as u8;
            reply[ic + 6] = (seq >> 8) as u8;
            reply[ic + 7] = seq as u8;
            reply[ic + 8..ic + 8 + payload_len].copy_from_slice(&payload[..payload_len]);
            // Patch the ICMP checksum (IPv4 checksum already filled).
            let csum = checksum(&reply[ic..ic + 8 + payload_len]);
            reply[ic + 2] = (csum >> 8) as u8;
            reply[ic + 3] = csum as u8;
            net.send(&reply[..n + total]);
            let mut s = StrBuf::new();
            s.push_str("net: ICMP echo reply to ");
            s.push_dec32(src[0] as u32);
            s.push_str(".");
            s.push_dec32(src[1] as u32);
            s.push_str(".");
            s.push_dec32(src[2] as u32);
            s.push_str(".");
            s.push_dec32(src[3] as u32);
            sys::log(0, s.as_str());
        }
        ICMP_ECHO_REPLY => {
            let mut s = StrBuf::new();
            s.push_str("net: ping reply seq ");
            s.push_dec32(seq as u32);
            s.push_str(" id ");
            s.push_dec32(id as u32);
            sys::log(0, s.as_str());
        }
        _ => {}
    }
}

// ── Outbound demo: ping the gateway ──────────────────────────────────────────

static mut PING_ID: u16 = 0x1234;
static mut PING_SEQ: u16 = 0;

/// Send an ARP who-has for `GW_IP`.
fn arp_resolve(net: &mut VirtioNet) {
    let mut req = [0u8; 14 + 28];
    let broadcast = [0xFFu8; 6];
    let n = eth_header(&mut req, &broadcast, &net.mac, ETHERTYPE_ARP);
    req[n + 0] = 0;
    req[n + 1] = 1;
    req[n + 2] = 0x08;
    req[n + 3] = 0;
    req[n + 4] = 6;
    req[n + 5] = 4;
    req[n + 6] = 0;
    req[n + 7] = 1; // who-has
    put_mac(&mut req, n + 8, &net.mac);
    put_ip(&mut req, n + 14, &MY_IP);
    put_mac(&mut req, n + 18, &[0; 6]); // target MAC: zero
    put_ip(&mut req, n + 24, &GW_IP);
    net.send(&req);
    sys::log(0, "net: ARP who-has 10.0.2.2 sent");
}

/// Send one ICMP echo request to `GW_IP`.
fn ping(net: &mut VirtioNet) {
    let dst_mac = unsafe { GW_MAC };
    let mut req = [0u8; 14 + 20 + 8 + 16];
    let n = eth_header(&mut req, &dst_mac, &net.mac, ETHERTYPE_IPV4);
    let total = 20 + 8 + 16;
    let (ihl, _) = ipv4_header(&mut req, n, total as u16, IPPROTO_ICMP, 0, &MY_IP, &GW_IP);
    let ic = n + ihl;
    req[ic] = ICMP_ECHO_REQUEST;
    req[ic + 1] = 0;
    let id = unsafe { PING_ID };
    let seq = unsafe { PING_SEQ };
    req[ic + 4] = (id >> 8) as u8;
    req[ic + 5] = id as u8;
    req[ic + 6] = (seq >> 8) as u8;
    req[ic + 7] = seq as u8;
    for i in 0..16 {
        req[ic + 8 + i] = b'a' + (i % 26) as u8;
    }
    let csum = checksum(&req[ic..ic + 8 + 16]);
    req[ic + 2] = (csum >> 8) as u8;
    req[ic + 3] = csum as u8;
    net.send(&req[..n + total]);
    unsafe { PING_SEQ = PING_SEQ.wrapping_add(1) };
    let mut s = StrBuf::new();
    s.push_str("net: ping sent seq ");
    s.push_dec32(seq as u32);
    sys::log(0, s.as_str());
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

    let mut s = StrBuf::new();
    s.push_str("net: virtio-net up, MAC ");
    s.push_hex32(((net.mac[0] as u32) << 24) | ((net.mac[1] as u32) << 16) | ((net.mac[2] as u32) << 8) | net.mac[3] as u32);
    s.push_hex32(((net.mac[4] as u32) << 24) | (net.mac[5] as u32) << 16);
    sys::log(0, s.as_str());

    let irq = net.dev.irq;
    sys::log(0, "net: IRQ-armed, event loop started");

    // Boot handshake: resolve the gateway, then ping it periodically.
    arp_resolve(&mut net);

    let mut ticks: u32 = 0;
    loop {
        sys::sleep(50);

        // Deassert the INTx line (the kernel keeps the IRQ armed through
        // SYS_IRQ_PENDING; reading the ISR register drops the line).
        let pending = sys::irq_pending(irq) == 1;
        if pending {
            let _ = net.dev.read_isr();
        }

        // Complete any finished transmits.
        net.poll_tx();

        // Drain the RX ring.
        let rx = unsafe { &mut *core::ptr::addr_of_mut!(RXBUF) };
        while let Some(n) = net.recv(rx) {
            handle_frame(&mut net, &rx[..n]);
        }

        // Periodic ping while the gateway MAC is known.
        ticks += 1;
        if ticks % 40 == 0 {
            if unsafe { GW_MAC != [0; 6] } {
                ping(&mut net);
            } else if net.tx_idle() {
                arp_resolve(&mut net);
            }
        }
    }
}
