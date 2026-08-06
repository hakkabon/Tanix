OUTPUT_ARCH(aarch64)
ENTRY(_start)

/*
 * QEMU virt machine memory map (aarch64):
 *   0x40000000  DDR start
 *   0x40080000  Kernel load address (matches QEMU -kernel default for virt)
 *
 * Stack: grows downward from __stack_top = load_addr + 64K.
 * The stack sits in the 64 KiB window *before* the kernel .text, which is
 * always available on a freshly booted virt machine.
 */

SECTIONS
{
    . = 0x40080000;

    .text : {
        KEEP(*(.text._start))   /* entry point must be first */
        *(.text.boot)           /* other early boot code */
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
__stack_top = 0x40080000;
