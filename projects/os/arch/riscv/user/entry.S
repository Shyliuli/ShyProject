/* The kernel enters user images at fixed VA 0x100. C function order is not
   guaranteed to put `_start` there, so this trampoline jumps to its symbol. */
    .section .text.start, "ax"
    .globl _user_entry
_user_entry:
    la      t0, _start
    jr      t0
