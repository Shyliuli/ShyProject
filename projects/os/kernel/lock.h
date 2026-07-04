#ifndef SHYOS_KERNEL_LOCK_H
#define SHYOS_KERNEL_LOCK_H

struct Lock {
    int locked;
};

void lock_init(Lock *lock);
void lock_acquire(Lock *lock);
void lock_release(Lock *lock);

int p3_counter_op(int op);

#endif
