/* Image header read by the kernel: bytes 4..8 are the memory hint and 8..12
   the stack hint, both big-endian. */
    .section .header, "a"
    .byte 0x00, 0x00, 0x00, 0x00          /* reserved */
    .byte 0x00, 0x10, 0x00, 0x00          /* 1 MiB of address space */
    .byte 0x00, 0x00, 0x40, 0x00          /* 16 KiB of stack */
