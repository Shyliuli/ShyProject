# GDB Remote Debugging

The emulator can expose a GDB Remote Serial Protocol server for ShyISA state
inspection, memory access, breakpoints, continue, and single-step.

Start the emulator and wait for GDB before executing guest code:

```sh
cargo run -- path/to/image.sfs --gdb --gdb-wait
```

Use a custom port:

```sh
cargo run -- path/to/image.sfs --gdb 2345 --gdb-wait
```

Without `--gdb-wait`, the emulator starts running immediately and accepts a GDB
connection when one arrives:

```sh
cargo run -- path/to/image.sfs --gdb
```

If a matching `path/to/image.debug.elf` exists, the GDB server advertises it via
`qXfer:exec-file:read`, so GDB can load source line debug info automatically
after `target remote`.  You can also pass it explicitly:

```sh
cargo run -- path/to/image.sfs --gdb --gdb-wait --debug-info path/to/image.debug.elf
```

Build a C/ShyC program with full DWARF debug info (source lines, locals,
parameters, types):

```sh
target/bin/shycc --shy-emit-source-lines --shy-emit-debug-info \
  test/testc/main.c test/testc/put.c -o /tmp/testc.sfs --sym /tmp/testc.sym
```

This writes `/tmp/testc.debug.elf`.  With `gdb-multiarch`, source commands such
as `break main`, `continue`, and `list` should work without `add-symbol-file`,
and commands like `info locals`, `info args`, `ptype`, and `print <var>` show
variables, parameters, struct members, arrays, and pointers.

### Backtraces

When the debug ELF contains `.debug_frame`, GDB can unwind nested ShyC calls
with `bt`. The CFI describes ShyISA's upward-growing stack and the standard
`fx` (`r15`) frame pointer prologue. For GDB's MIPS compatibility target, the
CFA is a synthetic negated frame address used only to keep frame IDs ordered;
saved registers are recovered from real guest addresses such as `fx - 8`
(caller PC) and `fx - 4` (caller `fx`). A breakpoint in a nested function shows
the caller chain:

```gdb
(gdb) break inner
(gdb) continue
(gdb) bt
#0  inner (x=5) at /tmp/bt_test.c:2
#1  0x0000037c in middle (x=5) at /tmp/bt_test.c:6
#2  0x00000280 in outer (x=5) at /tmp/bt_test.c:10
#3  0x00000184 in main () at /tmp/bt_test.c:14
```

GDB may still warn if it tries to unwind past `main` into `_start`, because the
startup code is not emitted as a C subprogram with frame metadata.

### Inspecting variables and types

When the debug ELF is loaded, GDB can inspect local variables, parameters, and
aggregate values.  For example, with a function:

```c
struct Point { int x; int y; };

int add(int a, int b) {
    struct Point p;
    p.x = a;
    p.y = b;
    int arr[4];
    int *ptr = &p.x;
    return *ptr + arr[0] + arr[1];
}
```

GDB will display:

```gdb
(gdb) info args
a = 3
b = 5

(gdb) info locals
p = {x = 0, y = 0}
arr = {0, 0, 0, 0}
ptr = 0x0

(gdb) ptype p
type = struct p {
    int x;
    int y;
}
```

Values at the start of a function are uninitialized (or zero) until the
corresponding assignment statements execute.

### Watchpoints

The GDB server supports data watchpoints through the standard RSP packets:
`Z2/z2` for writes, `Z3/z3` for reads, and `Z4/z4` for read-or-write access.
GDB emits these packets for commands such as `watch`, `rwatch`, and `awatch`.

For example, after stopping inside a function with debug info:

```gdb
(gdb) watch p.x
(gdb) continue
```

Execution stops before the guest instruction that writes the watched address,
and the stop reply includes a `watch:<addr>` field.  Ordinary breakpoints,
`continue`, `stepi`, memory inspection, and local variable inspection continue
to work with watchpoints installed.

### Register view

Because GDB requires a known architecture for remote targets, the RSP server
uses the **MIPS 32-bit big-endian** register layout as a compatibility shell.
ShyISA registers are mapped to fixed MIPS slots, so `info registers` shows the
correct values but with MIPS names:

| GDB / MIPS name | ShyISA register |
|-----------------|-----------------|
| `r0` .. `r15`   | `r0` .. `r15`   |
| `sp` (`r29`)    | `sp`            |
| `sr` (`r32`)    | `status`        |
| `cause` (`r36`) | `cause`         |
| `pc` (`r37`)    | `pc`            |

Other ShyISA registers (`rs`, `tm`, `epc`, `ksp`, `segs`, `sege`, `trap`,
`music0..3`) are not exposed in the GDB register view, but can still be read
and written through memory commands (`x`/`set`) and the `m`/`M` RSP packets.

Connect with `gdb-multiarch` (or a GDB that supports MIPS):

```gdb
set endian big
target remote 127.0.0.1:1234
info registers
x/8wx 0x100
break *0x100
continue
stepi
detach
```

VS Code 通过 Microsoft C/C++ 扩展连接时，需要在 `launch.json` 中指定
`set endian big`，并且仍然使用 MIPS 寄存器名：

```json
"setupCommands": [
  { "text": "set architecture mips" },
  { "text": "set endian big" }
]
```

当 `.debug.elf` 与 `.sfs` 同名同目录时，不需要在 VS Code 中额外执行
`add-symbol-file`。
