# chibicc_shy

`chibicc_shy` 是一个运行在 Shy 裸机环境上的 chibicc/ShyC 编译器实验版。
它从标准输入读取 ShyC/C 源码，遇到单独一行 `__SHYCC_END__` 后停止读取，
然后把生成的 Shy 汇编写到标准输出。

这个目录是从 `third_party/chibicc` 拷贝出来的移植工作区。移植中需要的粗糙运行时
实现，例如 bump allocator、`FILE` 包装和本编译器专用的格式化胶水，都留在本目录。
只有可以在裸机上合理通用实现的接口才放进 `libshy`，例如只支持 fd 0/1/2 的
`read`/`write`。

## 当前范围

- 输入：stdin，结束标记为单独一行 `__SHYCC_END__`。
- 输出：stdout 上的 Shy 汇编文本。
- 前端：复用当前 ShyC fork 的 `impl`、`self`、方法调用和 RAII/drop 支持。
- 预处理：当前启动器直接 tokenize/parse，不跑完整 include/macro 预处理流程。
- 目标程序测试：测试源自带最小 C 库实现，不要求被编译出来的程序链接 `libshy`。

## 构建和运行

所有产物写入 `projects/chibicc_shy/target/`。

```sh
make -C projects/chibicc_shy clean
make -C projects/chibicc_shy build
```

`build` 会生成：

- `target/chibicc_shy.sfs`
- `target/chibicc_shy.sym`
- `target/*.shy`：每个编译器源文件对应一个 `-S` 输出
- `target/chibicc_shy.shy`：把这些 `.shy` 拼接到一起，方便查看

运行编译器并生成测试程序汇编：

```sh
make -C projects/chibicc_shy run
```

这一步会把 `test.shyc` 和结束标记送入 `target/chibicc_shy.sfs`，输出
`target/test.shy`。

构建测试程序：

```sh
make -C projects/chibicc_shy build_test
```

运行测试程序并比对输出：

```sh
make -C projects/chibicc_shy run_test
```

完整测试也可以直接运行：

```sh
make -C projects/chibicc_shy test
```

期望输出是：

```text
[bookid] 红星照耀中国:10
红星照耀中国
```

## 手动 stdin 示例

```sh
(printf 'int main(void){return 0;}\n__SHYCC_END__\n') \
  | cargo run -q -p emu -- projects/chibicc_shy/target/chibicc_shy.sfs \
  > /tmp/out.shy
```

如果输入中有 UTF-8 字符串，`chibicc_shy` 会按原始字节读取 stdin，避免把中文字符
解码成 code point 后截断。
