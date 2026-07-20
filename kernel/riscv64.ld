/* Link layout for the Molt kernel on the QEMU RISC-V `virt` board.
 *
 * OpenSBI runs in M-mode, loads this ELF, and jumps to its entry (`_start`) in
 * S-mode at the fixed S-mode payload address 0x8020_0000. The `_start` shim
 * (defined in the `molt-riscv` crate) must be the very first thing in `.text`,
 * so it is kept ahead of the rest of the section. A block of zeroed RAM after
 * `.bss` provides the initial boot stack; `__bss_start`/`__bss_end` are aligned
 * to 8 bytes so the shim can clear `.bss` with doubleword stores.
 *
 * Every section boundary the kernel maps is 4 KiB aligned and exported as a
 * symbol pair, because `paging::init` gives each one its own permissions: a
 * section that shared a page with the next one would have to be mapped with
 * the union of their rights, which is how a writable `.text` happens. The
 * writable span deliberately runs from `.data` to `__kernel_end` so `.bss` and
 * the boot stack are covered by it. */

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
