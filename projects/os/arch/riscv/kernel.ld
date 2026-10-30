ENTRY(_start)
SECTIONS
{
    . = 0x80200000;
    .text : {
        *(.text.start)
        *(.text*)
    }
    .rodata : { *(.rodata*) }
    .data : { *(.data*) }
    .bss : { *(.bss*) *(COMMON) }
    /DISCARD/ : { *(.eh_frame*) *(.note*) *(.riscv.attributes) *(.comment) }
}
