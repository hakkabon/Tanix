OUTPUT_ARCH(aarch64)
ENTRY(_start)

/*
 * QEMU sbsa-ref machine memory map (aarch64) — Phase 16:
 *   0x10000000000  DDR start (1 TiB-aligned; -m 1G default)
 *   0x10000080000  Kernel load address (ELF is loaded at its link address)
 *
 * Stack: grows downward from __stack_top = load_addr + 64K.
 * The stack sits in the 64 KiB window *before* the kernel .text, which is
 * always available on a freshly booted sbsa-ref machine (RAM starts at
 * 0x10000000000).
 */

SECTIONS
{
    . = 0x10000080000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text._start))   /* entry point must be first */
        *(.text.boot)           /* other early boot code */
        *(.text.monitor)        /* Phase 16: EL3 monitor vectors + SMC entry */
        *(.text*)
    }

    .rodata : ALIGN(8) {
        *(.rodata*)
    }

    .data : ALIGN(8) {
        *(.data*)
    }

    /* Small data / GOT — needed even if Rust does not emit much here */
    .got : ALIGN(8) {
        *(.got*)
    }

    .bss (NOLOAD) : ALIGN(8) {
        __bss_start = .;
        *(.bss*)
        *(COMMON)
        __bss_end = .;
    }

    /*
     * Phase 16: EL3 monitor stack (BSS, used only at EL3 before the MMU is
     * on — identity mapped by the EL1 kernel table, but never touched by
     * EL1 code).
     */
    . = ALIGN(4096);
    __el3_stack_bottom = .;
    . = . + 0x8000;
    __el3_stack_top = .;

    /*
     * Phase 16: secure-world payload (vectors + code + data), copied by the
     * EL3 monitor into secure RAM at 0x20000000 (trust boundary: NS code
     * cannot reach it).  Must be position-independent and self-contained:
     * the whole section is relocated as one blob.
     */
    . = ALIGN(0x8000);
    .sec_payload : {
        KEEP(*(.sec_payload*))
    }
    __sec_payload_end = .;

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note*)
    }
}

/*
 * Stack lives in the 64 KiB below the kernel load address.
 * __stack_top is the initial SP value: the stack grows downward from here.
 */
__stack_top = 0x10000080000;
