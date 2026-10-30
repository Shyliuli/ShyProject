/* long sys_call(long number, long a, long b, long c); */
    .section .text, "ax"
    .globl sys_call
sys_call:
    mv      a7, a0
    mv      a0, a1
    mv      a1, a2
    mv      a2, a3
    ecall
    ret
