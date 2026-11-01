#ifndef SHYOS_MEMLAYOUT_H
#define SHYOS_MEMLAYOUT_H

#ifdef SHYOS_RISCV
/* QEMU virt: RAM starts at 0x8000_0000 and the kernel is loaded at 0x8020_0000. */
#define PHYS_BASE 0x80000000
#define KERNBASE 0x80200000
#define KERN_HEAP_START 0x80800000
#define KERN_HEAP_END 0x87800000
#define SCHED_STACK_TOP 0x87fff000
#define USER_SEG_START 0
#define USER_SEG_SIZE 0x00100000
#define USER_SEG_COUNT 16
#define USER_ARG0_ADDR 0x000eef80
#define USER_ARG_ADDR 0x000ef000
#define PHYSTOP 0x88000000
#else
#define KERNBASE 0x00000100

#ifdef SHYOS_SMALL
// 6 MiB 内存配置：内核镜像（含全部用户镜像）可到 1.25 MiB，堆和调度栈在
// 1.5 MiB 以下，段区从 1.5 MiB 开始，12 个 384 KiB 段正好到 6 MiB。
//
// 用户镜像必须小于 USER_ARG0_ADDR（约 315 KiB）。busybox 现在约 193 KiB，
// 其中大部分是 Shy 后端的指令膨胀：ShyISA 没有基址+偏移寻址，-O0 下每次
// 访问局部变量都要 setn/adda/geta 三条 12 字节指令。
#define KERN_HEAP_START 0x00140000
#define KERN_HEAP_END 0x00180000
#define SCHED_STACK_TOP 0x00178000
#define USER_SEG_START 0x00180000
#define USER_SEG_SIZE 0x00060000
#define USER_SEG_COUNT 12
#define USER_ARG0_ADDR 0x0004ef80
#define USER_ARG_ADDR 0x0004f000
#define PHYSTOP 0x00600000
#else
// The kernel heap must start above the linked kernel image, which includes
// the embedded user images. All-ShyC builds are larger than 1 MiB.
#define KERN_HEAP_START 0x00200000
#define KERN_HEAP_END 0x01000000
#define SCHED_STACK_TOP 0x07f00000
#define USER_SEG_START 0x01000000
#define USER_SEG_SIZE 0x00100000
#define USER_SEG_COUNT 48
#define USER_ARG0_ADDR 0x000eef80
#define USER_ARG_ADDR 0x000ef000
#define PHYSTOP 0x08000000
#endif
#endif

#define USER_ENTRY 0x00000100
#define USER_STACK_DEFAULT_SIZE 0x00001000
#define USER_STACK_MAX_SIZE 0x00010000
#define USER_STACK_GUARD 0x00000100
#define USER_STACK_LIMIT (USER_SEG_SIZE - USER_STACK_GUARD)
#define USER_STACK_BASE(stack_size) (USER_STACK_LIMIT - (stack_size))
#define USER_STACK_DEFAULT_BASE USER_STACK_BASE(USER_STACK_DEFAULT_SIZE)
#define USER_ARG_MAX 128

#endif
