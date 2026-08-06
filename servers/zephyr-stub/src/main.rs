//! Zephyr RTOS stub — Phase 3 guest VM with VirtIO driver.
//!
//! The guest receives the shared-memory base address in x1 at launch.
//! It reads the `VirtqueueConfig` header, then runs the VirtIO device loop:
//!
//!   1. Wait for the kernel to post a buffer to the avail ring (signalled by
//!      SGI 1 doorbell).
//!   2. Read the buffer — expect a `Print` opcode.
//!   3. Print the payload string to the UART.
//!   4. Write an `Echo` reply into the same buffer slot.
//!   5. Post the descriptor to the used ring.
//!   6. Fire SGI 1 back to the kernel.
//!   7. Repeat until the kernel signals shutdown (magic cleared) or 16 rounds.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

// ── UART ─────────────────────────────────────────────────────────────────────

const UART_DR: *mut u32 = 0x0900_0000 as *mut u32;
const UART_FR: *const u32 = 0x0900_0018 as *const u32;
const FR_TXFF: u32 = 1 << 5;

fn putc(b: u8) {
    unsafe {
        while core::ptr::read_volatile(UART_FR) & FR_TXFF != 0 {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile(UART_DR, b as u32);
    }
}

fn puts(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' { putc(b'\r'); }
        putc(b);
    }
}

fn put_u32_hex(v: u32) {
    let nybbles = [
        (v >> 28) & 0xF, (v >> 24) & 0xF, (v >> 20) & 0xF, (v >> 16) & 0xF,
        (v >> 12) & 0xF, (v >>  8) & 0xF, (v >>  4) & 0xF,  v        & 0xF,
    ];
    puts("0x");
    for n in nybbles {
        putc(if n < 10 { b'0' + n as u8 } else { b'a' + (n - 10) as u8 });
    }
}

// ── GIC system-register interface ────────────────────────────────────────────

fn gic_init() {
    // Enable SRE and Group 1 IRQs with priority mask = 0xFF.
    unsafe {
        core::arch::asm!(
            "msr S3_0_C12_C12_5, {sre}",    // ICC_SRE_EL1
            "isb",
            "msr S3_0_C12_C12_7, {grpen}",  // ICC_IGRPEN1_EL1
            "isb",
            "msr S3_0_C4_C6_0,   {pmr}",    // ICC_PMR_EL1
            "isb",
            sre   = in(reg) 1u64,
            grpen = in(reg) 1u64,
            pmr   = in(reg) 0xFFu64,
            options(nomem, nostack)
        );
    }
}

fn gic_ack() -> u32 {
    let iar: u64;
    unsafe {
        core::arch::asm!(
            "mrs {v}, S3_0_C12_C12_0",  // ICC_IAR1_EL1
            v = out(reg) iar,
            options(nomem, nostack)
        );
    }
    iar as u32
}

fn gic_eoi(intid: u32) {
    unsafe {
        core::arch::asm!(
            "msr S3_0_C12_C12_1, {v}",  // ICC_EOIR1_EL1
            v = in(reg) intid as u64,
            options(nomem, nostack)
        );
    }
}

/// Enable IRQs (clear DAIF.I).
fn enable_irq() {
    unsafe { core::arch::asm!("msr DAIFClr, #2", options(nomem, nostack)); }
}

// ── HVC doorbell ─────────────────────────────────────────────────────────────

const TANIX_DOORBELL_SEND: u64 = 0x8600_0001;

fn doorbell_send(handle: u32) {
    unsafe {
        core::arch::asm!(
            "hvc #0",
            inout("x0") TANIX_DOORBELL_SEND => _,
            in("x1") handle as u64,
            options(nomem)
        );
    }
}

/// SGI 1 to CPU 0 — matches what the kernel sends us.
fn sgi_send() {
    let sgi1r: u64 = (1u64 << 24) | 0b1; // SGI ID=1, target CPU0
    unsafe {
        core::arch::asm!(
            "msr S3_0_C12_C11_5, {v}", // ICC_SGI1R_EL1
            "isb",
            v = in(reg) sgi1r,
            options(nomem, nostack)
        );
    }
}

// ── VirtIO shared-memory layout constants (must match kernel's virtio/mod.rs) ─

const VIRTQ_MAGIC: u32  = 0x5649_5254;
const QUEUE_SIZE: usize = 16;
const BUF_SIZE:   usize = 256;
const OFF_CONFIG:  usize = 0x0000;
const OFF_DESC:    usize = 0x0040;
const OFF_AVAIL:   usize = OFF_DESC + QUEUE_SIZE * 16; // 0x0140
const OFF_USED:    usize = 0x1000;
// Guest accesses buffer memory directly via the physical address in the
// descriptor's `addr` field; OFF_BUFFERS isn't needed on the device side.

// ── Message opcodes ───────────────────────────────────────────────────────────

const OP_PRINT: u8 = 0x01;
const OP_ECHO:  u8 = 0x02;

// ── VirtIO ring accessors (volatile reads/writes only) ────────────────────────

unsafe fn vq_avail_idx(base: *mut u8) -> u16 {
    let avail = base.add(OFF_AVAIL) as *const u16;
    core::ptr::read_volatile(avail.add(1)) // flags(u16) + idx(u16)
}

unsafe fn vq_avail_ring(base: *mut u8, slot: usize) -> u16 {
    let avail = base.add(OFF_AVAIL) as *const u16;
    core::ptr::read_volatile(avail.add(2 + slot)) // flags + idx + ring[]
}

unsafe fn vq_desc_addr(base: *mut u8, idx: usize) -> u64 {
    let desc = base.add(OFF_DESC + idx * 16) as *const u64;
    core::ptr::read_volatile(desc)
}

unsafe fn vq_desc_len(base: *mut u8, idx: usize) -> u32 {
    let desc = base.add(OFF_DESC + idx * 16 + 8) as *const u32;
    core::ptr::read_volatile(desc)
}

/// Write a used-ring entry and advance the used index.
unsafe fn vq_put_used(base: *mut u8, last_used: &mut u16, desc_idx: u16, written: u32) {
    let used_base = base.add(OFF_USED) as *mut u16;
    // used ring layout: flags(u16) idx(u16) ring[](id:u32 + len:u32)
    let ring_elem = base.add(OFF_USED + 4 + (*last_used as usize % QUEUE_SIZE) * 8);
    core::ptr::write_volatile(ring_elem as *mut u32, desc_idx as u32);
    core::ptr::write_volatile(ring_elem.add(4) as *mut u32, written);

    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);

    *last_used = last_used.wrapping_add(1);
    core::ptr::write_volatile(used_base.add(1), *last_used);
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // x1 holds the shmem physical base, placed there by the kernel's launch
    // sequence (`mov x1, {shmem}` before `eret`).
    let shmem_phys: u64;
    unsafe {
        core::arch::asm!("mov {}, x1", out(reg) shmem_phys, options(nomem, nostack));
    }

    puts("\n[Zephyr-stub] Phase 3 guest booted, shmem=");
    put_u32_hex(shmem_phys as u32);
    puts("\n");

    // Initialise GIC so we can receive SGI 1.
    gic_init();
    enable_irq();

    let base = shmem_phys as *mut u8;

    // Wait for the kernel to write VIRTQ_MAGIC into the config block.
    loop {
        let magic = unsafe {
            core::ptr::read_volatile(base.add(OFF_CONFIG) as *const u32)
        };
        if magic == VIRTQ_MAGIC {
            break;
        }
        core::hint::spin_loop();
    }
    puts("[Zephyr-stub] VirtQueue config found\n");

    let mut last_avail: u16 = 0;
    let mut last_used:  u16 = 0;
    let mut rounds:     u32 = 0;
    const MAX_ROUNDS:   u32 = 16;

    loop {
        // Wait for a new avail-ring entry (signalled by SGI 1 doorbell).
        unsafe {
            loop {
                core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
                let avail_idx = vq_avail_idx(base);
                if avail_idx != last_avail {
                    break;
                }
                // Enable interrupts and wait (wfi).
                core::arch::asm!("wfi", options(nomem, nostack));
                // After wfi, handle any pending SGI.
                let intid = gic_ack();
                if intid != 1023 {
                    gic_eoi(intid);
                }
            }
        }

        // Process all pending avail entries.
        unsafe {
            loop {
                core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
                let avail_idx = vq_avail_idx(base);
                if avail_idx == last_avail {
                    break;
                }

                let slot      = last_avail as usize % QUEUE_SIZE;
                let desc_idx  = vq_avail_ring(base, slot);
                last_avail    = last_avail.wrapping_add(1);

                // Read the descriptor.
                let buf_addr  = vq_desc_addr(base, desc_idx as usize);
                let buf_len   = vq_desc_len(base, desc_idx as usize);
                let buf       = buf_addr as *const u8;

                let opcode    = core::ptr::read_volatile(buf);

                if opcode == OP_PRINT {
                    let payload_len = core::ptr::read_volatile(buf.add(3)) as usize;
                    let payload     = core::slice::from_raw_parts(buf.add(4), payload_len.min(BUF_SIZE - 4));

                    puts("[Zephyr-stub] Print: ");
                    for &b in payload {
                        putc(b);
                    }
                    puts("\n");

                    // Write Echo reply into the same buffer (kernel owns the
                    // descriptor address; we overwrite the payload in-place).
                    let reply_buf = buf_addr as *mut u8;
                    core::ptr::write_volatile(reply_buf,       OP_ECHO);
                    core::ptr::write_volatile(reply_buf.add(1), 0u8);
                    core::ptr::write_volatile(reply_buf.add(2), 0u8);
                    core::ptr::write_volatile(reply_buf.add(3), 4u8);
                    let printed = payload_len as u32;
                    reply_buf.add(4).copy_from(printed.to_le_bytes().as_ptr(), 4);

                    let written = 8u32; // opcode + reserved×2 + len + u32

                    // Return to used ring.
                    vq_put_used(base, &mut last_used, desc_idx, written);

                    // Signal kernel via HVC doorbell.
                    doorbell_send(1);
                    // Also fire SGI 1 so the kernel's IRQ handler sees it.
                    sgi_send();

                    rounds += 1;
                    let _ = buf_len;
                } else {
                    puts("[Zephyr-stub] unknown opcode\n");
                    vq_put_used(base, &mut last_used, desc_idx, 0);
                }

                if rounds >= MAX_ROUNDS {
                    break;
                }
            }
        }

        if rounds >= MAX_ROUNDS {
            break;
        }
    }

    puts("[Zephyr-stub] VirtIO loop complete — halting\n");
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)); }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    puts("[Zephyr-stub] PANIC");
    if let Some(loc) = info.location() {
        puts(" at ");
        puts(loc.file());
    }
    puts("\n");
    loop { core::hint::spin_loop(); }
}
