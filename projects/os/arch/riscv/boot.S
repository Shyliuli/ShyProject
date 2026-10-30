/* ShyOS RISC-V entry: OpenSBI leaves the kernel in S-mode at 0x80200000. */
    .section .text.start, "ax"
    .globl _start
_start:
    la      sp, boot_stack_top
    csrw    sscratch, zero
    la      t0, trap_entry
    csrw    stvec, t0
    call    kmain
1:
    j       1b

    .section .bss
    .align 4
    .globl boot_stack
boot_stack:
    .space 8192
    .globl boot_stack_top
boot_stack_top:
