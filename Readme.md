ShyProject是一个面向教学的计算机系统，基于ShyISA。

- 有关ShyISA的内容，请查看[这里](ShyISA.md)。
- 有关项目工程概要设计，请查看[这里](Design.md)。
- 有关当前裸机C子集和ABI，请查看[这里](ShyC.md)。

## chibicc_shy 自举实验

`projects/chibicc_shy` 是运行在 Shy 上的 chibicc/ShyC 编译器移植实验。它从
stdin 读取源码，遇到单独一行 `__SHYCC_END__` 后结束输入，并把 Shy 汇编输出到
stdout。

阶段命令：

```sh
make -C projects/chibicc_shy build       # 生成 target/chibicc_shy.sfs/.sym/.shy
make -C projects/chibicc_shy run         # 用 stdin 测试源生成 target/test.shy
make -C projects/chibicc_shy build_test  # 汇编链接 target/test.shy 为 target/test.sfs
make -C projects/chibicc_shy run_test    # 运行 target/test.sfs 并比对输出
```

完整流水线：

```sh
make -C projects/chibicc_shy test
```

更多说明见 [projects/chibicc_shy/README.md](projects/chibicc_shy/README.md)。

## 运行 testc

`test/testc` 是当前 ShyC 裸机 C 前端的最小联调样例。流程是先把两个
`.c` 文件分别编译为 Shy 汇编，再分别汇编为对象文件，最后链接并运行。

推荐使用 `shycc` 驱动一次完成前端、汇编和链接：

```sh
cargo run -q -p shycc -- test/testc/main.c test/testc/put.c -o test/testc/testc.sfs --sym test/testc/testc.sym
cargo run -q -p emu -- test/testc/testc.sfs
```

`shycc` 支持类似 GCC 的阶段选项：

```sh
cargo run -q -p shycc -- -S test/testc/main.c -o test/testc/main.shy
cargo run -q -p shycc -- -c test/testc/main.shy -o test/testc/main.sobj
cargo run -q -p shycc -- test/testc/main.sobj test/testc/put.sobj -o test/testc/testc.sfs --sym test/testc/testc.sym
```

调试中间产物可使用 `-save-temps` 保留 `.shy` 和 `.sobj`，使用 `-###` 只打印将要执行的命令。需要链接内部软浮点运行库时传入 `-lfloat`。

ShyC 原生文件后缀：

```text
.shyc  ShyC 源文件，也兼容 .c
.shyh  ShyC 头文件，也兼容 .h
.shy   Shy 汇编
.sobj  Shy object
```

## 构建工具链产物

把当前工具链二进制收集到 `target/bin/`：

```sh
make bin
```

产物包括 `shycc`、`shyasm`、`shyld`、`shyemu` 和本地 `chibicc`。`shycc` 会优先使用同目录下的这些工具，因此可以直接运行：

```sh
target/bin/shycc test/shyc/libshy_smoke.shyc -llibshy -o /tmp/app.sfs
target/bin/shyemu /tmp/app.sfs
```

安装到指定前缀：

```sh
make install-bin PREFIX=/usr/local
```

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
