#ifndef SHYOS_KERNEL_ARCH_H
#define SHYOS_KERNEL_ARCH_H

#include <stdint.shyh>

struct proc;
struct context;

/* Devices and machine control. */
void arch_uart_putc(int c);
int arch_uart_getc(void);
void arch_shutdown(int status);
void arch_set_trap(void *entry);
void arch_set_timer(uint32_t ticks);
void arch_fence_i(void);
int arch_atomic_exchange(volatile int *address, int value);
void arch_enter_user(void *entry, void *user_sp, void *kernel_sp);

/* Process address spaces. `arch_user_ptr` maps a user address of `p` to a
   kernel-usable pointer, so syscalls can copy through it. */
void *arch_user_ptr(struct proc *p, unsigned int user_addr);
void arch_space_alloc(struct proc *p);
void arch_space_free(struct proc *p);
int arch_space_copy(struct proc *parent, struct proc *child);
void arch_space_load(struct proc *p, const void *src, unsigned int n);

/* Enable the periodic timer interrupt. */
void arch_enable_timer(void);

/* Page tables (RISC-V); the Shy backend has no equivalent and never calls them. */
void arch_vm_init(void);
unsigned long arch_vm_kernel_root(void);
void arch_vm_enable(unsigned long root);
unsigned long arch_user_space(struct proc *p);

/* Return-value register of a saved user context. */
void ctx_set_ret(struct context *ctx, int value);

#endif
