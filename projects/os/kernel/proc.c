#![no_main]

#include <stdint.shyh>
#include "fs.h"
#include "lock.h"
#include "memlayout.h"
#include "proc.h"

#include <string.shyh>

void *kalloc_page(void);
void kfree_page(void *p);
unsigned int seg_alloc(void);
void seg_free(unsigned int pa);
void scheduler(void);

struct proc procs[NPROC];
int next_pid;
static Lock proc_lock;

static void write_u32(unsigned char *p, unsigned int off, unsigned int v) {
    p[off + 0] = (unsigned char)((v >> 24) & 0xff);
    p[off + 1] = (unsigned char)((v >> 16) & 0xff);
    p[off + 2] = (unsigned char)((v >> 8) & 0xff);
    p[off + 3] = (unsigned char)(v & 0xff);
}

static void write_inst(unsigned char *p, unsigned int off,
                       unsigned int op, unsigned int a1, unsigned int a2) {
    write_u32(p, off + 0, op);
    write_u32(p, off + 4, a1);
    write_u32(p, off + 8, a2);
}

static void copy_proc_name(char *dst, const char *src) {
    int i = 0;

    while (i < PROC_NAME_MAX - 1 && src[i]) {
        dst[i] = src[i];
        i = i + 1;
    }
    dst[i] = 0;
}

static const char *proc_state_name(enum procstate state) {
    if (state == UNUSED) {
        return "unused";
    }
    if (state == EMBRYO) {
        return "embryo";
    }
    if (state == SLEEPING) {
        return "sleep";
    }
    if (state == RUNNABLE) {
        return "runble";
    }
    if (state == RUNNING) {
        return "run";
    }
    if (state == ZOMBIE) {
        return "zombie";
    }
    return "?";
}

static char *map_user_buf(char *user_buf, int max) {
    unsigned int addr = (unsigned int)user_buf;

    if (max <= 0) {
        return (char *)0;
    }
    if (addr > USER_SEG_SIZE || (unsigned int)max > USER_SEG_SIZE ||
        addr + (unsigned int)max < addr || addr + (unsigned int)max > USER_SEG_SIZE) {
        return (char *)0;
    }
    return (char *)(current->segbase + addr);
}

static void clear_context(struct context *ctx) {
    for (int i = 0; i < 16; i++) {
        ctx->regs[i] = 0;
    }
    ctx->sp = 0;
    ctx->ksp = 0;
    ctx->epc = 0;
    ctx->segs = 0;
    ctx->sege = 0;
    ctx->valid = 0;
}

static void load_idle_code(struct proc *p) {
    unsigned char *mem = (unsigned char *)p->segbase;
    unsigned int off = 0x100;

    // setn 0x, SYS_yield; syscall; ujmpn syscall
    write_inst(mem, off + 0, 0x3E, 0x00, 2);
    write_inst(mem, off + 12, 0x54, 0, 0);
    write_inst(mem, off + 24, 0x4A, 0x10C, 0);
}

static struct proc *find_proc_locked(int pid) {
    for (int i = 0; i < NPROC; i++) {
        if (procs[i].state != UNUSED && procs[i].pid == pid) {
            return &procs[i];
        }
    }
    return 0;
}

static int wait_chan(struct proc *p) {
    return 0 - p->pid;
}

static int has_live_non_idle_locked(void) {
    for (int i = 0; i < NPROC; i++) {
        if (procs[i].state != UNUSED &&
            procs[i].state != ZOMBIE &&
            procs[i].pid != 0) {
            return 1;
        }
    }
    return 0;
}

static void copy_wait_status(struct proc *parent, unsigned int addr, int value) {
    if (addr != 0) {
        if (addr <= USER_SEG_SIZE - 4) {
            int *user_status = (int *)(parent->segbase + addr);
            *user_status = value;
        }
    }
}

static void free_proc_locked(struct proc *p);
static void wakeup_locked(int chan);
static void proc_exit_locked(int status);

static void wake_waiting_parent_locked(struct proc *parent, struct proc *child) {
    int pid = child->pid;

    copy_wait_status(parent, parent->wait_status, child->xstatus);
    free_proc_locked(child);
    parent->wait_status = 0;
    parent->waiting = 0;
    parent->chan = 0;
    parent->ctx.regs[1] = pid;
    parent->state = RUNNABLE;
}

void proc_init(void) {
    struct proc *idle;

    lock_init(&proc_lock);
    for (int i = 0; i < NPROC; i++) {
        clear_context(&procs[i].ctx);
        procs[i].state = UNUSED;
        procs[i].pid = 0;
        procs[i].parent = -1;
        procs[i].kstack = 0;
        procs[i].segbase = 0;
        procs[i].sz = 0;
        procs[i].heap_base = 0;
        procs[i].stack_base = USER_STACK_DEFAULT_BASE;
        procs[i].xstatus = 0;
        procs[i].chan = 0;
        procs[i].killed = 0;
        procs[i].waiting = 0;
        procs[i].wait_status = 0;
        procs[i].stdin_fd = -1;
        procs[i].stdout_fd = -1;
        procs[i].open_fds = 0;
        procs[i].name[0] = 0;
    }

    next_pid = 1;
    idle = alloc_proc();
    idle->pid = 0;
    idle->parent = -1;
    idle->state = RUNNABLE;
    next_pid = 1;
    load_idle_code(idle);
}

struct proc *alloc_proc(void) {
    struct proc *p = 0;

    lock_acquire(&proc_lock);
    for (int i = 0; i < NPROC; i++) {
        if (procs[i].state == UNUSED) {
            p = &procs[i];
            p->state = EMBRYO;
            p->pid = next_pid;
            next_pid = next_pid + 1;
            p->parent = -1;
    p->chan = 0;
    p->killed = 0;
            p->stdin_fd = -1;
            p->stdout_fd = -1;
            p->open_fds = 0;
            p->name[0] = 0;
    break;
        }
    }
    lock_release(&proc_lock);

    if (p == 0) {
        return 0;
    }

    p->kstack = (unsigned int)kalloc_page();
    p->segbase = seg_alloc();

    lock_acquire(&proc_lock);
    for (int r = 0; r < 16; r++) {
        p->ctx.regs[r] = 0;
    }
    p->ctx.ksp = p->kstack;
    p->ctx.sp = USER_STACK_DEFAULT_BASE;
    p->ctx.segs = p->segbase;
    p->ctx.sege = p->segbase + USER_SEG_SIZE;
    p->ctx.epc = USER_ENTRY;
    p->ctx.valid = 0;
    p->sz = 0;
    p->heap_base = 0;
    p->stack_base = USER_STACK_DEFAULT_BASE;
    p->xstatus = 0;
    p->chan = 0;
    p->killed = 0;
    p->waiting = 0;
    p->wait_status = 0;
    p->stdin_fd = -1;
    p->stdout_fd = -1;
    p->open_fds = 0;
    p->name[0] = 0;
    lock_release(&proc_lock);

    return p;
}

static void free_proc_locked(struct proc *p) {
    if (p == 0) {
        return;
    }

    if (p->kstack != 0) {
        kfree_page((void *)p->kstack);
    }
    if (p->segbase != 0) {
        seg_free(p->segbase);
    }

    clear_context(&p->ctx);
    p->state = UNUSED;
    p->pid = 0;
    p->parent = -1;
    p->kstack = 0;
    p->segbase = 0;
    p->sz = 0;
    p->heap_base = 0;
    p->stack_base = USER_STACK_DEFAULT_BASE;
    p->xstatus = 0;
    p->chan = 0;
    p->killed = 0;
    p->waiting = 0;
    p->wait_status = 0;
    p->stdin_fd = -1;
    p->stdout_fd = -1;
    p->open_fds = 0;
    p->name[0] = 0;
}

void free_proc(struct proc *p) {
    lock_acquire(&proc_lock);
    free_proc_locked(p);
    lock_release(&proc_lock);
}

int fork(void) {
    struct proc *child = alloc_proc();
    unsigned int *src;
    unsigned int *dst;
    unsigned int n;
    int child_pid;
    int stdin_fd = current->stdin_fd;
    int stdout_fd = current->stdout_fd;
    unsigned int open_fds = current->open_fds;

    if (child == 0) {
        lock_acquire(&proc_lock);
        current->ctx.regs[1] = (uint32_t)-1;
        lock_release(&proc_lock);
        return -1;
    }

    src = (unsigned int *)current->segbase;
    dst = (unsigned int *)child->segbase;
    n = current->sz / 4;
    if (current->sz % 4) {
        n = n + 1;
    }
    if (n < USER_SEG_SIZE / 4) {
        n = USER_SEG_SIZE / 4;
    }
    memcpy(dst, src, USER_SEG_SIZE);
    asm("fencei");

    lock_acquire(&proc_lock);
    child->ctx = current->ctx;
    child->ctx.segs = child->segbase;
    child->ctx.sege = child->segbase + USER_SEG_SIZE;
    child->ctx.regs[1] = 0;
    child->parent = current->pid;
    child->state = RUNNABLE;
    child->sz = current->sz;
    child->heap_base = current->heap_base;
    child->stack_base = current->stack_base;
    child->xstatus = 0;
    child->chan = 0;
    child->killed = 0;
    child->waiting = 0;
    child->wait_status = 0;
    child->stdin_fd = stdin_fd;
    child->stdout_fd = stdout_fd;
    child->open_fds = open_fds;
    copy_proc_name(child->name, current->name);
    file_dup_open_mask(open_fds);
    if (stdin_fd >= 3) {
        file_dup(stdin_fd);
    }
    if (stdout_fd >= 3) {
        file_dup(stdout_fd);
    }

    current->ctx.regs[1] = child->pid;
    child_pid = child->pid;
    lock_release(&proc_lock);
    return child_pid;
}

static void proc_exit_locked(int status) {
    int pid = current->pid;
    struct proc *parent;

    current->xstatus = status;
    current->chan = 0;
    current->waiting = 0;
    current->wait_status = 0;
    current->state = ZOMBIE;
    if (status == 2 && current->parent == 1) {
        asm("setn exit 0");
    }
    parent = find_proc_locked(current->parent);
    if (parent != 0 &&
        parent->state == SLEEPING &&
        parent->waiting) {
        wake_waiting_parent_locked(parent, current);
    } else if (parent != 0) {
        wakeup_locked(wait_chan(parent));
    }

    for (int i = 0; i < NPROC; i++) {
        if (procs[i].parent == pid) {
            if (procs[i].state == ZOMBIE) {
                free_proc_locked(&procs[i]);
            } else {
                procs[i].parent = 1;
            }
        }
    }

    if (pid != 0 && !has_live_non_idle_locked()) {
        asm("setn exit 0");
    }
    if (pid == 1) {
        asm("setn exit 0");
    }
}

void proc_exit(int status) {
    current->xstatus = status;
    if (current->stdin_fd >= 3 || current->stdout_fd >= 3 || current->open_fds != 0) {
        file_close_proc();
    }
    lock_acquire(&proc_lock);
    proc_exit_locked(current->xstatus);
    lock_release(&proc_lock);
}

#undef exit
void exit(int status) {
    proc_exit(status);
}

int wait(int *status) {
    while (1) {
        int have_child = 0;

        lock_acquire(&proc_lock);
        for (int i = 0; i < NPROC; i++) {
            if (procs[i].parent == current->pid) {
                have_child = 1;
                if (procs[i].state == ZOMBIE) {
                    int pid = procs[i].pid;
                    copy_wait_status(current, (unsigned int)status, procs[i].xstatus);
                    current->waiting = 0;
                    current->wait_status = 0;
                    free_proc_locked(&procs[i]);
                    lock_release(&proc_lock);
                    return pid;
                }
            }
        }

        if (!have_child) {
            lock_release(&proc_lock);
            return -1;
        }
        if (current->killed) {
            lock_release(&proc_lock);
            return -1;
        }

        current->waiting = 1;
        current->wait_status = (unsigned int)status;
        current->chan = wait_chan(current);
        current->state = SLEEPING;
        lock_release(&proc_lock);
        return 0;
    }
}

int getpid(void) {
    int pid;

    lock_acquire(&proc_lock);
    pid = current->pid;
    lock_release(&proc_lock);
    return pid;
}

int kill(int pid) {
    struct proc *p;

    lock_acquire(&proc_lock);
    p = find_proc_locked(pid);
    if (p == 0 || p->pid == 0) {
        lock_release(&proc_lock);
        return -1;
    }

    p->killed = 1;
    if (p->state == SLEEPING) {
        p->chan = 0;
        p->state = RUNNABLE;
    }
    lock_release(&proc_lock);
    return 0;
}

int sleep(int chan) {
    if (chan == 0) {
        return -1;
    }
    if (current == 0 || current->pid == 0) {
        return -1;
    }

    lock_acquire(&proc_lock);
    current->chan = chan;
    current->state = SLEEPING;
    lock_release(&proc_lock);
    return 0;
}

static void wakeup_locked(int chan) {
    if (chan == 0) {
        return;
    }

    for (int i = 0; i < NPROC; i++) {
        if (procs[i].state == SLEEPING && procs[i].chan == chan) {
            procs[i].chan = 0;
            procs[i].state = RUNNABLE;
        }
    }
}

void wakeup(int chan) {
    lock_acquire(&proc_lock);
    wakeup_locked(chan);
    lock_release(&proc_lock);
}

void yield(void) {
    lock_acquire(&proc_lock);
    if (current->state == RUNNING) {
        current->state = RUNNABLE;
    }
    lock_release(&proc_lock);
    scheduler();
}

int proc_sbrk(int n) {
    unsigned int old;
    unsigned int new_break;
    unsigned int stack_base;

    lock_acquire(&proc_lock);
    if (current == 0 || current->pid == 0) {
        lock_release(&proc_lock);
        return -1;
    }

    old = current->sz;
    new_break = old;
    stack_base = current->stack_base;
    if (stack_base == 0 || stack_base > USER_SEG_SIZE) {
        stack_base = USER_STACK_DEFAULT_BASE;
    }

    if (n >= 0) {
        unsigned int inc = (unsigned int)n;

        new_break = old + inc;
        if (new_break < old) {
            lock_release(&proc_lock);
            return -1;
        }
    } else {
        unsigned int dec = 0 - (unsigned int)n;

        if (dec > old || old - dec < current->heap_base) {
            lock_release(&proc_lock);
            return -1;
        }
        new_break = old - dec;
    }

    if (new_break > USER_SEG_SIZE || new_break >= stack_base) {
        lock_release(&proc_lock);
        return -1;
    }

    current->sz = new_break;
    lock_release(&proc_lock);
    return (int)old;
}

struct proc *proc_next(void) {
    int start = 0;
    struct proc *next;

    lock_acquire(&proc_lock);
    if (current != 0) {
        for (int i = 0; i < NPROC; i++) {
            if (&procs[i] == current) {
                start = i + 1;
                break;
            }
        }
    }

    for (int n = 0; n < NPROC; n++) {
        int i = (start + n) % NPROC;
        if (procs[i].state == RUNNABLE && procs[i].pid != 0) {
            if (procs[i].killed) {
                current = &procs[i];
                proc_exit_locked(-1);
                continue;
            }
            next = &procs[i];
            lock_release(&proc_lock);
            return next;
        }
    }

    if (current != 0 && current->state == RUNNABLE && current->pid != 0) {
        next = current;
        lock_release(&proc_lock);
        return next;
    }

    next = &procs[0];
    lock_release(&proc_lock);
    return next;
}

void proc_set_current_state(enum procstate state) {
    lock_acquire(&proc_lock);
    if (current != 0) {
        current->state = state;
    }
    lock_release(&proc_lock);
}

int proc_current_state_is(enum procstate state) {
    int same = 0;

    lock_acquire(&proc_lock);
    if (current != 0 && current->state == state) {
        same = 1;
    }
    lock_release(&proc_lock);
    return same;
}

int proc_current_killed_runnable(void) {
    int killed = 0;

    lock_acquire(&proc_lock);
    if (current != 0 &&
        current->pid != 0 &&
        current->killed &&
        current->state == RUNNABLE) {
        killed = 1;
    }
    lock_release(&proc_lock);
    return killed;
}

int proc_live_non_idle_exists(void) {
    int live;

    lock_acquire(&proc_lock);
    live = has_live_non_idle_locked();
    lock_release(&proc_lock);
    return live;
}

void proc_set_current_size(unsigned int sz) {
    lock_acquire(&proc_lock);
    if (current != 0) {
        current->sz = sz;
        current->heap_base = sz;
    }
    lock_release(&proc_lock);
}

void proc_set_current_stack_base(unsigned int stack_base) {
    lock_acquire(&proc_lock);
    if (current != 0) {
        current->stack_base = stack_base;
    }
    lock_release(&proc_lock);
}

void proc_set_current_name(const char *name) {
    lock_acquire(&proc_lock);
    if (current != 0) {
        copy_proc_name(current->name, name);
    }
    lock_release(&proc_lock);
}

int proc_psline(int index, char *user_buf, int max) {
    char *dst;
    int seen = 0;
    int pos = 0;

    if (index < 0) {
        return 0;
    }
    dst = map_user_buf(user_buf, max);
    if (dst == 0) {
        return -1;
    }

    lock_acquire(&proc_lock);
    for (int i = 0; i < NPROC; i++) {
        if (procs[i].state != UNUSED) {
            if (seen == index) {
                int pid = procs[i].pid;
                int divisor = 1;
                const char *state_name = proc_state_name(procs[i].state);
                const char *name = procs[i].name;
                int j = 0;

                if (pid == 0) {
                    if (pos < max - 1) {
                        dst[pos] = '0';
                        pos = pos + 1;
                    }
                } else {
                    if (pid < 0) {
                        if (pos < max - 1) {
                            dst[pos] = '-';
                            pos = pos + 1;
                        }
                        pid = 0 - pid;
                    }
                    while (pid / divisor >= 10) {
                        divisor = divisor * 10;
                    }
                    while (divisor > 0) {
                        if (pos < max - 1) {
                            dst[pos] = (char)('0' + ((pid / divisor) % 10));
                            pos = pos + 1;
                        }
                        divisor = divisor / 10;
                    }
                }
                if (pos < max - 1) {
                    dst[pos] = ' ';
                    pos = pos + 1;
                }
                while (state_name[j]) {
                    if (pos < max - 1) {
                        dst[pos] = state_name[j];
                        pos = pos + 1;
                    }
                    j = j + 1;
                }
                if (pos < max - 1) {
                    dst[pos] = ' ';
                    pos = pos + 1;
                }
                if (!name[0]) {
                    name = "?";
                }
                j = 0;
                while (name[j]) {
                    if (pos < max - 1) {
                        dst[pos] = name[j];
                        pos = pos + 1;
                    }
                    j = j + 1;
                }
                if (pos < max - 1) {
                    dst[pos] = '\n';
                    pos = pos + 1;
                }
                dst[pos] = 0;
                lock_release(&proc_lock);
                return 1;
            }
            seen = seen + 1;
        }
    }
    lock_release(&proc_lock);
    return 0;
}
