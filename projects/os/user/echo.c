#![no_main]
#![mem(1M)]
#![stack(4K)]

#include "usys.h"
#include "ulib.h"

int main(void) {
    char *arg = user_arg();

    if (arg[0]) {
        puts(arg);
    }
    puts("\n");
    exit(0);
    return 0;
}

void _start(void) {
    main();
    exit(0);
}
