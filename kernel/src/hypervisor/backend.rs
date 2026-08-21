#![allow(dead_code)]
//! Hypervisor backend implementations.
//!
//! BareMetalBackend
//! ─────────────────
//! A minimal VMM that runs guest VMs as *cooperative vCPU pairs* inside a
//! single EL1 address space:
//!   • "vCPU run" is a context switch into the guest; the guest returns
//!     control by calling a kernel-provided yield function (an exit).
//!   • "Message queue" is a real kernel object: a fixed-size ring of 96 B
//!     messages with Gunyah's non-blocking send/recv + "ready" semantics.
//!   • "Doorbell" is a software interrupt delivered via GIC SGI (ID 1).
//!   • "Shared memory" is a physical range handed to the guest via boot
//!     registers; both sides are identity-mapped, so addresses are shared
//!     directly.
//!
//! This gives us the same *API contract* as a Type-1 hypervisor without
//! requiring EL2.  The security guarantees are weaker (no EL2 boundary),
//! but the design maps 1:1 onto Gunyah: vCPU run = GH_VCPU_RUN, message
//! queue = GH_MSGQ_SEND/RECV, doorbell = GH_BELL_SEND, shared memory =
//! GH_MEMEXTENT_DONATE.
//!
//! GunyahBackend
//! ─────────────
//! Issues the real Gunyah hypercalls via SMCCC HVC (function IDs per the
//! upstream Linux driver).  The hypercall layer (identify, msgq send/recv,
//! doorbell send/set-mask, vcpu_run, addrspace map) is implemented against
//! the ABI; object *creation* flows through the Resource Manager, which
//! needs a Gunyah environment (QEMU fork or SA8295P hardware), so those
//! methods return `NotSupported` until then.  Enable probing with the
//! `gunyah` cargo feature; see hypervisor/mod.rs.

use super::{Hypervisor, HvError, VcpuExit, VmConfig, VmHandle, MsgqHandle, DoorbellHandle, ShmemHandle};
use crate::mem::PhysAddr;
use crate::sched::task::{context_switch, Context};
use super::MSGQ_MAX_MSG_SIZE;

// ── Bare-metal backend ────────────────────────────────────────────────────────

/// Maximum number of simultaneously active guest VMs.
const MAX_VMS: usize = 4;

/// Maximum number of simultaneous message queues.
const MAX_MSGQ: usize = 8;

/// Message queues hold at most this many messages (fixed-size ring).
const MSGQ_SLOTS: usize = 16;

/// One message-queue slot.
#[derive(Clone, Copy)]
struct MsgqSlot {
    /// Message bytes (fixed-size payload, Gunyah-style).
    data: [u8; MSGQ_MAX_MSG_SIZE],
    /// Bytes actually used (0..=MSGQ_MAX_MSG_SIZE).
    len: u16,
}

/// A message queue object: bounded ring with Gunyah semantics.
struct MsgqRecord {
    handle: MsgqHandle,
    /// VM the queue was created for (its "owner").
    owner: VmHandle,
    /// Ring head (dequeue index) and occupancy.
    head: usize,
    count: usize,
    /// Fixed capacity (1..=MSGQ_SLOTS).
    depth: usize,
    slots: [Option<MsgqSlot>; MSGQ_SLOTS],
}

impl MsgqRecord {
    fn enqueue(&mut self, msg: &[u8]) -> Result<bool, HvError> {
        if msg.len() > MSGQ_MAX_MSG_SIZE {
            return Err(HvError::BadMessage);
        }
        if self.count >= self.depth {
            return Err(HvError::Full);
        }
        let idx = (self.head + self.count) % MSGQ_SLOTS;
        let mut data = [0u8; MSGQ_MAX_MSG_SIZE];
        data[..msg.len()].copy_from_slice(msg);
        self.slots[idx] = Some(MsgqSlot { data, len: msg.len() as u16 });
        self.count += 1;
        Ok(self.count < self.depth)
    }

    fn dequeue(&mut self, buf: &mut [u8]) -> Result<(usize, bool), HvError> {
        if self.count == 0 {
            return Err(HvError::Empty);
        }
        let slot = self.slots[self.head].take().unwrap();
        let n = slot.len as usize;
        let copy = n.min(buf.len());
        buf[..copy].copy_from_slice(&slot.data[..copy]);
        self.head = (self.head + 1) % MSGQ_SLOTS;
        self.count -= 1;
        Ok((copy, self.count > 0))
    }
}

/// Internal VM record tracked by the bare-metal backend.
struct VmRecord {
    handle: VmHandle,
    /// Physical base address of this VM's RAM.
    ram_base: PhysAddr,
    ram_size: usize,
    /// Guest entry point.
    entry: PhysAddr,
    /// Boot arguments (x4/x5 at first launch).
    boot: [u64; 2],
    /// True if the VM has been vcpu_run at least once (contexts live).
    started: bool,
    /// True while a vCPU is inside the context switch (only vCPU 0 exists
    /// on the bare-metal backend).
    running: bool,
    /// True while the CPU is actually executing this VM's vCPU.
    /// Distinguishes "mid `enter_guest`" from "back in kernel code after
    /// the guest yielded / was preempted" — only the former may be
    /// preempted by a tick.
    in_guest: bool,
    /// True if the last `vcpu_run` returned because the tick preempted
    /// the guest (Phase 21) rather than because the guest yielded.
    preempted: bool,
    /// Remaining time-slice budget in ticks (Phase 21 co-tenancy).  Set at
    /// every `vcpu_run` entry; consumed by ticks that land inside the
    /// guest; reaching 0 arms the preemption capture.
    budget_ticks: u32,
    /// The kernel/guest context pair for the cooperative vCPU.
    kernel_ctx: Context,
    guest_ctx: Context,
}

pub struct BareMetalBackend {
    vms: [Option<VmRecord>; MAX_VMS],
    msgqs: [Option<MsgqRecord>; MAX_MSGQ],
    next_handle: u32,
    /// Next SGI doorbell target (we target CPU 0 in a loopback test).
    next_doorbell: u32,
}

impl BareMetalBackend {
    pub const fn new() -> Self {
        Self {
            vms: [None, None, None, None],
            msgqs: [None, None, None, None, None, None, None, None],
            next_handle: 1,
            next_doorbell: 1,
        }
    }

    fn alloc_handle(&mut self) -> VmHandle {
        let h = VmHandle(self.next_handle);
        self.next_handle += 1;
        h
    }

    fn find_vm(&mut self, handle: VmHandle) -> Option<&mut VmRecord> {
        self.vms
            .iter_mut()
            .filter_map(|s| s.as_mut())
            .find(|v| v.handle == handle)
    }

    fn find_msgq(&mut self, handle: MsgqHandle) -> Option<&mut MsgqRecord> {
        self.msgqs
            .iter_mut()
            .filter_map(|s| s.as_mut())
            .find(|q| q.handle == handle)
    }

    /// Find the VM whose vCPU is currently running inside the cooperative
    /// switch (called from the guest's yield path).
    fn running_vm_mut(&mut self) -> Option<&mut VmRecord> {
        self.vms
            .iter_mut()
            .filter_map(|s| s.as_mut())
            .find(|v| v.running)
    }
}

impl Hypervisor for BareMetalBackend {
    fn detect() -> bool {
        true // always available
    }

    fn vm_create(&mut self, config: VmConfig) -> Result<VmHandle, HvError> {
        // Allocate handle and find a free slot before any borrows overlap.
        let handle = self.alloc_handle();

        let slot = self.vms.iter_mut().find(|s| s.is_none())
            .ok_or(HvError::NoMemory)?;

        // NOTE: the guest RAM is not mapped here.  The whole DDR window is
        // pre-mapped as 2 MiB blocks at MMU enable time (mem::page_table),
        // so frames handed out by the allocator are always accessible.
        // The vm::Manager zeroes and loads the image into RAM before this
        // call, so no mapping work remains by the time we get here.

        log::info!(
            "vm_create: handle={:?} ram={:#x}+{:#x} entry={:#x}",
            handle, config.ram_base, config.ram_size, config.entry
        );

        *slot = Some(VmRecord {
            handle,
            ram_base: config.ram_base,
            ram_size: config.ram_size,
            entry: config.entry,
            boot: config.boot,
            started: false,
            running: false,
            in_guest: false,
            preempted: false,
            budget_ticks: 0,
            kernel_ctx: Context::zeroed(),
            guest_ctx: Context::zeroed(),
        });

        Ok(handle)
    }

    fn vm_destroy(&mut self, handle: VmHandle) -> Result<(), HvError> {
        let slot = self.vms
            .iter_mut()
            .find(|s| s.as_ref().map(|v| v.handle == handle).unwrap_or(false))
            .ok_or(HvError::InvalidHandle)?;
        if slot.as_ref().unwrap().running {
            return Err(HvError::BadState);
        }
        log::info!("vm_destroy: handle={:?}", handle);
        *slot = None;
        Ok(())
    }

    fn vcpu_run(&mut self, vm: VmHandle, vcpu: u32) -> Result<VcpuExit, HvError> {
        if vcpu != 0 {
            return Err(HvError::NotSupported); // one cooperative vCPU per VM
        }
        {
            let vm_rec = self.find_vm(vm).ok_or(HvError::InvalidHandle)?;
            if vm_rec.running {
                return Err(HvError::BadState);
            }
            // First run: prime the guest context (entry + stack top).
            if !vm_rec.started {
                vm_rec.guest_ctx = Context::new(vm_rec.entry, vm_rec.ram_base + vm_rec.ram_size);
                vm_rec.started = true;
                log::info!(
                    "vcpu_run: {:?} first run entry={:#x} sp={:#x}",
                    vm, vm_rec.entry, vm_rec.ram_base + vm_rec.ram_size
                );
            }
            // Phase 21: guests run with IRQ *enabled* so the tick can
            // preempt them (co-tenancy time-slicing).  `context_switch`
            // stores a constant masked SPSR every time the guest context
            // is saved, so re-prime it on every entry — EXCEPT when the
            // context resumes through `restore_preempted_guest`: that stub
            // rebuilds the vCPU from the captured frame and must run
            // interrupt-masked until its final `eret` (a tick mid-stub
            // would capture a half-restored file — x9/x10 still holding
            // the ELR/SPSR values — as the new vCPU state).  The stub
            // reinstates the guest's own PSTATE from the frame itself.
            if vm_rec.guest_ctx.lr != restore_preempted_guest as *const () as u64 {
                vm_rec.guest_ctx.spsr = SPSR_GUEST;
            }
            // Fresh time slice for the upcoming run.
            vm_rec.budget_ticks = unsafe { QUANTUM_TICKS };
        }

        // The actual CPU transfer into the guest.  `enter_guest` returns
        // when the guest yields control back (its vm_yield_entry call) —
        // on Gunyah this is GH_VCPU_RUN returning with an exit reason.
        // Phase 21: it also returns, via `context_switch_preempt`, when a
        // tick cut the guest's time slice short — same resume point, the
        // loop continuation, so both exits converge here.
        unsafe {
            let vm_rec = self.find_vm(vm).unwrap();
            let boot = vm_rec.boot;
            vm_rec.running = true;
            vm_rec.in_guest = true;
            ACTIVE_VM = Some(vm);
            ACTIVE_CPU = crate::smp::cpu_index();
            let uart = crate::arch::aarch64::machine().uart_base as u64;
            enter_guest(&mut vm_rec.kernel_ctx, &mut vm_rec.guest_ctx, boot, uart);
            vm_rec.in_guest = false;
            vm_rec.running = false;
            ACTIVE_VM = None;
        }

        // Distinguish the two exit reasons for the co-tenant scheduler.
        let preempted = self
            .find_vm(vm)
            .map(|r| {
                let p = r.preempted;
                r.preempted = false;
                p
            })
            .unwrap_or(false);
        log::info!(
            "vcpu_run: {:?} vCPU 0 exited ({})",
            vm,
            if preempted { "preempted" } else { "yielded" }
        );
        Ok(if preempted {
            VcpuExit::Preempted
        } else {
            VcpuExit::Yielded
        })
    }

    fn vcpu_stop(&mut self, vm: VmHandle, _vcpu: u32) -> Result<(), HvError> {
        let vm_rec = self.find_vm(vm).ok_or(HvError::InvalidHandle)?;
        if vm_rec.running {
            return Err(HvError::BadState); // cannot stop a running vCPU
        }
        vm_rec.started = false;
        log::info!("vcpu_stop: {:?} reset (next run re-primes the context)", vm);
        Ok(())
    }

    fn msgq_create(&mut self, vm: VmHandle, depth: u32) -> Result<MsgqHandle, HvError> {
        if self.find_vm(vm).is_none() {
            return Err(HvError::InvalidHandle);
        }
        let depth = (depth as usize).clamp(1, MSGQ_SLOTS);

        let slot = self.msgqs.iter_mut().find(|s| s.is_none())
            .ok_or(HvError::NoMemory)?;
        let handle = MsgqHandle(self.next_handle);
        self.next_handle += 1;

        *slot = Some(MsgqRecord {
            handle,
            owner: vm,
            head: 0,
            count: 0,
            depth,
            slots: [None; MSGQ_SLOTS],
        });

        log::info!(
            "msgq_create: handle={:?} owner={:?} depth={} ({} B messages)",
            handle, vm, depth, MSGQ_MAX_MSG_SIZE
        );
        Ok(handle)
    }

    fn msgq_send(&mut self, mq: MsgqHandle, msg: &[u8]) -> Result<bool, HvError> {
        let q = self.find_msgq(mq).ok_or(HvError::InvalidHandle)?;
        let ready = q.enqueue(msg)?;
        log::trace!("msgq_send: {:?} {} B (ready={})", mq, msg.len(), ready);
        Ok(ready)
    }

    fn msgq_recv(&mut self, mq: MsgqHandle, buf: &mut [u8]) -> Result<(usize, bool), HvError> {
        let q = self.find_msgq(mq).ok_or(HvError::InvalidHandle)?;
        let (n, ready) = q.dequeue(buf)?;
        log::trace!("msgq_recv: {:?} -> {} B (ready={})", mq, n, ready);
        Ok((n, ready))
    }

    fn doorbell_create(
        &mut self,
        owner_vm: VmHandle,
        irq: u32,
    ) -> Result<DoorbellHandle, HvError> {
        if self.find_vm(owner_vm).is_none() {
            return Err(HvError::InvalidHandle);
        }
        crate::hypervisor::doorbell::register(owner_vm.0, irq)
            .ok_or(HvError::NoMemory)
    }

    fn doorbell_send(&mut self, handle: DoorbellHandle) -> Result<(), HvError> {
        // Bare-metal delivery: a GIC SGI on the doorbell's registered IRQ,
        // dispatched to CPU 0 (the vCPU that owns the doorbell).  Rings
        // are counted here; the actual delivery is recorded by the IRQ
        // handler through `note_delivery`.  Mirrors Gunyah `GH_BELL_SEND`.
        crate::hypervisor::doorbell::ring_sgi(handle)?;
        Ok(())
    }

    fn doorbell_set_mask(
        &mut self,
        handle: DoorbellHandle,
        enable_mask: u64,
        ack_mask: u64,
    ) -> Result<(), HvError> {
        crate::hypervisor::doorbell::set_mask(handle, enable_mask, ack_mask)?;
        log::debug!(
            "doorbell_set_mask: {:?} enable={:#x} ack={:#x}",
            handle, enable_mask, ack_mask
        );
        Ok(())
    }

    fn mem_share(
        &mut self,
        phys: PhysAddr,
        size: usize,
    ) -> Result<ShmemHandle, HvError> {
        // In bare-metal mode "sharing" means the physical range is already
        // in the kernel's address space (identity-mapped by the MMU setup).
        // We just record the handle and return.
        let handle = ShmemHandle(phys as u32 ^ (size as u32));
        log::info!("mem_share: phys={:#x} size={:#x} → handle={:?}", phys, size, handle);
        Ok(handle)
    }
}

// ── Cooperative vCPU switch machinery (bare-metal only) ───────────────────────

/// PSTATE guests run with (Phase 21): EL1h with the IRQ bit *unmasked* so
/// the EL1 physical timer (PPI 30) can interrupt a running vCPU and the
/// co-tenant scheduler can cut its time slice.  Differential: D/A/F stay
/// masked (0x345 = 0x3c5 & ~(1<<7)); the kernel itself always runs with
/// DAIF masked.
const SPSR_GUEST: u64 = 0x345;

/// The tenant whose vCPU is currently executing (None when no guest runs).
static mut ACTIVE_VM: Option<VmHandle> = None;

/// The CPU currently executing a guest vCPU (only CPU 0 hosts tenants, but
/// secondaries' ticks must not preempt what they are not running).
static mut ACTIVE_CPU: usize = 0;

/// Phase 21: while true, a tick inside a guest may preempt it.  Set by the
/// co-tenant scheduler (`vm::sched::run`) for its duration; the earlier
/// single-guest demos (phases 3/14) run with this false and keep their
/// purely cooperative semantics.
static mut PREEMPT_ENABLED: bool = false;

/// Default time slice in ticks, loaded into every `vcpu_run` entry.
static mut QUANTUM_TICKS: u32 = 0;

/// Arm the co-tenant preemption machinery.  Called by `vm::sched::run`.
///
/// # Safety
/// Single-CPU boot context (no guest runs concurrently on other cores).
pub unsafe fn enable_guest_preemption(quantum_ticks: u32) {
    PREEMPT_ENABLED = true;
    QUANTUM_TICKS = quantum_ticks;
    log::info!(
        "phase 21: co-tenant preemption armed (quantum={} ticks)",
        quantum_ticks
    );
}

/// Disarm the co-tenant preemption machinery after the tenant scheduler
/// finishes.  Beware of the Phase-20 `MAX_LOG_LEVEL_FILTER` lesson: log
/// through the kernel's own path; this runs in the boot context where the
/// global filter is intact.
///
/// # Safety
/// No guest may be running when this is called.
pub unsafe fn disable_guest_preemption() {
    PREEMPT_ENABLED = false;
    QUANTUM_TICKS = 0;
}

/// True when a tick that just fired is inside a guest vCPU run and the
/// co-tenant scheduler is armed.  If so, `irq_handler` must hand the tick
/// to `tick_guest` instead of `task::tick_preempt`.
pub fn guest_tick_active() -> bool {
    unsafe {
        PREEMPT_ENABLED && ACTIVE_VM.is_some() && crate::smp::cpu_index() == ACTIVE_CPU
    }
}

/// The tick that just interrupted a guest vCPU: consume one tick of the
/// running tenant's quantum; when the slice is spent, capture the entire
/// preemption frame (the IRQ_ENTRY frame on the guest's stack) into the
/// tenant's context and switch to the tenant's kernel-side continuation
/// (the VMM loop) — which then runs the next tenant.  Never returns when
/// it preempts.
///
/// # Safety
/// Only ever called from `irq_handler` for a tick that landed inside a
/// guest (slot 5, interrupts masked by hardware), with
/// `guest_tick_active()` true.
pub unsafe fn tick_guest(frame: *mut u64) {
    if !PREEMPT_ENABLED || crate::smp::cpu_index() != ACTIVE_CPU {
        return; // stale tick — ignore
    }
    let Some(handle) = ACTIVE_VM else {
        return;
    };
    let backend = &mut *core::ptr::addr_of_mut!(BARE);
    let Some(rec) = backend.find_vm(handle) else {
        return;
    };
    // A tick between `enter_guest`'s return and the flag-clearing tail is
    // not inside guest code — leave it to the task scheduler.
    if !rec.in_guest {
        return;
    }

    if rec.budget_ticks > 0 {
        rec.budget_ticks -= 1;
        return; // slice not spent — keep the tenant running
    }

    // ── Preempt: capture the whole vCPU from the IRQ frame ────────────────
    // Frame (272 B, `IRQ_ENTRY` in vectors.s):
    //   [0..152]  x0-x18 + x30      [160] ELR_EL1   [168] SPSR_EL1
    //   [176..248] x19-x28          [256] x29
    // The frame stays on the guest's stack: context.sp is set to its base,
    // so `restore_preempted_guest` picks it up on the next run.
    let f = frame as *const u64;
    for i in 0..10 {
        rec.guest_ctx.x19_to_x28[i] = core::ptr::read_volatile(f.add(22 + i));
    }
    rec.guest_ctx.fp = core::ptr::read_volatile(f.add(32));
    rec.guest_ctx.sp = frame as u64;
    rec.guest_ctx.lr = restore_preempted_guest as *const () as u64;
    // The stub runs at EL1h with interrupts masked; the guest's own PSTATE
    // (IRQ unmasked) sits in the frame at [sp+168] and is reinstated by the
    // stub's final `eret`.
    rec.guest_ctx.spsr = crate::sched::task::SPSR_KERNEL;
    rec.guest_ctx.ttbr0 = crate::mem::page_table::kernel_l0_phys() as u64;
    rec.preempted = true;
    log::info!(
        "tick: preempting tenant @ guest_PC={:#x} gX10={:#x} gX30={:#x}",
        core::ptr::read_volatile(f.add(20)),
        core::ptr::read_volatile(f.add(10)),
        core::ptr::read_volatile(f.add(19))
    );

    log::trace!(
        "phase 21: preempting guest {:?} (frame={:#x} elr={:#x})",
        handle,
        frame as usize,
        core::ptr::read_volatile(f.add(20))
    );

    // Abandon this execution stream (it lives on the guest's stack) and
    // resume the VMM loop where it entered this guest.  Never returns.
    crate::sched::task::context_switch_preempt(&rec.kernel_ctx);
}

extern "C" {
    /// `restore_preempted_guest` in vectors.s — resurrects a preempted
    /// guest from its captured IRQ frame (see `tick_guest`).
    fn restore_preempted_guest();
}

/// Switch from the kernel to the guest, re-establishing the guest's boot
/// args in x4/x5/x6 (shared-memory base, yield function, guest-context
/// pointer).
///
/// `context_switch` only saves/restores x19–x28 + fp/lr/sp, so the boot
/// args are caller-saved and must be set on *every* entry — first launch
/// and each resume — because the kernel's own execution between a yield
/// and its resume clobbers them.  Everything happens inside one `asm!`
/// block so the compiler cannot interleave instructions between the
/// register loads and the switch; the block must therefore clobber every
/// caller-saved register, since `bl` lets the guest run arbitrary code.
///
/// Also primes `guest_ctx.spsr = SPSR_GUEST` (Phase 21: the guest runs
/// with IRQ enabled for tick-driven co-tenancy preemption) immediately
/// before the switch.
///
/// Returns when the guest yields control back (guest context saved,
/// kernel context restored) — or when the Phase-21 tick preempted the
/// guest and `context_switch_preempt` restored the kernel context.
///
/// # Safety
/// `kernel_ctx` must belong to the calling task and `guest_ctx` to the
/// guest being entered.
unsafe fn enter_guest(
    kernel_ctx: &mut Context,
    guest_ctx: &mut Context,
    boot: [u64; 2],
    uart: u64,
) {
    // Prime the guest PSTATE for a direct entry (see `vcpu_run`: a
    // stub-resumed context keeps its masked SPSR_KERNEL so
    // `restore_preempted_guest` runs with IRQs masked).
    if guest_ctx.lr != restore_preempted_guest as *const () as u64 {
        guest_ctx.spsr = SPSR_GUEST;
    }
    core::arch::asm!(
        "mov x4, {s}",
        "mov x5, {y}",
        "mov x6, {c}",
        "mov x0, {k}",
        "mov x1, {c}",
        "bl context_switch",
        s = in(reg) boot[0],
        y = in(reg) boot[1],
        c = in(reg) core::ptr::addr_of!(*guest_ctx) as u64,
        // The UART base rides in x7 (guests read it in `_start`).  Bind it
        // to the concrete register instead of `in(reg)`: a generic-allocated
        // input could land on x7, and the template's own `mov x7, {u}` would
        // then clobber a DIFFERENT `in(reg)` operand that LLVM placed there
        // earlier (e.g. `{k}`) — sending the save target to 0x60000000.
        in("x7") uart,
        k = in(reg) core::ptr::addr_of_mut!(*kernel_ctx) as u64,
        // `bl` lets the guest run arbitrary code, which clobbers every
        // caller-saved register (x0–x18) — declare them all.  x19–x28,
        // fp and sp are *not* clobbered: `context_switch` saves them into
        // `kernel_ctx` before the guest runs and restores them on yield.
        // (x19 is additionally reserved by LLVM and kept alive on its own.)
        out("x0") _, out("x1") _, out("x2") _, out("x3") _,
        out("x4") _, out("x5") _, out("x6") _,
        out("x8") _, out("x9") _, out("x10") _, out("x11") _,
        out("x12") _, out("x13") _, out("x14") _, out("x15") _,
        out("x16") _, out("x17") _, out("x18") _,
        out("x30") _,
    );
}

/// The guest-facing yield entry: called *by the guest* to hand control
/// back to the kernel (the bare-metal stand-in for a trap exit).
///
/// The guest calls `vm_yield_entry` — a tiny assembly prologue in
/// vectors.s that masks IRQ (guests run with IRQ enabled in Phase 21; the
/// switch to the kernel must never be interrupted mid-way by a
/// preemption tick) and branches here.
///
/// Saves the guest's context into the running VM's `guest_ctx` (whose
/// address was passed to the guest in x6 at launch) and restores the
/// kernel context, so the kernel continues right after its `enter_guest`
/// call.  When the kernel later runs the vCPU again, this function returns
/// and the guest continues where it left off (IRQ state restored by the
/// `context_switch` restore — SPSR_GUEST was primed by `enter_guest`).
#[no_mangle]
pub extern "C" fn vm_yield_entry_masked(guest_ctx: *mut Context) {
    unsafe {
        let backend = &mut *core::ptr::addr_of_mut!(BARE);
        let Some(vm) = backend.running_vm_mut() else {
            log::error!("vm_yield_entry_masked: no running VM");
            return;
        };
        log::info!("yield: entering context_switch (gctx={:#x} kctx={:#x} pc={:#x} k_lr={:#x} k_sp={:#x} k_spsr={:#x})", guest_ctx as u64, &vm.kernel_ctx as *const Context as u64, guest_ctx_pc(guest_ctx), ctx_lr(&vm.kernel_ctx), ctx_sp(&vm.kernel_ctx), ctx_spsr(&vm.kernel_ctx));
        context_switch(guest_ctx, &vm.kernel_ctx as *const Context);
        log::info!("yield: context_switch RETURNED (resumed guest)");
    }
}

fn guest_ctx_pc(ctx: *mut Context) -> u64 {
    unsafe { core::ptr::read_volatile((ctx as *const u64).add(11)) }
}
fn ctx_lr(ctx: &Context) -> u64 {
    unsafe { core::ptr::read_volatile((core::ptr::addr_of!(*ctx) as *const u64).add(11)) }
}
fn ctx_sp(ctx: &Context) -> u64 {
    unsafe { core::ptr::read_volatile((core::ptr::addr_of!(*ctx) as *const u64).add(12)) }
}
fn ctx_spsr(ctx: &Context) -> u64 {
    unsafe { core::ptr::read_volatile((core::ptr::addr_of!(*ctx) as *const u64).add(14)) }
}

/// Park the currently-in-guest tenant in its shmem info block (GUEST_PARKED)
/// so the co-tenant scheduler drops it.  Called from `sync_handler` when a
/// guest's own address fault lands in the kernel's EL1 vectors.
pub unsafe fn park_active_tenant() {
    let Some(handle) = ACTIVE_VM else {
        return;
    };
    let backend = &mut *core::ptr::addr_of_mut!(BARE);
    if let Some(vm) = backend.find_vm(handle) {
        if vm.in_guest {
            crate::vm::sched::set_tenant_state(
                vm.boot[0] as usize,
                crate::vm::sched::GUEST_PARKED,
            );
        }
    }
}

/// The single bare-metal backend instance (see note in `hypervisor/mod.rs`).
///
/// Lives here — not in `mod.rs` — so `vm_yield_entry` (which must not go
/// through the trait object) reaches the exact instance `vcpu_run` switched
/// out of.  The Gunyah backend would never enter a cooperative guest.
pub static mut BARE: BareMetalBackend = BareMetalBackend::new();

// ── Gunyah backend ────────────────────────────────────────────────────────────

/// Gunyah hypercall function identifiers — the `fn` field of the SMCCC
/// vendor-hyp function ID; the full encoding is `GH_HYPERCALL(fn)`.
///
/// These match the upstream Linux driver (`arch/arm64/gunyah/` /
/// `drivers/virt/gunyah/`).
mod gh_hvc {
    /// Identify the Gunyah hypervisor (returns a UID).
    pub const HYP_IDENTIFY: u64 = 0x8000;
    /// Send a doorbell notification to a capability.
    pub const BELL_SEND: u64 = 0x8012;
    /// Configure doorbell flag masking.
    pub const BELL_SET_MASK: u64 = 0x8015;
    /// Send a message on a message queue.
    pub const MSGQ_SEND: u64 = 0x801B;
    /// Receive a message from a message queue.
    pub const MSGQ_RECV: u64 = 0x801C;
    /// Map memory into an address space (from a memory extent).
    pub const ADDRSPACE_MAP: u64 = 0x802B;
    /// Unmap memory from an address space.
    pub const ADDRSPACE_UNMAP: u64 = 0x802C;
    /// Donate memory from one memory extent to another.
    pub const MEMEXTENT_DONATE: u64 = 0x8061;
    /// Donate CPU time to a vCPU.
    pub const VCPU_RUN: u64 = 0x8065;

    /// Encode a function number as an SMCCC vendor-hyp hypercall:
    ///   SMC64 | FAST_CALL | OEN=VENDOR_HYP(6) | CALL_TYPE_HYPERCALL(2)<<14
    pub const fn hypercall(fn_num: u64) -> u64 {
        0x8000_0000
            | 0x4000_0000
            | (6u64 << 24)
            | (2u64 << 14)
            | (fn_num & 0x3FFF)
    }
}

/// `GUNYAH_HYPERCALL_MSGQ_TX_FLAGS_PUSH` — immediately raise the RX vIRQ
/// on the receiving VM.
pub const MSGQ_TX_FLAGS_PUSH: u64 = 1;

/// Address-space capability representing the caller's own address space.
const ADDRSPACE_SELF_CAP: u64 = 0;

pub struct GunyahBackend {
    next_handle: u32,
}

impl GunyahBackend {
    pub const fn new() -> Self {
        Self { next_handle: 1 }
    }

    /// Issue an SMCCC-style HVC with up to six input arguments (x0..x5,
    /// per `arm_smccc_1_1_hvc`); returns x0..x3.
    #[inline]
    unsafe fn hvc(args: [u64; 6]) -> (u64, u64, u64, u64) {
        let r0: u64;
        let r1: u64;
        let r2: u64;
        let r3: u64;
        core::arch::asm!(
            "hvc #0",
            inlateout("x0") args[0] => r0,
            inlateout("x1") args[1] => r1,
            inlateout("x2") args[2] => r2,
            inlateout("x3") args[3] => r3,
            in("x4") args[4],
            in("x5") args[5],
            lateout("x4") _,
            lateout("x5") _,
            lateout("x6") _,
            options(nomem)
        );
        (r0, r1, r2, r3)
    }
}

impl Hypervisor for GunyahBackend {
    fn detect() -> bool {
        // Issue HYP_IDENTIFY — if Gunyah is present it returns a specific
        // UID in x1..x4 (bytes {0x19, 0x47, 0x55, 0x4e} = "GUY\x19").
        let func = gh_hvc::hypercall(gh_hvc::HYP_IDENTIFY);
        let (r0, r1, r2, r3) = unsafe { Self::hvc([func, 0, 0, 0, 0, 0]) };
        // Error code in r0 must be OK; the UID signature lives in r1/r2/r3.
        r0 == 0 && r1 == 0x4755_5919 && r2 == 0 && r3 == 0
    }

    fn vm_create(&mut self, _config: VmConfig) -> Result<VmHandle, HvError> {
        // TODO: allocate a Gunyah VM capability via the RM.  Requires a
        // Gunyah environment (QEMU fork / SA8295P) — see module docs.
        Err(HvError::NotSupported)
    }

    fn vm_destroy(&mut self, _handle: VmHandle) -> Result<(), HvError> {
        Err(HvError::NotSupported)
    }

    fn vcpu_run(&mut self, _vm: VmHandle, _vcpu: u32) -> Result<VcpuExit, HvError> {
        // Requires a vCPU capability from the RM; without one this would
        // fail the hypercall with NOSUCHCAP.  Kept explicit rather than
        // issuing a doomed call.
        Err(HvError::NotSupported)
    }

    fn vcpu_stop(&mut self, _vm: VmHandle, _vcpu: u32) -> Result<(), HvError> {
        Err(HvError::NotSupported)
    }

    fn msgq_create(&mut self, _vm: VmHandle, _depth: u32) -> Result<MsgqHandle, HvError> {
        // Message queues are created by the RM and translated to the
        // participating VMs; a raw create needs the RM protocol.
        Err(HvError::NotSupported)
    }

    fn msgq_send(&mut self, mq: MsgqHandle, msg: &[u8]) -> Result<bool, HvError> {
        if msg.len() > MSGQ_MAX_MSG_SIZE {
            return Err(HvError::BadState);
        }
        let func = gh_hvc::hypercall(gh_hvc::MSGQ_SEND);
        let (r0, r1, _, _) = unsafe {
            Self::hvc([func, mq.0 as u64, msg.len() as u64, msg.as_ptr() as u64, MSGQ_TX_FLAGS_PUSH, 0])
        };
        match r0 {
            0 => Ok(r1 != 0), // a1 = "ready" (more room)
            e => Err(super::HvError::HypercallFailed(e)),
        }
    }

    fn msgq_recv(&mut self, mq: MsgqHandle, buf: &mut [u8]) -> Result<(usize, bool), HvError> {
        let func = gh_hvc::hypercall(gh_hvc::MSGQ_RECV);
        let (r0, r1, r2, _) = unsafe {
            Self::hvc([func, mq.0 as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0])
        };
        match r0 {
            0 => Ok((r1 as usize, r2 != 0)), // a1 = received size, a2 = ready
            e => Err(super::HvError::HypercallFailed(e)),
        }
    }

    fn doorbell_create(
        &mut self,
        _owner_vm: VmHandle,
        _irq: u32,
    ) -> Result<DoorbellHandle, HvError> {
        // Doorbells are created by the RM and translated to the target VM.
        Err(HvError::NotSupported)
    }

    fn doorbell_send(&mut self, handle: DoorbellHandle) -> Result<(), HvError> {
        let func = gh_hvc::hypercall(gh_hvc::BELL_SEND);
        let (r0, _old_flags, _, _) = unsafe {
            Self::hvc([func, handle.0 as u64, 0, 0, 0, 0])
        };
        if r0 == 0 {
            Ok(())
        } else {
            Err(HvError::HypercallFailed(r0))
        }
    }

    fn doorbell_set_mask(
        &mut self,
        handle: DoorbellHandle,
        enable_mask: u64,
        ack_mask: u64,
    ) -> Result<(), HvError> {
        let func = gh_hvc::hypercall(gh_hvc::BELL_SET_MASK);
        let (r0, _, _, _) = unsafe {
            Self::hvc([func, handle.0 as u64, enable_mask, ack_mask, 0, 0])
        };
        if r0 == 0 {
            Ok(())
        } else {
            Err(HvError::HypercallFailed(r0))
        }
    }

    fn mem_share(&mut self, phys: PhysAddr, size: usize) -> Result<ShmemHandle, HvError> {
        // Gunyah maps shared ranges via memory extents: donate the frames
        // from the caller's address space into an extent, then map the
        // extent into the target address space.  Without an RM-created
        // extent capability this fails gracefully (NOSUCHCAP): we issue the
        // real hypercalls and report the error, exactly as a driver would
        // when handed no capabilities.
        let donate = gh_hvc::hypercall(gh_hvc::MEMEXTENT_DONATE);
        // a1 = options (donate-to-child), a2 = from (own address space),
        // a3 = to (extent capability), a4 = offset, a5 = size.
        let (r0, cap, _, _) = unsafe {
            Self::hvc([
                donate,
                0,
                ADDRSPACE_SELF_CAP,
                0,
                phys as u64,
                size as u64,
            ])
        };
        if r0 != 0 {
            return Err(HvError::HypercallFailed(r0));
        }

        let map = gh_hvc::hypercall(gh_hvc::ADDRSPACE_MAP);
        // a1 = address-space cap, a2 = extent cap, a3 = vbase, a4 = attrs
        // (GUNYAH_PAGETABLE_ACCESS_RW), a5 = flags.
        let (m0, _, _, _) = unsafe {
            Self::hvc([
                map,
                ADDRSPACE_SELF_CAP,
                cap,
                phys as u64,
                0b110, // RW
                0,
            ])
        };
        if m0 != 0 {
            return Err(HvError::HypercallFailed(m0));
        }

        log::info!("mem_share: phys={:#x} size={:#x} extent={:?}", phys, size, cap);
        Ok(ShmemHandle(cap as u32))
    }
}
