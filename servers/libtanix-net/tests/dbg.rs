#![no_std]
extern crate std;

#[test]
fn dbg_vec() {
    use std::println;
    use tanix_libnet::checksum::{checksum_seed, pseudo_seed};
    let guest = [10, 0, 2, 15];
    let host = [10, 0, 2, 2];
    let seed = pseudo_seed(&guest, &host, 17, 13);
    let mut udp = [0u8; 13];
    udp[0] = 0x15; udp[1] = 0xB3;
    udp[2] = 0x15; udp[3] = 0xB5;
    udp[5] = 13;
    udp[8..].copy_from_slice(b"hello");
    println!("seed={:#x} sum={:#06x}", seed, checksum_seed(&udp, seed));
}
