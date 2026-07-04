#![no_main]

void uart_putc(int c) {
    asm!(c) {
        "oututfa {c}"
    };
}

int uart_getc(void) {
    int stat = 0;
    int data = 0;

    asm!(stat, data) {
        "seta {stat} 0x71\n"
        "seta {data} 0x70"
    };

    if ((stat & 1) == 0) {
        return -1;
    }
    return data;
}

void panic(char *msg) {
    char *prefix = "panic: ";
    char *null_msg = "(null)";

    for (int i = 0; prefix[i]; i++) {
        uart_putc(prefix[i]);
    }

    if (!msg) {
        msg = null_msg;
    }

    for (int i = 0; msg[i]; i++) {
        uart_putc(msg[i]);
    }
    uart_putc('\n');

    asm("setn exit 0");
    for (;;) {
    }
}
