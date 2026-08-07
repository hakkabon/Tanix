/*
 * Linker script for the server binaries.
 *
 * Each server is linked at its final runtime physical address (LINK_BASE,
 * passed via --defsym from build.rs).  The address is fixed so that every
 * absolute pointer inside the image (function pointers in vtables, static
 * data) is correct the moment the kernel copies the binary there — the
 * servers are ordinary statically linked executables, exactly like the
 * kernel itself, and no relocation machinery is needed.
 *
 * The fixed bases live in kernel/src/server.rs and MUST match the values
 * in each crate's build.rs.
 */

OUTPUT_ARCH(aarch64)
ENTRY(_start)

SECTIONS
{
    . = LINK_BASE;

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
