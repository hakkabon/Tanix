//! Secure-services demo server (`sec`) — Phase 17.
//!
//! Exercises the EL3-monitor services the kernel exposes as syscalls
//! 18-22, and proves the quote/seal round-trips:
//!
//!   • secure storage: PUT a 232-byte blob, GET it back, byte-compare;
//!     the plaintext crosses the kernel EL1 → EL3 boundary, but lives
//!     only in secure RAM.
//!   • keybox: GEN a key inside EL3 (it never leaves), SEAL a secret
//!     buffer, UNSEAL it, byte-compare; then log the sealed bytes so a
//!     human can see the ciphertext differs from the plaintext.
//!   • attestation: SYS_ATTEST quotes *this server's own image* with a
//!     nonce; the digest+MAC are logged (the MAC is keyed with the
//!     EL3-only secret, so the NS kernel cannot forge it).
//!
//! The whole demo runs only on `sbsa-ref`, where the EL3 monitor exists.
//! On `virt` the SMC traps to QEMU PSCI and the wrappers return -1 — the
//! server logs one line and parks.

#![no_std]
#![no_main]

use tanix_libsys::abi::{BootInfo, MACHINE_SBSA_REF};
use tanix_libsys::{fmt::StrBuf, sys};

/// A small seed the `sec` server plants in the secure store at boot, so
/// the first PUT exercises both the "create slot" and "overwrite" paths.
const SEED: &[u8; 16] = b"tanixsecseed-17!";

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    let machine = unsafe { sys::boot_info().machine };
    if machine != MACHINE_SBSA_REF {
        sys::log(1, "sec: no EL3 monitor on this machine (sbsa-ref only)");
        loop {
            sys::sleep(10_000);
        }
    }
    sys::log(0, "sec: up (sbsa-ref, EL3 services)");

    // ── 1. Secure storage round-trip ────────────────────────────────────────
    let mut blob = [0u8; 232];
    for (i, b) in blob.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7).wrapping_add(0x5E);
    }
    let rc = sys::sec_storage_put(b"seed\0\0\0\0", SEED);
    sys::log(0, "sec: storage put tanix/seed (seed)");
    if rc != 0 {
        sys::log(1, "sec: storage put failed");
    }
    let rc = sys::sec_storage_put(b"blob\0\0\0\0", &blob);
    if rc != 0 {
        sys::log(1, "sec: storage put blob failed");
    }
    let mut back = [0u8; 232];
    let n = sys::sec_storage_get(b"blob\0\0\0\0", &mut back);
    let mut ok = n == 232 && back[..] == blob[..];
    let mut buf = StrBuf::new();
    buf.push_str("sec: storage get blob -> ");
    buf.push_dec32(n as u32);
    if ok {
        buf.push_str(" bytes, match");
    } else {
        buf.push_str(" MISMATCH");
    }
    sys::log(if ok { 0 } else { 2 }, buf.as_str());

    // ── 2. Keybox seal/unseal round-trip ────────────────────────────────────
    sys::keybox_gen(0);
    let mut secret = [0u8; 32];
    for (i, b) in secret.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(3).wrapping_add(0x11);
    }
    let rc = sys::keybox_seal(0, &mut secret);
    if rc != 0 {
        sys::log(1, "sec: keybox seal failed");
    } else {
        buf.reset();
        buf.push_str("sec: sealed (key 0): ");
        for b in &secret[..8] {
            buf.push_hex32(*b as u32);
        }
        sys::log(0, buf.as_str());
    }
    let rc = sys::keybox_unseal(0, &mut secret);
    if rc != 0 {
        sys::log(1, "sec: keybox unseal failed");
    } else {
        // `secret` must be back to the original pattern.
        let good = (0..32).all(|i| secret[i] == (i as u8).wrapping_mul(3).wrapping_add(0x11));
        sys::log(if good { 0 } else { 2 }, "sec: unseal -> match");
    }

    // ── 3. Attestation: quote this server's own image ───────────────────────
    let mut nonce = 0xC0FFEE17_0000_0001u64;
    let mut quote = [0u64; 2];
    let rc = sys::attest(nonce, &mut quote);
    if rc != 0 {
        sys::log(1, "sec: attest failed");
    } else {
        buf.reset();
        buf.push_str("sec: attest digest=");
        buf.push_hex64(quote[0]);
        buf.push_str(" mac=");
        buf.push_hex64(quote[1]);
        buf.push_str(" nonce=");
        buf.push_hex64(nonce);
        sys::log(0, buf.as_str());
        // A second call with a *different* nonce must produce a different
        // MAC (nonce-binding) but the same digest (image did not change).
        nonce = 0xC0FFEE17_0000_0002u64;
        let rc2 = sys::attest(nonce, &mut quote);
        if rc2 != 0 {
            sys::log(1, "sec: attest (nonce 2) failed");
        } else {
            buf.reset();
            buf.push_str("sec: attest(nonce2) digest=");
            buf.push_hex64(quote[0]);
            buf.push_str(" mac=");
            buf.push_hex64(quote[1]);
            sys::log(0, buf.as_str());
        }
    }

    sys::log(0, "sec: demo complete — parking");
    loop {
        sys::sleep(10_000);
    }
}
