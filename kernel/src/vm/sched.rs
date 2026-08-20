#![allow(dead_code)]
//! Phase 21: co-tenant vCPU scheduler.
//!
//! Multiple guest VMs share the physical CPU as *co-tenants*: the kernel
//! rounds-robin over them, and the EL1 tick time-slices them.  A tenant
//! that burns its quantum is preempted *inside* the guest (the full vCPU
//! frame is captured — `backend::tick_guest`) and resumed later exactly
//! where it stopped; a tenant whose RTOS has nothing to do yields
//! cooperatively and is re-run on the next pass.
//!
//! This module is policy: it owns the tenant table (which guest, its
//! shared-memory channel, its scheduling stats) and drives the rotation.
//! Mechanics (frame capture, `context_switch_preempt`, quantum bookkeeping)
//! live in `hypervisor::backend`.
//!
//! Model notes:
//!   • On Gunyah this is the VMM's `GH_VCPU_RUN` loop: run each vCPU for a
//!     slice, handle its exits (`VCPU_RUN_RESP_IRQ`, TCG, power-off),
//!     re-run.  Here "IRQ exit" is realised by the tick landing on the EL1
//!     timer while inside the guest.
//!   • No EL2/stage-2 isolation: tenants share the kernel's EL1 address
//!     space (a "real Zephyr" build requires EL2 page tables + its own
//!     GIC/timer view; documented as a platform constraint).

use crate::hypervisor::{HvError, Hypervisor, VcpuExit, VmHandle};
use crate::mem::PhysAddr;
use crate::virtio::transport::VirtioTransport;

/// Maximum number of co-tenants the scheduler rotates.
pub const MAX_TENANTS: usize = 4;

/// VMM info block each tenant publishes in its shared memory: the guest
/// writes its run state at THIS offset (shared convention with the
/// phase-14 layout; the phase-21 block lives at 0x3000 in each tenant's
/// *own* shmem, clear of the virtio buffer area [0x2000, 0x3000)).
const VMM_INFO_OFF: usize = 0x3000;
const GUEST_STATE_OFF: usize = 0x10;

/// Value the kernel publishes as the info-block magic ("IVMM").
const VMM_INFO_MAGIC: u32 = 0x564D_4D49;

/// Offsets inside each tenant's info block (kernel → guest).
const TENANT_ID_OFF: usize = 0x04;

/// Guest states (mirror the zephyr-rtos guest): RUNNING / WAITING /
/// PARKED.
pub const GUEST_RUNNING: u32 = 0;
pub const GUEST_WAITING: u32 = 1;
pub const GUEST_PARKED: u32 = 2;

/// One co-tenant guest: handle, its shared-memory virtio channel, and the
/// scheduler bookkeeping the demo reports at the end.
pub struct Tenant {
    pub name: [u8; 16],
    pub handle: VmHandle,
    /// Physical base of this tenant's shared-memory region (info block +
    /// virtqueue).
    pub shmem_phys: PhysAddr,
    /// Kernel-side virtqueue to this tenant's guest.
    pub transport: VirtioTransport,
    /// Times the scheduler entered this vCPU.
    pub runs: u64,
    /// Times the EL1 tick cut this tenant's slice short inside the guest.
    pub preempts: u64,
    /// Times the guest yielded cooperatively.
    pub yields: u64,
    /// Times the guest actually ran *and* echoed a kernel Print in one run.
    pub echoes: u64,
    /// Guest finished its demo and wrote PARKED into its info block —
    /// dropped from the rotation.
    pub parked: bool,
}

impl Tenant {
    pub fn new(name: &str, handle: VmHandle, shmem_phys: PhysAddr, transport: VirtioTransport) -> Self {
        let mut name_buf = [0u8; 16];
        let n = name.len().min(15);
        name_buf[..n].copy_from_slice(&name.as_bytes()[..n]);
        Self {
            name: name_buf,
            handle,
            shmem_phys,
            transport,
            runs: 0,
            preempts: 0,
            yields: 0,
            echoes: 0,
            parked: false,
        }
    }

    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.name[..end]).unwrap_or("?")
    }
}

/// Read the tenant's published state word from its shmem info block.
pub fn tenant_state(shmem_phys: usize) -> u32 {
    unsafe {
        core::ptr::read_volatile(
            (shmem_phys + VMM_INFO_OFF + GUEST_STATE_OFF) as *const u32,
        )
    }
}

/// Write a state word into a tenant's shmem info block (kernel side).
///
/// # Safety
/// `shmem_phys` must be an identity-mapped shared region with the info
/// block laid out.
pub unsafe fn set_tenant_state(shmem_phys: usize, state: u32) {
    core::ptr::write_volatile(
        (shmem_phys + VMM_INFO_OFF + GUEST_STATE_OFF) as *mut u32,
        state,
    );
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
}

/// Format a short "T<i> r<pass>" text into `out`; returns the length.
/// No_std-safe (no fmt machinery in the tick path).
fn round_text(tenant_id: u8, pass: u32, out: &mut [u8; MAX_PAYLOAD]) -> usize {
    let mut n = 0;
    out[n] = b'T';
    n += 1;
    if tenant_id < 10 {
        out[n] = b'0' + tenant_id;
        n += 1;
    } else {
        out[n] = b'0' + tenant_id / 10;
        out[n + 1] = b'0' + tenant_id % 10;
        n += 2;
    }
    out[n] = b' ';
    n += 1;
    out[n] = b'r';
    n += 1;
    // decimal pass (0..=9999)
    if pass >= 1000 {
        out[n] = b'0' + (pass / 1000) as u8;
        n += 1;
        out[n] = b'0' + ((pass / 100) % 10) as u8;
        n += 1;
        out[n] = b'0' + ((pass / 10) % 10) as u8;
        n += 1;
        out[n] = b'0' + (pass % 10) as u8;
        n += 1;
    } else if pass >= 100 {
        out[n] = b'0' + (pass / 100) as u8;
        n += 1;
        out[n] = b'0' + ((pass / 10) % 10) as u8;
        n += 1;
        out[n] = b'0' + (pass % 10) as u8;
        n += 1;
    } else if pass >= 10 {
        out[n] = b'0' + (pass / 10) as u8;
        n += 1;
        out[n] = b'0' + (pass % 10) as u8;
        n += 1;
    } else {
        out[n] = b'0' + pass as u8;
        n += 1;
    }
    n
}

/// Maximum payload of a virtio Print message (mirrors virtio/channel.rs).
const MAX_PAYLOAD: usize = 252;

/// Run `tenants` as co-tenants: `/quantum_ticks` ticks per time slice,
/// round-robin, posting one kernel `Print` into each tenant's virtqueue per
/// pass (its guest idler Echoes it back — the heartbeat that keeps the
/// channel exercised under preemption).  Stops when every tenant parked or
/// `max_passes` scheduler passes elapsed; returns the per-tenant stats in
/// the table for the demo's summary log.
///
/// The kernel's continuation between guest exits is the caller's stack:
/// each `vcpu_run` both saves it (on guest entry) and, after a preemption,
/// resumes it (`context_switch_preempt` restores the saved VMM state), so
/// this loop is the natural resume point for both exit kinds.
///
/// # Safety
/// Must run in the boot context with at least one non-parked tenant.
/// Arms and later disarms the co-tenant preemption machinery; the EL1
/// physical timer must already be ticking (PPI 30 enabled, tick armed).
pub unsafe fn run(
    tenants: &mut [Tenant],
    quantum_ticks: u32,
    max_passes: u32,
    hv: &mut dyn Hypervisor,
) -> Result<(), HvError> {
    crate::hypervisor::backend::enable_guest_preemption(quantum_ticks);

    // Publish each tenant's info block before the first resume: magic,
    // tenant id, and the initial RUNNING state (the guest waits for the
    // magic before it starts its RTOS).
    for (i, t) in tenants.iter().enumerate() {
        let base = (t.shmem_phys as usize + VMM_INFO_OFF) as *mut u32;
        core::ptr::write_volatile(base, VMM_INFO_MAGIC);
        core::ptr::write_volatile(base.add(TENANT_ID_OFF / 4), i as u32);
        core::ptr::write_volatile(
            base.add(GUEST_STATE_OFF / 4),
            GUEST_RUNNING,
        );
    }
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);

    let mut cursor = 0usize;
    let mut pass = 0u32;
    let mut parked = 0usize;

    while pass < max_passes {
        pass += 1;
        if parked >= tenants.len() {
            break;
        }

        // One pass = one slice per remaining tenant.
        let n = tenants.len();
        for _ in 0..n {
            // Round-robin to the next non-parked tenant.
            let mut idx = cursor;
            let mut seen = 0;
            while tenants[idx].parked {
                idx = (idx + 1) % n;
                seen += 1;
                if seen >= n {
                    idx = n; // all parked
                    break;
                }
            }
            if idx == n {
                break;
            }
            cursor = (idx + 1) % n;

            let t = &mut tenants[idx];

            // Kernel-side heartbeat: post a Print into the tenant's ring.
            let mut text = [0u8; MAX_PAYLOAD];
            let len = round_text(idx as u8, pass, &mut text);
            let desc = t.transport.send_print(&text[..len], hv);

            // Run the tenant's vCPU for up to `quantum_ticks` ticks.
            let exit = hv.vcpu_run(t.handle, 0)?;
            match exit {
                VcpuExit::Preempted => t.preempts += 1,
                VcpuExit::Yielded => t.yields += 1,
                other => {
                    log::warn!("phase 21: tenant {} exited {:?}", t.name_str(), other)
                }
            }
            t.runs += 1;

            // Collect the Echo (if the guest's idler got to it).
            let mut echoed = false;
            t.transport.poll_replies(|d, _op, _printed| {
                if d == desc {
                    echoed = true;
                }
            });
            if echoed {
                t.echoes += 1;
            }

            // Guest announced it is done → drop it from the rotation.
            if tenant_state(t.shmem_phys as usize) == GUEST_PARKED && !t.parked {
                t.parked = true;
                parked += 1;
                log::info!("phase 21: tenant {} parked", t.name_str());
            }
        }
    }

    crate::hypervisor::backend::disable_guest_preemption();
    log::info!(
        "phase 21: tenant scheduler done after {} passes ({} parked)",
        pass,
        parked
    );
    Ok(())
}