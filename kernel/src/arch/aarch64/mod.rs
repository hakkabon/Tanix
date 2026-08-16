pub mod boot;
pub mod acpi;
pub mod cache;
pub mod efi;
pub mod exception;
pub mod fdt;
pub mod gic;
pub mod machine;
pub mod mmu;
pub mod monitor;
pub mod psci;
pub mod timer;
pub mod uart;

/// The platform this kernel was built for (`virt` by default, `sbsa-ref`
/// with `--features sbsa-ref`).
pub use machine::machine;

/// Initialise all aarch64 subsystems in dependency order.
///
/// Called from `kmain` once BSS is zeroed and SP is valid.
pub fn init() {
    // 0. Bring up the UART and register it as the log backend.
    //    This must come first so all subsequent init() calls can log.
    uart::init();
    uart::logger_init();
    log::info!("Tanix kernel — aarch64 init");

    // 1. Install exception vector table so we catch faults immediately.
    exception::init();
    log::info!("exception vectors installed");

    // 2. Configure TCR / MAIR (MMU stays off until frame allocator is ready).
    mmu::init();

    // 3. Initialise the Generic Interrupt Controller (GICv3).
    gic::init();
    log::info!("GICv3 initialised");

    // 4. Disarm the timer (will be armed by the scheduler tick).
    timer::init();
    log::info!("timer ready (freq={} Hz)", timer::frequency());
}

/// Spin forever — used as the idle loop and as a last-resort halt.
///
/// `wfi` (Wait For Interrupt) puts the core into a low-power state until
/// the next interrupt fires, which is better than a busy spin.
pub fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}
