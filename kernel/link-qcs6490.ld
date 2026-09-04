OUTPUT_ARCH(aarch64)
ENTRY(_start)

/*
 * Phase 23: QCS6490 real-silicon linker script (Dragon Q6A / RUBIK Pi 3).
 *
 * Load address: 0x80080000
 *   QCS6490 LPDDR5 starts at 0x8000_0000 (the 32-bit DRAM window).
 *   The first 512 KiB (0x8000_0000 .. 0x8008_0000) are reserved as the
 *   stack area (grows downward from __stack_top = 0x80080000), mirroring
 *   the virt layout.  UEFI will load the EFI application at exactly this
 *   address (the PE ImageBase encodes it) or fail visibly if it cannot.
 *
 * Sections are 4 KiB-aligned so elf2efi.py can produce valid PE/COFF
 * section RVAs (EFI requires SECT_ALIGN = 0x1000).
 *
 * No EL3 monitor, no sec_payload, no virtio-mmio — this is a UEFI-only
 * bare-metal probe target.  The monitor sections are kept as empty stubs
 * so the linker script is structurally identical to link.ld (making future
 * merges straightforward).
 */

SECTIONS
{
    . = 0x80080000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text._start))   /* _start must be first (EFI entry point) */
        *(.text.boot)
        *(.text.monitor)        /* empty on qcs6490 — no EL3 monitor */
        *(.text*)
    }

    .rodata : ALIGN(4096) {
        *(.rodata*)
    }

    .data : ALIGN(4096) {
        *(.data*)
    }

    .got : ALIGN(4096) {
        *(.got*)
    }

    .bss (NOLOAD) : ALIGN(4096) {
        __bss_start = .;
        *(.bss*)
        *(COMMON)
        __bss_end = .;
    }

    /*
     * EL3 stack symbols must exist (referenced from the virt/sbsa _start
     * fallback stubs even when not used).  Zero-size on qcs6490.
     */
    . = ALIGN(4096);
    __el3_stack_bottom = .;
    __el3_stack_top    = .;

    /*
     * Secure payload — empty on qcs6490 (QSEE owns the secure world).
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
 * Stack: 512 KiB below the kernel load address.
 * __stack_top is the initial SP value (stack grows downward).
 */
__stack_top = 0x80080000;
