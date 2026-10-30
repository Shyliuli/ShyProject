    .section .text
    .globl arch_set_trap
arch_set_trap:
    csrw stvec, a0
    ret

    .globl arch_set_timer
arch_set_timer:
    mv a1, a0
    csrr a0, time
    add a0, a0, a1
    li a7, 0                 /* SBI TIME extension: set_timer */
    ecall
    ret

    .globl arch_fence_i
arch_fence_i:
    fence.i
    ret

    .globl arch_atomic_exchange
arch_atomic_exchange:
    amoswap.w.aqrl a0, a1, (a0)
    ret

    .globl arch_enter_user
arch_enter_user:
    csrw sscratch, a2        /* kernel stack for trap entry */
    csrw sepc, a0
    mv sp, a1
    li t0, 0x100             /* clear SPP: return to U-mode */
    csrc sstatus, t0
    sret

/* void arch_shutdown(int status): report success/failure to QEMU virt. */
    .globl arch_shutdown
arch_shutdown:
    /* Use the kernel table so the QEMU test device is mapped, then power off. */
    la      t0, boot_root
    li      t1, 8
    slli    t1, t1, 60
    srli    t2, t0, 12
    or      t1, t1, t2
    csrw    satp, t1
    sfence.vma
    li      t0, 0x100000
    li      t1, 0x5555
    beqz    a0, 2f
    slli    t1, a0, 16
    li      t2, 0x3333
    or      t1, t1, t2
2:
    sw      t1, 0(t0)
1:
    j       1b

/* void arch_enable_timer(void): supervisor timer interrupt on. */
    .globl arch_enable_timer
arch_enable_timer:
    li      t0, 0x20            /* sie.STIE; sstatus.SIE stays off in the kernel */
    csrs    sie, t0
    ret

/* The shared kernel calls `scheduler()`; on RISC-V it is the trap return path. */
    .globl scheduler
scheduler:
    j       arch_scheduler

    .section .bss
    .align 12
    .globl boot_root
boot_root:
    .space 4096
    .globl boot_t0
boot_t0:
    .space 4096
