/* OpenSBI loads this S-mode image at 0x80200000.
 * Keep `_start` first, align permission boundaries to 4 KiB, and include
 * `.bss` plus the boot stack in the writable span. */

OUTPUT_ARCH(riscv)
ENTRY(_start)

MEMORY {
    RAM (rwxa) : ORIGIN = 0x80200000, LENGTH = 126M
}

SECTIONS {
    . = ORIGIN(RAM);

    .text : ALIGN(4K) {
        __text_start = .;
        KEEP(*(.text._start))
        *(.text .text.*)
        . = ALIGN(4K);
        __text_end = .;
    } > RAM

    .rodata : ALIGN(4K) {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        . = ALIGN(4K);
        __rodata_end = .;
    } > RAM

    .data : ALIGN(4K) {
        __data_start = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
    } > RAM

    .bss (NOLOAD) : ALIGN(8) {
        __bss_start = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    } > RAM

    . = ALIGN(16);
    . += 128K;
    __molt_stack_top = .;

    . = ALIGN(4K);
    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
    }
}
