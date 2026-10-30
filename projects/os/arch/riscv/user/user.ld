ENTRY(_start)
SECTIONS
{
    . = 0;
    .header : { *(.header) }
    . = 0x100;
    .text : { *(.text.start) *(.text*) }
    .rodata : { *(.rodata*) }
    .data : { *(.data*) }
    .bss : { *(.bss*) *(COMMON) }
    /DISCARD/ : { *(.eh_frame*) *(.note*) *(.riscv.attributes) *(.comment) }
}
