# ShyOS

ShyOS is the teaching operating system for ShyISA. It demonstrates privilege
separation, syscall-based I/O, and preemptive multitasking on top of the ShyISA
trap model.

## M1: Single User Program

M1 runs one user program in user mode. The user image is linked as a normal
`.sfs` starting at virtual address `0x100`, embedded into the kernel image, and
copied into the user segment at boot.

Syscall ABI:

```text
0x = syscall number
1x = arg0
2x = arg1
3x = arg2
syscall
0x = return value
```

Syscalls:

```text
0 exit(code)
1 write(fd, user_ptr, len)
2 read(fd, user_ptr, len)
3 yield()
4 getpid()
```

Only `fd=1` is accepted for `write`, and only `fd=0` is accepted for `read`.
The kernel validates user virtual pointers before translating them with
`paddr = current->segs + vaddr`.

## M2: Round-Robin Processes

M2 uses four fixed process slots. The demo programs `proc0`, `proc1`, and
`proc2` are kept as scheduler test cases, but the default M3 boot path starts
only the shell.

```text
KERNEL_STACK_TOP = 0x00100000
USER_ENTRY_VA    = 0x00000100
USER_STACK_TOP   = 0x001ff000
TIMER_SLICE      = 20          # 200 ms in the current emulator

proc0: SEGS=0x00400000, SEGE=0x00600000
proc1: SEGS=0x00600000, SEGE=0x00800000
proc2: SEGS=0x00800000, SEGE=0x00A00000
proc3: SEGS=0x00A00000, SEGE=0x00C00000
```

The kernel stack is shared. Trap handling is atomic for M2, so each process only
needs a saved user context and a user stack pointer.

## Trap Frame

`trap_entry.shy` saves the current user context into the global `trap_frame`,
calls `shyos_trap_dispatch`, then restores `trap_frame` and executes `iret`.

Layout:

```text
tf[0..15] = 0x..fx
tf[16]    = EPC
tf[17]    = CAUSE
tf[18]    = user_sp, saved from KSP after user-to-kernel trap stack swap
tf[19]    = RS
```

The global trap frame is the context-switch hub. Each PCB contains a copy of
this frame.

## PCB

Each fixed process slot stores:

```text
pid
state: FREE, READY, RUNNING, WAITING
segs
sege
user_sp
wait_pid
name
fd table
saved TrapFrame
```

`current` points at the currently running PCB.

## Scheduling

Round-robin scheduling runs when:

```text
CAUSE=2 timer interrupt
syscall 3 yield()
syscall 0 exit()
fault causes 3/4/5
```

For timer and yield, the dispatcher copies the global `trap_frame` into the
current PCB, marks it READY, selects the next READY process, copies that PCB's
saved frame back into the global `trap_frame`, updates `SEGS/SEGE/EPC/KSP`, and
reloads `TM=20`.

For exit and fault, the current process is marked FREE before scheduling, so its
context is not saved or resumed. If no process remains, the kernel prints a
completion message and writes `exit=0`.

The first process starts by setting `current=proc0`, configuring
`SEGS/SEGE/KSP/TM`, and executing:

```asm
enteruser 0x00000100
```

## M3: Dynamic Processes And Shell

M3 changes boot from fixed demo processes to an interactive user shell. The
kernel creates `proc0` as `shell`, enters it with `enteruser`, and leaves the
remaining PCB slots free. The shell runs in user mode and can only interact with
the machine through syscalls.

Additional syscalls:

```text
5 exec(name_ptr, name_len) -> pid or -1
6 ps(buf_ptr, buf_len)     -> bytes written or -1
7 kill(pid)                -> 0 or -1
8 waitpid(pid)             -> 0 or -1
```

`exec` is a simplified process creation model: there is no `fork`. The kernel
copies the program name out of the caller's user segment, looks it up in the
built-in image table, finds a free PCB slot, loads the image into that slot's
physical user segment, initializes its trap frame with `PC=0x100` and
`user_sp=0x001ff000`, and marks it READY.

The built-in program table currently contains:

```text
shell   interactive command loop
hello   short program that prints its pid and exits
ticker  loop that prints its pid/tick and yields
```

`ps` formats process state in the kernel and writes the bytes into a validated
user buffer. `kill` marks the target PCB FREE and wakes any waiters. Killing the
currently running process immediately schedules another READY process. If no
runnable process remains, the kernel prints `ShyOS M3: all processes exited`
and exits the emulator.

`waitpid` is blocking. If the target process is still alive, the dispatcher
writes return value `0` into `tf[0]`, copies the current trap frame into the
PCB, changes the caller to WAITING, records `wait_pid`, and schedules another
process. `exit` and `kill` call `wake_waiters(pid)`, which moves matching
WAITING processes back to READY with saved `0x=0`, so the restored user context
returns from `waitpid` successfully.

Shell commands:

```text
help
ps
run <name>
kill <pid>
wait <pid>
```

## M4: Ramdisk Files

M4 replaces the M3 built-in program image table with a read-only ramdisk. The
build first links each user program as an `.sfs`, then `embed_ramdisk.py`
embeds those bytes plus text files into one kernel data file. Each ramdisk entry
has:

```text
name
byte data
length
executable flag
```

The default ramdisk contains:

```text
shell
hello
ticker
readme.txt
hello.txt
.
```

`.` is a virtual directory file generated by the embed tool. Its contents are
the visible ramdisk file names, one per line, so the shell can implement `ls`
with the same `open/read/close` path as `cat`.

Additional file syscalls:

```text
9  open(name_ptr, name_len) -> fd or -1
10 close(fd)                -> 0 or -1
```

`read` keeps its M1 stdin behavior for `fd=0`. For `fd>=3`, it reads from the
current process's ramdisk fd table entry, advances that entry's offset, and
returns `0` at EOF. `write` accepts `fd=1` and `fd=2` for UART output; ramdisk
files are read-only.

Each PCB owns `NFILE=4` fd slots:

```text
0 stdin   reserved
1 stdout  reserved
2 stderr  reserved
3 ramdisk file, allocated by open
```

An fd table entry stores the ramdisk file index and current offset. Process
creation initializes the table, and `exit`/`kill` close fd 3 automatically.

`exec(name)` now resolves `name` through the ramdisk and only accepts executable
entries. It copies the selected `.sfs` bytes into the target process's user
segment, initializes the PCB context as before, and executes `fencei` after
loading so a reused process slot cannot execute stale cached instructions.

M4 shell commands:

```text
help
ls
cat <name>
ps
run <name>
kill <pid>
wait <pid>
```

## Fork/Exec Shell And Proc Methods

After M4, ShyOS changes from `exec`-creates-process to the classic
`fork + exec + wait` shell model.

Current syscall table:

```text
0  exit(code)
1  write(fd, user_ptr, len)
2  read(fd, user_ptr, len)
3  yield()
4  getpid()
5  exec(name_ptr, name_len)
6  ps(buf_ptr, buf_len)
7  kill(pid)
8  waitpid(pid)
9  open(name_ptr, name_len)
10 close(fd)
11 fork()
```

`fork()` copies the current process into a free PCB slot. It byte-copies the
full 2MiB user segment, copies the trap frame and fd table, sets the child
return value `0x=0`, and sets the parent return value to the child pid. The
child is READY and resumes at the same syscall return point as the parent.

`exec(name)` now replaces the current process. On success it loads the named
executable ramdisk file into the current process's existing user segment,
resets registers, sets `EPC=0x100`, resets the user stack to `0x001ff000`,
closes fd slots 3 and above, executes `fencei`, and returns to user mode at the
new program entry. A successful exec does not return to the old user code.
Failure returns `-1`.

The shell `run <name>` command is foreground:

```text
pid = fork()
child: exec(name), exit(1) if exec fails
parent: waitpid(pid), then print the next prompt
```

Process management code is organized with ShyC `impl` methods. The shared
kernel declarations live in `os/kernel/types.shyh`, which defines the kernel
types, global instances, and cross-file impl method prototypes.

`Proc` owns single-process operations such as `fork`, `exec`, `exit`, `kill`,
`waitpid`, `open`, `close`, ramdisk fd reads, and stdout/stderr writes.
`ProcTable` owns allocation, lookup, startup, scheduling, waiter wakeup, and
`ps` formatting. `RamDisk` owns ramdisk lookup/read/program loading.
`Console` owns UART output and emulator exit. `Cpu` owns privileged register
writes and `fencei`.

The syscall dispatcher remains a plain trap entry target named
`shyos_trap_dispatch`, but it calls object methods directly:

```text
current.exec(...)
current.fork(...)
current.read(...)
current.write(...)
ptable.schedule(...)
ptable.format_ps(...)
ramdisk.lookup(...)
console.puts(...)
```

The user shell is also organized around `impl Shell`; `Shell.run()` drives the
prompt loop and dispatches `ls`, `cat`, `run`, `ps`, `kill`, and `wait`.

The scheduler executes `fencei` when switching to a process with a different
`SEGS`, because the emulator instruction cache is indexed by virtual PC. Exec
also executes `fencei` after replacing a process image.
