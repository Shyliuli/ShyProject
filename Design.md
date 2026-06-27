# ShyProject 概要设计

ShyProject 是一个面向教学的完整计算机系统实验项目。项目目标不是只定义一套指令集，而是围绕 ShyISA 建出一条从汇编、模拟执行、编译器后端、操作系统到基础运行时库的完整链路。

项目的长期形态包括：

- ShyISA：稳定的指令集、内存模型、Trap 模型和二进制格式规范。
- asm：将 ShyISA 汇编源码编译为 `.sobj` 可链接 object 文件。
- linker：将一个或多个 `.sobj` 链接为 `.sfs` raw 内存镜像。
- emu：执行 `.sfs` 镜像的 ShyISA 模拟器。
- LLVM 后端：让高级语言能够生成 ShyISA 机器码。
- OS：运行在 ShyISA 上的最小操作系统。
- libc：面向 C 程序的基础运行时库。
- librust：面向 Rust 程序的基础运行时支持。

## 设计目标

ShyProject 优先服务教学和可理解性，因此设计目标为简单，清晰，最小化.

## 组件划分

### ShyISA 规范

ShyISA 是项目的底层契约，定义在 `ShyISA.md` 中。它需要覆盖：

- 寄存器、特殊寄存器和地址映射。
- 指令编码、指令语义和大小端规则。
- `.sobj` section/object 格式与 `.sfs` raw 内存镜像格式。
- Trap、权限、内核态/用户态切换规则。
- I/O 指令与内存映射 I/O。
- 汇编源文件格式和数据布局。

规范的目标是让 asm、emu、OS 和未来 LLVM 后端都能按同一份文档实现，而不是从彼此代码里反推行为。

### asm

asm 是 ShyISA 汇编器，输入 `.asm` 文本，输出 `.sobj` 编译中间对象。

主要职责：

- 读取汇编源码。
- 解析 `___DEFINE___`、`___DATA___`、`___CODE___` 三个段。
- 处理宏定义、寄存器名、常量、section、symbol 和局部 label。
- 将数据和代码写入对应 section。
- 将代码段编码为固定 12 字节指令。
- 为 symbol、section、局部 label 等最终地址未知的位置生成 relocation。
- 输出可链接 `.sobj` object 文件。

### emu

emu 是 ShyISA 模拟器，输入 `.sfs` 镜像并执行。

主要职责：

- 加载 raw 内存镜像。
- 初始化寄存器、PC、STATUS 等 CPU 状态。
- 按 12 字节固定长度取指、译码、执行。
- 实现普通内存、寄存器映射区、I/O 映射区和权限检查。
- 实现 Trap、`syscall`、`iret`、定时器和用户态/内核态切换。
- 提供调试输出，例如寄存器 dump、单步执行、非法地址定位。

emu 是整个项目最重要的验证工具。asm 的输出、OS 的行为、LLVM 后端生成的程序，最终都要在 emu 上跑通。

### linker

linker 输入一个或多个 `.sobj` object 文件，按 `ObjFormat.md` 中定义的 section 规则分配最终地址、解析 symbol、处理 relocation，并输出 `.sfs` raw 内存镜像。

`.sfs` 是 ShyISA 的合法程序内存镜像表示。emu 将 `.sfs` 文件按偏移加载到同值地址，并从入口地址 `0x00000100` 开始执行。

一期到此结束
<!--
### LLVM 后端

LLVM 后端用于让 C、Rust 或其他 LLVM 前端语言生成 ShyISA 代码。

### OS

OS 是运行在 ShyISA 上的最小操作系统，负责把裸机环境组织成可运行用户程序的平台。


### libc

libc 是面向 C 程序的基础运行时库，运行在 ShyISA OS 之上。


### librust

librust 是面向 Rust 程序的基础运行时支持，运行在 ShyISA OS 之上。
-->
