# ShyProject

ShyProject 是一个面向教学的完整计算机系统实验项目。它从零开始构建了一套可工作的
计算机——包括自定义指令集、汇编器、模拟器、链接器、C 编译器以及裸机运行时库。
目标不是只定义一套纸上规范，而是让学习者亲手跑通从源代码到硅片的完整链路。

## 项目概览

```
  C/ShyC 源码 ──→ shycc ──→ .shy 汇编
                              │
                              ▼
                           shyasm ──→ .sobj object
                                          │
                                          ▼
                                       shyld ──→ .sfs 内存镜像
                                                    │
                                                    ▼
                                                 shyemu 执行
```

核心组件：

| 组件 | 路径 | 说明 |
|------|------|------|
| **ShyISA** | `ShyISA.md` | 完整指令集架构规范——寄存器、地址映射、指令集、Trap 模型、二进制格式 |
| **asm** | `asm/` | 汇编器，将 ShyISA 汇编源码编译为 `.sobj` 可链接 object 文件 |
| **linker** | `linker/` | 链接器，将一个或多个 `.sobj` 链接为 `.sfs` raw 内存镜像 |
| **emu** | `emu/` | 模拟器，加载并执行 `.sfs` 镜像，是整个项目最重要的验证工具 |
| **shycc** | `shycc/` | C 编译器驱动，基于 chibicc 分支，支持 ShyC 语言扩展 |
| **libshy** | `libshy/` | 裸机 C 支持库——`stdio`、`string`、`ctype`、`stdlib` 等 |
| **chibicc\_shy** | `projects/chibicc_shy/` | 自举实验——在 Shy 裸机上运行的 chibicc/ShyC 编译器移植 |

## ShyISA 一览

ShyISA 是为教学设计的精简指令集架构，核心理念是"一切皆地址"——寄存器、指令、
内存、I/O 设备全部映射到统一的 32 位地址空间。

- **16 个 32 位通用寄存器**，以 `0x`、`1x`…`fx` 命名
- **固定 12 字节指令编码**（操作码 + 参数 1 + 参数 2）
- **双操作数架构**，支持地址和立即数两种寻址模式
- **内核态/用户态双模式**，统一 Trap 入口
- **段式虚拟内存**，最小特权模型
- **内存映射 I/O**——UART、声音输出
- **原子内存指令**、**窄访存指令**、**定时器中断**

详细规范见 [ShyISA.md](ShyISA.md)，工程概要设计见 [Design.md](Design.md)。

## ShyC 语言

ShyC 是面向 ShyISA 的裸机 C 前端。基于 chibicc 分支实现，提供：

- C 语言核心子集——整数、指针、结构体、联合体、数组、控制流、`goto`
- ILP32 指针模型，`long` 保持 64 位
- **ShyC 原生扩展**：`.shyc`/`.shyh` 后缀、`impl` 方法、`self` 指针、RAII `drop`、`asm!` 绑定内联汇编
- 软件浮点支持、基础原子操作

详细规范见 [ShyC.md](ShyC.md)，libshy 库说明见 [libshy/README.md](libshy/README.md)。

## 环境要求

- **Rust** 工具链（`cargo`）
- **make**
- C 编译器（用于构建本地 chibicc）

## 快速开始

### 构建工具链

```sh
# 构建所有工具到 target/bin/
make bin
```

产物：`target/bin/shycc`、`target/bin/shyasm`、`target/bin/shyld`、`target/bin/shyemu`、`target/bin/chibicc`

`shycc` 会优先使用同目录下的这些工具，因此可以直接运行：

```sh
target/bin/shycc test/shyc/libshy_smoke.shyc -llibshy -o /tmp/app.sfs
target/bin/shyemu /tmp/app.sfs
```

安装到系统路径：

```sh
make install-bin PREFIX=/usr/local
```

### 运行最小联调样例

`test/testc` 是 ShyC 裸机 C 前端的最小联调样例，预期输出 `hello world`。

推荐使用 `shycc` 驱动一次完成编译、汇编和链接：

```sh
cargo run -q -p shycc -- test/testc/main.c test/testc/put.c -o test/testc/testc.sfs --sym test/testc/testc.sym
cargo run -q -p emu -- test/testc/testc.sfs
```

`shycc` 支持类 GCC 的阶段选项：

```sh
# 只编译，不汇编
cargo run -q -p shycc -- -S test/testc/main.c -o test/testc/main.shy

# 只汇编，不链接
cargo run -q -p shycc -- -c test/testc/main.shy -o test/testc/main.sobj

# 只链接
cargo run -q -p shycc -- test/testc/main.sobj test/testc/put.sobj -o test/testc/testc.sfs --sym test/testc/testc.sym
```

附加选项：

- `-save-temps`：保留中间产物 `.shy` 和 `.sobj`
- `-###`：只打印将要执行的命令
- `--shy-emit-source-lines`：生成 `.shy` 时插入 `//source file:line text` 注释，记录 C 源码行与后续汇编的对应关系，供调试信息工具链使用
- `-lfloat`：链接内部软浮点运行库

### 分步执行（不通过 shycc）

先构建本地 chibicc：

```sh
make -C third_party/chibicc chibicc
```

编译 `.c` 为 `.shy`：

```sh
third_party/chibicc/chibicc --target=shy -S -o test/testc/main.shy test/testc/main.c
third_party/chibicc/chibicc --target=shy -S -o test/testc/put.shy test/testc/put.c
```

汇编 `.shy` 为 `.sobj`：

```sh
cargo run -q -p asm -- test/testc/main.shy -o test/testc/main.sobj
cargo run -q -p asm -- test/testc/put.shy -o test/testc/put.sobj
```

链接并运行：

```sh
cargo run -q -p linker -- test/testc/main.sobj test/testc/put.sobj -o test/testc/testc.sfs --sym test/testc/testc.sym
cargo run -q -p emu -- test/testc/testc.sfs
```

### 文件后缀一览

| 后缀 | 含义 |
|------|------|
| `.shyc` | ShyC 源文件（兼容 `.c`） |
| `.shyh` | ShyC 头文件（兼容 `.h`） |
| `.shy` | Shy 汇编 |
| `.sobj` | Shy object |
| `.sfs` | Shy 可执行内存镜像 |
| `.sym` | 符号文件 |

## chibicc_shy 自举实验

`projects/chibicc_shy` 是整个项目最具野心的实验：在 Shy 裸机环境上运行
chibicc/ShyC 编译器，让它编译新的 C 程序。输入通过 stdin 送入，遇到
`__SHYCC_END__` 结束标记后编译，将 Shy 汇编输出到 stdout。

```sh
# 构建自举编译器
make -C projects/chibicc_shy build

# 用自举编译器编译测试源
make -C projects/chibicc_shy run

# 汇编、链接并运行测试程序
make -C projects/chibicc_shy build_test
make -C projects/chibicc_shy run_test

# 一键全流程
make -C projects/chibicc_shy test
```

更多说明见 [projects/chibicc_shy/README.md](projects/chibicc_shy/README.md)。

## 项目结构

```
ShyProject/
├── ShyISA.md              # 指令集架构规范
├── Design.md              # 工程概要设计
├── ShyC.md                # 裸机 C 子集与 ABI
├── ObjFormat.md           # Object 文件格式
├── MemoryMap.md           # 内存布局说明
├── Makefile               # 构建入口
├── Cargo.toml             # Rust workspace
├── asm/                   # 汇编器
├── emu/                   # 模拟器
├── linker/                # 链接器
├── shycc/                 # C 编译器驱动
├── libshy/                # 裸机 C 支持库
├── shy_isa_lib/           # 共享 ISA 定义库
├── test/                  # 测试用例
├── projects/              # 子项目
│   └── chibicc_shy/       # 自举编译器实验
├── third_party/           # 第三方依赖
│   └── chibicc/           # chibicc C 编译器分支
└── vscode-shyc/           # VS Code 扩展
```
