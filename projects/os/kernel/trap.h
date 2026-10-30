#ifndef SHYOS_KERNEL_TRAP_H
#define SHYOS_KERNEL_TRAP_H

#include <stdint.shyh>

#ifdef SHYOS_RISCV
/* The full user register file, saved by the trap entry. */
struct trapframe {
    unsigned long regs[32];
    unsigned long epc;
    unsigned long cause;
    unsigned long badaddr;
};

#define NREG 32

struct context {
    unsigned long regs[32];
    unsigned long sp;    /* user stack pointer */
    unsigned long ksp;   /* kernel stack top */
    unsigned long epc;   /* user program counter */
    unsigned long space; /* satp root page, physical */
    unsigned long valid; /* 0 until the process has run */
};
#else
struct trapframe {
    uint32_t r0;
    uint32_t r1;
    uint32_t r2;
    uint32_t r3;
    uint32_t r4;
    uint32_t r5;
    uint32_t r6;
    uint32_t r7;
    uint32_t r8;
    uint32_t r9;
    uint32_t ra;
    uint32_t rb;
    uint32_t rc;
    uint32_t rd;
    uint32_t re;
    uint32_t rf;
    uint32_t sp;  // kernel sp after hardware swap and our pushes
    uint32_t ksp; // user sp after hardware swap
    uint32_t epc;
    uint32_t segs;
    uint32_t sege;
    uint32_t cause;
};

#define NREG 16

struct context {
    uint32_t regs[16];
    uint32_t sp;  // user sp
    uint32_t ksp; // kernel sp (top of empty kernel stack)
    uint32_t epc;
    uint32_t segs;
    uint32_t sege;
    uint32_t valid;
};
#endif


void trap_save_context(struct trapframe *tf);
void ctx_set_current_ret(uint32_t value);
void ctx_reset_current(uint32_t sp);
void ctx_retry_current_syscall(void);
void trap_handler(uint32_t cause, uint32_t r0, uint32_t r1, uint32_t r2, uint32_t r3);

/* Shared syscall semantics: every architecture dispatches here. */
int syscall_dispatch(int number, unsigned int a0, unsigned int a1, unsigned int a2);

#endif
