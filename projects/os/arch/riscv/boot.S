/* ShyOS RISC-V entry: OpenSBI leaves the kernel in S-mode at 0x80200000. */
    .section .text.start, "ax"
    .globl _start
_start:
    la      sp, boot_stack_top
    la      t0, trap_entry
    csrw    stvec, t0
    call    kmain
1:
    j       1b

/* void user_enter(void *entry, void *user_sp, void *kernel_sp); */
    .section .text, "ax"
    .globl user_enter
user_enter:
    csrw    sscratch, a2          /* kernel stack for traps */
    csrw    sepc, a0              /* return address for sret */
    mv      sp, a1                /* user stack */
    li      t0, 0x100             /* SPP = 0: sret returns to U-mode */
    csrc    sstatus, t0
    sret

/* Trap entry: swap in the kernel stack, save the caller-saved set. */
    .globl trap_entry
    .align 4
trap_entry:
    csrrw   sp, sscratch, sp
    addi    sp, sp, -128
    sd      ra,   0(sp)
    sd      t0,   8(sp)
    sd      t1,  16(sp)
    sd      t2,  24(sp)
    sd      a0,  32(sp)
    sd      a1,  40(sp)
    sd      a2,  48(sp)
    sd      a3,  56(sp)
    sd      a4,  64(sp)
    sd      a5,  72(sp)
    sd      a6,  80(sp)
    sd      a7,  88(sp)
    csrr    t0, scause
    li      t1, 8                 /* ecall from U-mode */
    bne     t0, t1, trap_unexpected
    call    user_exit
    ld      ra,   0(sp)
    ld      t0,   8(sp)
    ld      t1,  16(sp)
    ld      t2,  24(sp)
    ld      a0,  32(sp)
    ld      a1,  40(sp)
    ld      a2,  48(sp)
    ld      a3,  56(sp)
    ld      a4,  64(sp)
    ld      a5,  72(sp)
    ld      a6,  80(sp)
    ld      a7,  88(sp)
    addi    sp, sp, 128
    csrrw   sp, sscratch, sp
    sret

    .section .bss
    .align 4
    .globl boot_stack
boot_stack:
    .space 4096
    .globl boot_stack_top
boot_stack_top:
    .space 4096
    .globl user_stack
user_stack:
    .space 4096
    .globl user_stack_top
user_stack_top:
