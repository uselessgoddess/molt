ENTRY(_start)

PHDRS
{
  text PT_LOAD FLAGS(5);
  rodata PT_LOAD FLAGS(4);
  data PT_LOAD FLAGS(6);
}

SECTIONS
{
  __molt_image_base = DEFINED(__molt_image_base_override) ? __molt_image_base_override : 0x00c0000000000000;
  . = __molt_image_base;
  .text : ALIGN(4096) { KEEP(*(.text._start)) *(.text .text.* .ltext .ltext.*) } :text
  . = ALIGN(4096);
  .rodata : { *(.rodata .rodata.* .lrodata .lrodata.*) } :rodata
  . = ALIGN(4096);
  .data : { *(.sdata .sdata.*) *(.data .data.* .ldata .ldata.*) *(.got .got.*) } :data
  .bss (NOLOAD) : { *(.sbss .sbss.*) *(.bss .bss.* .lbss .lbss.*) *(COMMON) } :data
  /DISCARD/ : { *(.eh_frame .eh_frame_hdr .comment .note .note.*) }
}
