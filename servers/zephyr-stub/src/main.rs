//! Zephyr RTOS stub — Phase 3 guest VM with VirtIO device driver.
//!
//! The guest is loaded into guest RAM by the kernel and entered via a
//! cooperative context switch.  Boot arguments arrive in registers:
//!
//!   x4 — shared-memory physical base (VirtqueueConfig lives at offset 0)
//!   x5 — address of the kernel's `vm_yield_entry` function
//!   x6 — pointer to the guest context the kernel uses to resume us
//!
//! Execution model: the guest is a *cooperative vCPU*.  It runs, processes
//! every pending VirtIO request, and then calls the yield function to hand
//! control back to the kernel.  The kernel later resumes us; the yield call
//! returns and the loop continues.  No interrupts, no hypercalls, no wfi.
//!
//! VirtIO device loop:
//!
//!   1. Wait for the kernel to post a buffer to the avail ring.
//!   2. Read the buffer — expect a `Print` opcode.
//!   3. Print the payload string to the UART.
//!   4. Write an `Echo` reply into the same buffer slot.
//!   5. Post the descriptor to the used ring.
//!   6. Process all remaining pending buffers, then yield back to the kernel.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

// ── UART ──────────────────────────────────────────────────────────────────────
//
// The kernel hands the machine console base in boot register x7 (set on
// every entry), so the same binary runs on `virt` (0x0900_0000) and
// `sbsa-ref` (0x6000_0000).  Falls back to the original virt address if x7
// is missing.

static mut UART_BASE: usize = 0x0900_0000;

fn set_uart_base(uart: u64) {
    if uart != 0 {
        unsafe { UART_BASE = uart as usize; }
    }
}

fn uart_dr() -> *mut u32 {
    unsafe { (UART_BASE + 0x0000) as *mut u32 }
}
fn uart_fr() -> *const u32 {
    unsafe { (UART_BASE + 0x0018) as *const u32 }
}
const FR_TXFF: u32 = 1 << 5;

fn putc(b: u8) {
    unsafe {
        while core::ptr::read_volatile(uart_fr()) & FR_TXFF != 0 {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile(uart_dr(), b as u32);
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

// ── VirtIO shared-memory layout constants (mirror kernel virtio/mod.rs) ─────

const VIRTQ_MAGIC: u32  = 0x5649_5254;
const QUEUE_SIZE: usize = 16;
const BUF_SIZE:   usize = 256;
const OFF_CONFIG: usize = 0x0000;
const OFF_DESC:   usize = 0x0040;
const OFF_AVAIL:  usize = OFF_DESC + QUEUE_SIZE * 16; // 0x0140
const OFF_USED:   usize = 0x1000;
// Buffers are addressed via the descriptor's `addr` field; the guest never
// needs OFF_BUFFERS.

// ── Message opcodes ───────────────────────────────────────────────────────────

const OP_PRINT: u8 = 0x01;
const OP_ECHO:  u8 = 0x02;

// ── Phase 13: VMM info block (kernel publishes into the shmem region) ────────

/// Info block offset in the shared-memory region: u32 magic, u32 msgq
/// handle, u64 `vmm_service` address.
const VMM_INFO_OFF: usize = 0x2000;
const VMM_INFO_MAGIC: u32 = 0x564D_4D49; // "IVMM"

/// Guest -> VMM service function ids (mirror `tanix_hvc` in the kernel).
const TANIX_MSGQ_SEND: u64 = 0x8600_0003;
const TANIX_MSGQ_RECV: u64 = 0x8600_0004;
const TANIX_HVC_ERR:   u64 = u64::MAX;

unsafe fn vmm_info_ready(base: *mut u8) -> bool {
    let magic = core::ptr::read_volatile(base.add(VMM_INFO_OFF) as *const u32);
    magic == VMM_INFO_MAGIC
}

// ── VirtIO ring accessors (volatile reads/writes only) ────────────────────────

/// Read the avail-ring index (offset 2: flags is a u16 before it).
unsafe fn vq_avail_idx(base: *mut u8) -> u16 {
    let avail = base.add(OFF_AVAIL) as *const u16;
    core::ptr::read_volatile(avail.add(1))
}

unsafe fn vq_avail_ring(base: *mut u8, slot: usize) -> u16 {
    let avail = base.add(OFF_AVAIL) as *const u16;
    core::ptr::read_volatile(avail.add(2 + slot))
}

unsafe fn vq_desc_addr(base: *mut u8, idx: usize) -> u64 {
    let desc = base.add(OFF_DESC + idx * 16) as *const u64;
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

// ── Cooperative yield ─────────────────────────────────────────────────────────

/// The kernel's `vm_yield_entry(guest_ctx: usize)` — saves our state and
/// switches back to the kernel.  Returns when the kernel resumes us.
type YieldFn = unsafe extern "C" fn(guest_ctx: usize);

// ── Entry point ───────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Boot arguments set by vm::Manager::start immediately before the
    // context switch (x4/x5/x6/x7 are untouched by the switch stub).
    let shmem_phys: u64;
    let yield_addr: u64;
    let guest_ctx: u64;
    let uart: u64;
    unsafe {
        core::arch::asm!("mov {}, x4", out(reg) shmem_phys, options(nomem, nostack));
        core::arch::asm!("mov {}, x5", out(reg) yield_addr, options(nomem, nostack));
        core::arch::asm!("mov {}, x6", out(reg) guest_ctx, options(nomem, nostack));
        core::arch::asm!("mov {}, x7", out(reg) uart, options(nomem, nostack));
    }
    set_uart_base(uart);

    puts("\n[Zephyr-stub] guest booted, shmem=");
    put_u32_hex(shmem_phys as u32);
    puts("\n");

    let base = shmem_phys as *mut u8;
    let yield_fn: YieldFn = unsafe { core::mem::transmute(yield_addr) };

    // Wait for the kernel to write VIRTQ_MAGIC into the config block.
    // (In the current flow this is written before we are launched.)
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
        // Process every avail-ring entry the kernel has posted since we
        // last ran.  The kernel posts exactly one Print per resume.
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

                // Read the descriptor → buffer location.
                let buf_addr = vq_desc_addr(base, desc_idx as usize);
                let buf      = buf_addr as *mut u8;

                let opcode = core::ptr::read_volatile(buf);

                if opcode == OP_PRINT {
                    let payload_len =
                        core::ptr::read_volatile(buf.add(3)) as usize;
                    let payload = core::slice::from_raw_parts(
                        buf.add(4),
                        payload_len.min(BUF_SIZE - 4),
                    );

                    puts("[Zephyr-stub] Print: ");
                    for &b in payload {
                        putc(b);
                    }
                    puts("\n");

                    // Write an Echo reply into the same buffer (opcode,
                    // reserved, length=4, u32 LE printed-byte count).
                    core::ptr::write_volatile(buf,       OP_ECHO);
                    core::ptr::write_volatile(buf.add(1), 0u8);
                    core::ptr::write_volatile(buf.add(2), 0u8);
                    core::ptr::write_volatile(buf.add(3), 4u8);
                    let printed = payload_len as u32;
                    buf.add(4).copy_from(printed.to_le_bytes().as_ptr(), 4);

                    let written = 8u32; // opcode + reserved×2 + len + u32

                    // Return the descriptor to the kernel via the used ring.
                    vq_put_used(base, &mut last_used, desc_idx, written);

                    rounds += 1;
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

        // No more work — hand control back to the kernel.  Returns when the
        // kernel posts the next Print and resumes us.
        unsafe { yield_fn(guest_ctx as usize) };

        // Phase 13: the kernel may have published the VMM info block
        // (message-queue demo); if so, leave the VirtIO loop.
        if unsafe { vmm_info_ready(base) } {
            break;
        }
    }

    if unsafe { vmm_info_ready(base) } {
        // ── Phase 14: doorbell-driven message queue ──────────────────────────
        // Info block v2: u32 magic, u32 msgq handle, u64 `vmm_service`
        // entry (EL1 stand-in for an HVC trap), u32 doorbell handle,
        // u32 guest_state, u32 doorbell_flags — all published by the
        // kernel in the shared region.
        //
        // Protocol: we are a *blocking* consumer.  On an empty receive we
        // mark ourselves WAITING in the info block and yield; the kernel
        // rings the doorbell (GIC SGI on bare metal, GH_BELL_SEND on
        // Gunyah) when it sends and later resumes us — the yield returns
        // and we re-check the queue.  The doorbell decouples send from
        // wakeup: the kernel can produce several messages before we run.
        //
        // The queue is shared, so we reply to exactly the three pings and
        // then hand control back — if we kept receiving we would pick up
        // our own pongs.
        const GUEST_RUNNING: u32 = 0;
        const GUEST_WAITING: u32 = 1;
        const DOORBELL_FLAG_MSG: u32 = 1;

        unsafe {
        let info = base.add(VMM_INFO_OFF);
        let mq_handle: u32 =
            core::ptr::read_volatile(info.add(4) as *const u32);
        let service_addr: u64 =
            core::ptr::read_volatile(info.add(8) as *const u64);
        let state_ptr = info.add(0x14) as *mut u32;
        let flags_ptr = info.add(0x18) as *mut u32;
        let service: unsafe extern "C" fn(u64, u64, u64, u64) -> u64 =
            core::mem::transmute(service_addr);

        puts("[Zephyr-stub] Phase 14: doorbell message-queue\n");

        // Blocking receive: on an empty queue write WAITING, yield (the
        // kernel resumes us after ringing the doorbell), then re-check.
        // Returns the received size in `buf`.
        let recv_or_block = |buf: &mut [u8; 96]| -> usize {
            loop {
                let r = service(TANIX_MSGQ_RECV, mq_handle as u64,
                                buf.as_mut_ptr() as u64, 96);
                if r != TANIX_HVC_ERR {
                    return r as usize;
                }
                core::ptr::write_volatile(state_ptr, GUEST_WAITING);
                yield_fn(guest_ctx as usize);
                core::ptr::write_volatile(state_ptr, GUEST_RUNNING);
                if core::ptr::read_volatile(flags_ptr) & DOORBELL_FLAG_MSG != 0 {
                    puts("[Zephyr-stub] woken by doorbell\n");
                    core::ptr::write_volatile(flags_ptr, 0);
                }
            }
        };

        let mut buf = [0u8; 96];
        let mut index = 0u32;
        for _ in 0u32..3 {
            let n = recv_or_block(&mut buf);

            puts("[Zephyr-stub] msgq: received ");
            for &b in &buf[..n.min(buf.len())] {
                putc(b);
            }
            puts("\n");

            // Reply with the matching pong (index = receive order).
            let reply: &[u8] = match index {
                0 => b"pong-0",
                1 => b"pong-1",
                _ => b"pong-2",
            };
            index += 1;
            let _ = service(TANIX_MSGQ_SEND, mq_handle as u64,
                            reply.as_ptr() as u64, reply.len() as u64);
        }

        // Hand the three pongs back — the kernel drains the queue before
        // sending the finale message.
        yield_fn(guest_ctx as usize);

        // Finale: expect "done", acknowledge and park.  The kernel sends
        // it with the doorbell ringing before resuming us, so this receive
        // usually succeeds immediately.
        let n = recv_or_block(&mut buf);
        puts("[Zephyr-stub] msgq: received ");
        for &b in &buf[..n.min(buf.len())] {
            putc(b);
        }
        puts("\n");

        if n == 4 && buf[..4] == *b"done" {
            puts("[Zephyr-stub] Phase 14 complete — parked\n");
            let _ = service(TANIX_MSGQ_SEND, mq_handle as u64,
                            b"ack".as_ptr() as u64, 3);
            yield_fn(guest_ctx as usize);
        } else {
            puts("[Zephyr-stub] Phase 14: unexpected finale\n");
        }
        loop {
            core::hint::spin_loop();
        }
        }
    }

    puts("[Zephyr-stub] VirtIO loop complete — halting\n");
    loop {
        core::hint::spin_loop();
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
