/* Link layout for the Molt kernel on the QEMU RISC-V `virt` board.
 *
 * OpenSBI runs in M-mode, loads this ELF, and jumps to its entry (`_start`) in
 * S-mode at the fixed S-mode payload address 0x8020_0000. The `_start` shim
 * (defined in the `molt-riscv` crate) must be the very first thing in `.text`,
 * so it is kept ahead of the rest of the section. A block of zeroed RAM after
 * `.bss` provides the initial boot stack; `__bss_start`/`__bss_end` are aligned
 * to 8 bytes so the shim can clear `.bss` with doubleword stores. */

OUTPUT_ARCH(riscv)
ENTRY(_start)

MEMORY {
    RAM (rwxa) : ORIGIN = 0x80200000, LENGTH = 126M
}

SECTIONS {
    . = ORIGIN(RAM);

    .text : ALIGN(4K) {
        KEEP(*(.text._start))
        *(.text .text.*)
    } > RAM

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    } > RAM

    .data : ALIGN(4K) {
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

    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
    }
}
