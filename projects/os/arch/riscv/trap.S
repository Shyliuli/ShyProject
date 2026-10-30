/* Supervisor-mode entry points: page-table control, trap entry, and the
   scheduler return path. The trap frame lives on the process kernel stack. */
    .section .text

/* void arch_vm_enable(unsigned long root); */
    .globl arch_vm_enable
arch_vm_enable:
    li      t0, 8
    slli    t0, t0, 60          /* Sv39 */
    srli    t1, a0, 12
    or      t0, t0, t1
    csrw    satp, t0
    sfence.vma
    ret

/* void arch_vm_switch(unsigned long root); */
    .globl arch_vm_switch
arch_vm_switch:
    li      t0, 8
    slli    t0, t0, 60
    srli    t1, a0, 12
    or      t0, t0, t1
    csrw    satp, t0
    sfence.vma
    ret

/* void arch_vm_flush(void); */
    .globl arch_vm_flush
arch_vm_flush:
    sfence.vma
    ret

/* Trap entry from user mode: swap in the kernel stack and spill x1..x31. */
    .globl trap_entry
    .align 4
trap_entry:
    csrrw   sp, sscratch, sp    /* sscratch=0 in the kernel, kernel top in user mode */
    bnez    sp, 1f
    csrrw   sp, sscratch, sp    /* supervisor fault: retain the kernel stack */
1:
    addi    sp, sp, -288
    sd      zero, 0(sp)
    sd      x1,   8(sp)
    sd      x3,  24(sp)
    sd      x4,  32(sp)
    sd      x5,  40(sp)
    sd      x6,  48(sp)
    sd      x7,  56(sp)
    sd      x8,  64(sp)
    sd      x9,  72(sp)
    sd      x10, 80(sp)
    sd      x11, 88(sp)
    sd      x12, 96(sp)
    sd      x13,104(sp)
    sd      x14,112(sp)
    sd      x15,120(sp)
    sd      x16,128(sp)
    sd      x17,136(sp)
    sd      x18,144(sp)
    sd      x19,152(sp)
    sd      x20,160(sp)
    sd      x21,168(sp)
    sd      x22,176(sp)
    sd      x23,184(sp)
    sd      x24,192(sp)
    sd      x25,200(sp)
    sd      x26,208(sp)
    sd      x27,216(sp)
    sd      x28,224(sp)
    sd      x29,232(sp)
    sd      x30,240(sp)
    sd      x31,248(sp)
    csrr    t0, sstatus
    sd      t0, 280(sp)
    andi    t1, t0, 0x100
    bnez    t1, 2f
    csrr    t0, sscratch        /* original user sp */
    j       3f
2:
    addi    t0, sp, 288         /* original supervisor sp */
3:
    sd      t0, 16(sp)
    csrw    sscratch, zero      /* nested supervisor faults retain this stack */
    csrr    t0, sepc
    sd      t0, 256(sp)
    csrr    t0, scause
    sd      t0, 264(sp)
    csrr    t0, stval
    sd      t0, 272(sp)
    mv      a0, sp
    tail    riscv_trap          /* handler schedules; it never returns */

/* The scheduler picks the next process and resumes it in user mode. */
    .globl arch_scheduler
arch_scheduler:
    call    proc_next           /* a0 = struct proc*, ctx is at offset 0 */
    la      t0, current
    sd      a0, 0(t0)

    ld      t1, 280(a0)         /* ctx.space: page-table root */
    beqz    t1, 1f
    li      t0, 8
    slli    t0, t0, 60
    srli    t2, t1, 12
    or      t0, t0, t2
    csrw    satp, t0
    sfence.vma
1:
    ld      t1, 264(a0)         /* ctx.ksp: kernel stack top */
    csrw    sscratch, t1
    ld      t1, 272(a0)         /* ctx.epc */
    csrw    sepc, t1

    /* Return to U-mode with interrupts enabled. Kernel copies use physical
       aliases, so SUM stays off and accidental user-VA accesses fault. */
    li      t0, 0x40100
    csrc    sstatus, t0
    li      t0, 0x20
    csrs    sstatus, t0

    ld      x1,   8(a0)
    ld      x3,  24(a0)
    ld      x4,  32(a0)
    ld      x5,  40(a0)
    ld      x6,  48(a0)
    ld      x7,  56(a0)
    ld      x8,  64(a0)
    ld      x9,  72(a0)
    ld      x11, 88(a0)
    ld      x12, 96(a0)
    ld      x13,104(a0)
    ld      x14,112(a0)
    ld      x15,120(a0)
    ld      x16,128(a0)
    ld      x17,136(a0)
    ld      x18,144(a0)
    ld      x19,152(a0)
    ld      x20,160(a0)
    ld      x21,168(a0)
    ld      x22,176(a0)
    ld      x23,184(a0)
    ld      x24,192(a0)
    ld      x25,200(a0)
    ld      x26,208(a0)
    ld      x27,216(a0)
    ld      x28,224(a0)
    ld      x29,232(a0)
    ld      x30,240(a0)
    ld      x31,248(a0)
    ld      sp, 256(a0)         /* user stack first: a0 is still the base */
    ld      x10, 80(a0)         /* a0 last below the switch */
    sret
