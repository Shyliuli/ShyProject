# ShyISA 用户态 ABI

本文描述 ShyISA 上用户态程序看到的约定：函数调用、进程初始栈、程序入口状态
和系统调用。它对应 `shyemu-user` 与 ShyC 用户态程序所实现的行为。

内存布局本身见 [MemoryMap.md](MemoryMap.md)，指令集见 [ShyISA.md](ShyISA.md)。
ShyOS 的内核分发使用它自己的一套调用号（见 `projects/os/kernel/trap.shyc`），
不在本文范围内。

## 调用约定

与 [ShyC.md](ShyC.md) 的 ABI v1 一致：

- 标量参数依次占用 `4x 5x 6x 7x 8x 9x ax bx`。32 位及以下占一个槽；64 位值
  低字在前占两个槽。放不下的标量整体走栈，不会拆到寄存器和栈两边。
- 结构体和联合体总是按值拷贝到栈上，不占参数寄存器。
- 结构体返回值使用调用者提供的存储，隐藏指针放在第一个参数槽 `4x`。
- 标量返回值放在 `1x`（32 位及以下），64 位值低 32 位在 `1x`、高 32 位在
  `2x`。
- `fx` 是帧指针，属于被调用者保存；`0x` 到 `ex` 和比较结果 `rs` 由调用者
  保存。
- 栈向上生长，`sp` 指向下一次 push 要写入的位置，保持 4 字节对齐。调用前
  `sp` 为 `B` 时，第一个栈参数位于 `B - S1`，第二个位于 `B - S2`，依此类推。
- 内存按大端序访问，32 位普通内存访问要求 4 字节对齐。

## 进程初始栈

启动时 `shyemu-user` 把参数和环境变量放在 `sp` 下方，从低地址到高地址依次是：

```text
低地址
  参数字符串        argv[0] 起，每个以 NUL 结尾
  argc              一个 32 位字
  argv 指针数组     argv[0] .. argv[argc-1]
  NULL
  envp 指针数组     envp[0] .. envp[envc-1]
  NULL
  (对齐填充)
  sp = 0x00FFF000   空闲栈底，第一次 push 写这里
高地址
```

- 数据块整体 16 字节对齐，各指针数组 4 字节对齐。
- `argv[0]` 是程序路径，其后是命令行参数；`envp` 是宿主的环境变量，每项形如
  `NAME=value`。
- `sp` 以上到实现内存末尾是程序可用的栈空间。栈向上生长，所以初始数据块放在
  `sp` 下方，不会被栈覆盖。
- 堆由 `brk` 从镜像末尾向上分配，最多长到初始数据块的最低地址。

## 程序入口

链接器把 `text._start` 放在 `0x00000100`。进入 `_start` 时：

| 状态 | 值 |
|---|---|
| `pc` | `0x00000100` |
| `status` | 用户态，中断关闭 |
| `segs` / `sege` | `0` / 实现内存末尾，用户地址与物理地址一一对应 |
| `sp` | `0x00FFF000` |
| `4x` | `argc` |
| `5x` | `argv`（指针数组地址） |
| `6x` | `envp`（指针数组地址） |
| 其他通用寄存器、`rs` | `0` |

`4x`/`5x` 正好是前两个参数寄存器，所以 `_start` 可以直接 `calln main`：
`main` 按 ABI 收到的就是 `argc` 和 `argv`。ShyC 的 `Args` 把这两个值看作一个
`{ int argc; char **argv; }` 结构体，见 [ShyC.md](ShyC.md) 的 `#[args_builtin]`。

## 系统调用

`syscall` 指令在用户态进程模式下由模拟器直接执行宿主系统调用，不进入 trap：

| 寄存器 | 含义 |
|---|---|
| `0x` | 系统调用号 |
| `1x` … `6x` | 第 0 … 5 个参数 |
| `1x` | 返回值 |

成功时 `1x` 是结果；失败时是负的 errno（32 位二补码），和 Linux 相同。指令
执行后 `pc` 前进 12 字节。用户程序写 `exit` 寄存器等价于结束进程，退出码取写入值。

调用号使用 Linux asm-generic 编号，riscv64 用的是同一套：

| 号码 | 名称 | 参数 | 返回值 |
|---:|---|---|---|
| 17 | `getcwd` | `buf`, `size` | 含 NUL 的路径长度 |
| 56 | `openat` | `dirfd`, `path`, `flags`, `mode` | 新文件描述符 |
| 57 | `close` | `fd` | `0` |
| 62 | `lseek` | `fd`, `offset`, `whence` | 新偏移 |
| 63 | `read` | `fd`, `buf`, `count` | 读到的字节数 |
| 64 | `write` | `fd`, `buf`, `count` | 写出的字节数 |
| 93 | `exit` | `status` | 不返回 |
| 94 | `exit_group` | `status` | 不返回 |
| 214 | `brk` | `addr` | 新的堆顶 |

实现约定：

- 0、1、2 号描述符是宿主的 stdin/stdout/stderr；`openat` 返回 3 号起最小的空闲
  描述符，`close` 只关闭这些打开的文件。
- `openat` 只接受 `AT_FDCWD`（-100）作为 `dirfd`。`flags` 使用 Linux 取值：
  `O_RDONLY`=0、`O_WRONLY`=1、`O_RDWR`=2、`O_CREAT`=0x40、`O_TRUNC`=0x200、
  `O_APPEND`=0x400。
- `lseek` 的 `offset` 是有符号 32 位，`whence` 取 0（开头）、1（当前位置）、2（末尾）。
  0 到 2 号描述符不支持定位，返回 `-ESPIPE`。
- `brk(0)` 返回当前堆顶；新的堆顶超出范围时原样返回当前值，调用者据此判断失败。
- 未实现的号码返回 `-ENOSYS`，其余错误码沿用宿主。
