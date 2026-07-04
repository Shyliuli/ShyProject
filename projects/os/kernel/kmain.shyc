#![no_main]

#include <stdint.shyh>
#include "fs.h"
#include "proc.h"
#include "memlayout.h"

#include <string.shyh>

void uart_putc(int c);
void mem_init(void);
void scheduler(void);

extern struct proc *current;

static void load_init(struct proc *p) {
    struct fs_file *file = fs_lookup("init");
    unsigned char *dst = (unsigned char *)p->segbase;

    if (!user_image_layout_ok(file)) {
        uart_putc('!');
        uart_putc('\n');
        asm("setn exit 0");
    }

    memcpy(dst, file->data, file->size);

    p->ctx.epc = USER_ENTRY;
    p->ctx.sp = user_image_stack_base(file);
    p->sz = file->size;
    p->heap_base = file->size;
    p->stack_base = p->ctx.sp;
    p->name[0] = 'i';
    p->name[1] = 'n';
    p->name[2] = 'i';
    p->name[3] = 't';
    p->name[4] = 0;
}

void kmain(void) {
    struct proc *p;

    mem_init();
    proc_init();

    uart_putc('S');
    uart_putc('h');
    uart_putc('y');
    uart_putc('O');
    uart_putc('S');
    uart_putc(' ');
    uart_putc('b');
    uart_putc('o');
    uart_putc('o');
    uart_putc('t');
    uart_putc('\n');

    // Install the trap handler.
    asm("setn trap trap_entry");
    asm("setn tm 100");

    p = alloc_proc();
    load_init(p);
    p->state = RUNNABLE;

    current = &procs[0];

    // Never returns.
    scheduler();
}
