#ifndef SHYOS_KERNEL_PROC_H
#define SHYOS_KERNEL_PROC_H

#include "trap.h"

#define NPROC 16
#define PROC_NAME_MAX 16

enum procstate { UNUSED, EMBRYO, SLEEPING, RUNNABLE, RUNNING, ZOMBIE };

struct proc {
    struct context ctx;
    enum procstate state;
    int pid;
    int parent;
    unsigned int kstack;
    unsigned int segbase;
    unsigned int sz;
    unsigned int heap_base;
    unsigned int stack_base;
    int xstatus;
    int chan;
    int killed;
    int waiting;
    unsigned int wait_status;
    int stdin_fd;
    int stdout_fd;
    unsigned int open_fds;
    char name[PROC_NAME_MAX];
};

extern struct proc procs[NPROC];
extern struct proc *current;
extern int next_pid;

void proc_init(void);
struct proc *alloc_proc(void);
void free_proc(struct proc *p);
int fork(void);
void proc_exit(int status);
void exit(int status);
#define exit proc_exit
int wait(int *status);
int getpid(void);
int kill(int pid);
int sleep(int chan);
void wakeup(int chan);
void yield(void);
int proc_sbrk(int n);
struct proc *proc_next(void);
void proc_set_current_state(enum procstate state);
int proc_current_state_is(enum procstate state);
int proc_current_killed_runnable(void);
int proc_live_non_idle_exists(void);
void proc_set_current_size(unsigned int sz);
void proc_set_current_stack_base(unsigned int stack_base);
void proc_set_current_name(const char *name);
int proc_psline(int index, char *user_buf, int max);

#endif
