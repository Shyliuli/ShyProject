#![no_main]

#include "lock.h"

static Lock p3_counter_lock;
static int p3_counter;

impl Lock {
    void init(self *lock) {
        lock.locked = 0;
    }

    void acquire(self *lock) {
        int old;
        int *addr = &lock.locked;

        while (1) {
            old = 1;
            asm!(addr, old) {
                "atoma {addr} {old}"
            };
            if (old == 0) {
                return;
            }
        }
    }

    void release(self *lock) {
        lock.locked = 0;
    }
}

void lock_init(Lock *lock) {
    lock.init();
}

void lock_acquire(Lock *lock) {
    lock.acquire();
}

void lock_release(Lock *lock) {
    lock.release();
}

int p3_counter_op(int op) {
    int value;

    p3_counter_lock.acquire();
    if (op == 0) {
        p3_counter = 0;
    } else if (op == 1) {
        p3_counter = p3_counter + 1;
    }
    value = p3_counter;
    p3_counter_lock.release();
    return value;
}
