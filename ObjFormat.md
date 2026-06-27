# ShyISA Object 文件格式规范

本文定义 ShyISA 汇编器输出的最小可链接 object 格式。object 文件不是可执行内存镜像，不能直接交给模拟器执行。它只保存 section 内容、对外符号、局部 label 解析结果和需要链接器补齐的地址字段。

最终可执行文件仍由链接器输出为 `.sfs` raw 内存镜像。

## 1. 设计目标

ShyISA object 格式只解决三个问题：

- 符号可以对外暴露，并在链接时确定最终地址。
- 源码可以定义 section，链接器可以把 section 放到指定位置。
- 多个汇编文件可以先分别编译，再由链接器合并。

本格式不定义权限位、符号类型、动态链接、调试信息或复杂文件头。

## 2. 文件扩展名

ShyISA object 文件使用 `.sobj` 扩展名。

汇编与链接流程为：

```text
source.asm -> asm -> source.sobj
*.sobj     -> linker -> program.sfs
```

## 3. 逻辑结构

object 文件包含三类记录：

```text
sections
symbols
relocations
```

等价 Rust 表示如下：

```rust
pub struct ObjectFile {
    pub sections: Vec<ObjSection>,
    pub symbols: Vec<ObjSymbol>,
    pub relocations: Vec<ObjRelocation>,
}
```

## 4. Section

section 是链接器放置代码和数据的最小单位。

```rust
pub struct ObjSection {
    pub name: String,
    pub bytes: Vec<u8>,
}
```

规则：

- section 名来自汇编源码中的 `.section <name>`。
- section 名本身也是一个对外暴露的符号，表示该 section 的起始地址。
- 同一个 object 文件中可以多次切换到同名 section，内容按出现顺序追加。
- 链接同一批 object 文件时，不允许出现重名 section。
- `text._start` 是入口 section，链接器必须把它放到程序入口地址 `0x00000100`。
- 默认 section 名 `text._start` 和 `data` 主要用于简单单文件程序。多文件链接时，除入口文件的 `text._start` 外，源码应显式使用唯一 section 名。

推荐命名：

```text
text._start
text.main
text.print
data.message
data.counter
```

命名中的 `text.` 和 `data.` 只作为链接器默认布局的分类依据，不是权限标记。

## 5. Symbol

symbol 表示对外可见名字。它可以来自 `.symbol <name>`，也可以来自 section 名。

```rust
pub struct ObjSymbol {
    pub name: String,
    pub section: String,
    pub offset: u32,
}
```

规则：

- `.symbol <name>` 定义当前位置的对外符号。
- `.section <name>` 自动定义一个同名符号，offset 为 `0`。
- symbol 的地址在 object 文件内表示为 `section + offset`。
- symbol 的最终绝对地址只能由链接器确定。
- 链接同一批 object 文件时，不允许出现重名 symbol。

示例：

```asm
.section text.print
.symbol print
    oututfa 1x
    ret
```

会产生：

```text
section: text.print
symbol:  text.print -> text.print + 0
symbol:  print      -> text.print + 0
```

## 6. 局部 Label

局部 label 使用 `<name>:` 定义。

```asm
.section text.main
.symbol main
loop:
    addn 1x 1
    jmpn loop
```

局部 label 不写入 object 的 symbol 表，也不对其他 object 文件可见。汇编器必须在当前源码文件内把局部 label 解析为：

```text
section + offset
```

如果局部 label 被用于需要写入最终绝对地址的地方，汇编器应产生 relocation。链接器根据 label 所在 section 的最终地址补齐字段。

实现可以选择把局部 label 编译成一种内部 relocation target，例如：

```rust
pub enum RelocTarget {
    Symbol(String),
    SectionOffset { section: String, offset: u32 },
}
```

如果实现希望保持 object 更简单，也可以在 object 内部为局部 label 生成不可导出的临时名字，但这些临时名字不得暴露给其他 object 文件或最终 `.sym` 文件。

## 7. Relocation

relocation 表示 object 文件中某个 32 位字段需要链接器在最终地址确定后回填。

第一版只支持一种 relocation：把目标的最终绝对地址写成 32 位大端序整数。

ShyISA object 格式不需要单独记录 extern 声明。若 relocation 的 `Symbol(name)` 在当前 object 的 symbol 表中找不到，链接器应在其他 object 的 symbol 表中查找。链接后仍找不到则报未定义符号错误。

```rust
pub struct ObjRelocation {
    pub section: String,
    pub offset: u32,
    pub target: RelocTarget,
    pub addend: u32,
}
```

```rust
pub enum RelocTarget {
    Symbol(String),
    SectionOffset { section: String, offset: u32 },
}
```

字段含义：

- `section`：要修改哪个 section 的字节内容。
- `offset`：要修改该 section 内哪个字节开始的 32 位字段。
- `target`：要写入哪个符号或 section 内偏移的最终地址。
- `addend`：在目标地址基础上额外加上的无符号偏移；普通符号引用为 `0`，`message(8)` 这类表达式为 `8`。

链接器处理 relocation 时执行：

```text
final = target_section_base + target_offset + addend
write_u32_be(section.bytes[offset..offset + 4], final)
```

其中 `Symbol(name)` 需要先查 symbol 表，得到该 symbol 的 `section + offset`。

## 8. 指令中的重定位

ShyISA 指令固定为 3 个 32 位字：

```text
opcode
arg1
arg2
```

当 `arg1` 或 `arg2` 引用 symbol、section 或局部 label 时，汇编器先写入 `0x00000000` 占位，并记录 relocation。

示例：

```asm
.section text._start
.symbol _start
calln print
```

假设 `calln` 的 opcode 已写入当前 section offset `0`，那么 `print` 位于第二个 32 位参数字段，relocation 记录为：

```text
section = 当前section
offset  = 4
target  = Symbol("print")
addend  = 0
```

示例：

```asm
.section text._start
.symbol _start
setn 1x message(4)
```

其中 `message(4)` 写入第三个 32 位字段：

```text
section = 当前section
offset  = 当前指令offset + 8
target  = Symbol("message")
addend  = 4
```

## 9. 数据中的重定位

数据 section 中也可以引用 symbol。数据初始化仍使用 `<address-expr> <value>` 格式；若写入位置或 32 位数据值引用 symbol，汇编器同样写入占位值并产生 relocation。

示例：

```asm
.section data.table
.symbol main_ptr_table
main_ptr_table main(12)
```

如果右侧的 `main(12)` 被解析为符号引用，object 中 `main_ptr_table` 对应位置先写入 `0x00000000`，并记录：

```text
section = data.table
offset  = 0
target  = Symbol("main")
addend  = 12
```

## 10. 最小链接规则

第一版链接器不需要链接脚本，使用固定默认规则：

- `text._start` 必须存在，并放到 `0x00000100`。
- 其他 `text.*` section 接在 `text._start` 后面。
- `data.*` section 从 `0x00200000` 开始依次放置。
- 其他 section 名暂不定义默认布局，链接器可以报错。
- section 起始地址按 4 字节对齐。

链接器步骤：

1. 读取所有 `.sobj`。
2. 检查 section 名和 symbol 名是否重复。
3. 按默认规则为每个 section 分配最终地址。
4. 计算所有 symbol 的最终地址。
5. 处理所有 relocation。
6. 把 section bytes 写入 `.sfs` raw 内存镜像对应地址。

## 11. 可选符号表输出

链接器可以额外输出 `.sym` 文本文件，方便模拟器和调试工具使用。

格式：

```text
_start 0x00000100
main 0x00000118
message 0x00200000
```

`.sym` 不影响程序执行。


### 12. `.sobj` 二进制格式

所有 `u32` 使用大端序。
所有链表指针都是文件内绝对偏移，`0` 表示 `NULL`。
所有 `*_c_str` 都是 UTF-8 字节串，以 `0x00` 结尾，字符串内容不得包含 `0x00`。

文件头：

```text
u32 magic = 0x66CCFF00
u32 section_start      // first SectionNode file offset, 0 means empty
u32 symbol_start       // first SymbolNode file offset, 0 means empty
u32 relocation_start   // first RelocationNode file offset, 0 means empty
```

`SectionNode` 保存一个 section 的名字和字节内容：

```text
SectionNode {
  u32 next_section     // next SectionNode file offset, 0 means end
  section_name_c_str
  u32 byte_size
  u8 bytes[byte_size]
}
```

`SymbolNode` 保存一个对外符号的位置：

```text
SymbolNode {
  u32 next_symbol      // next SymbolNode file offset, 0 means end
  u32 offset           // offset inside section_name
  section_name_c_str
  symbol_name_c_str
}
```

`RelocationNode` 保存一个需要链接器回填的 32 位字段：

```text
RelocationNode {
  u32 next_relocation  // next RelocationNode file offset, 0 means end
  u32 offset           // patch offset inside section_name
  u32 addend
  u32 target_kind      // 1 = Symbol, 0 = SectionOffset
  section_name_c_str   // section to patch

  if target_kind == 1:
    target_symbol_name_c_str
  else:
    u32 target_offset
    target_section_name_c_str
}
```

`target_kind` 只能是 `1` 或 `0`，其他值为非法格式。
