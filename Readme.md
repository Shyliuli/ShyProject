ShyProject是一个面向教学的计算机系统，基于ShyISA。

- 有关ShyISA的内容，请查看[这里](ShyISA.md)。
- 有关项目工程概要设计，请查看[这里](Design.md)。
- 有关当前裸机C子集和ABI，请查看[这里](ShyC.md)。

## 运行 testc

`test/testc` 是当前 ShyC 裸机 C 前端的最小联调样例。流程是先把两个
`.c` 文件分别编译为 Shy 汇编，再分别汇编为对象文件，最后链接并运行。

先构建本地 chibicc：

```sh
make -C third_party/chibicc chibicc
```

把 C 文件分别编译为 `.shy`：

```sh
third_party/chibicc/chibicc --target=shy -S -o test/testc/main.shy test/testc/main.c
third_party/chibicc/chibicc --target=shy -S -o test/testc/put.shy test/testc/put.c
```

把 `.shy` 分别汇编为 `.sobj`：

```sh
cargo run -q -p asm -- test/testc/main.shy -o test/testc/main.sobj
cargo run -q -p asm -- test/testc/put.shy -o test/testc/put.sobj
```

链接：

```sh
cargo run -q -p linker -- test/testc/main.sobj test/testc/put.sobj -o test/testc/testc.sfs --sym test/testc/testc.sym
```

运行：

```sh
cargo run -q -p emu -- test/testc/testc.sfs
```

当前期望输出：

```text
hello world
```
