/*
 * Linker script for the Zephyr-stub guest VM.
 *
 * The stub is loaded by the kernel into guest RAM allocated by the frame
 * allocator.  The exact physical address is not known at compile time, so
 * this script uses a position-independent base of 0x0 — the kernel loader
 * adds the actual RAM base when copying the binary.
 *
 * For a flat (non-ELF) load the entry point IS address 0 within the image,
 * so _start must be the very first byte.
 */

OUTPUT_ARCH(aarch64)
ENTRY(_start)

SECTIONS
{
    . = 0x0;

    .text : {
        KEEP(*(.text._start))
        *(.text.boot)
        *(.text*)
    }

    .rodata : ALIGN(8) { *(.rodata*) }
    .data   : ALIGN(8) { *(.data*)   }
    .got    : ALIGN(8) { *(.got*)    }

    .bss (NOLOAD) : ALIGN(8) {
        __bss_start = .;
        *(.bss*)
        *(COMMON)
        __bss_end = .;
    }

    . = ALIGN(4096);
    __image_end = .;

    /DISCARD/ : {
        *(.eh_frame) *(.eh_frame_hdr) *(.comment) *(.note*)
    }
}
