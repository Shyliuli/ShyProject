本项目旨在使用rust实现ShyISA的模拟器(emu)和汇编器(asm),你可以查看ShyISA.md了解ISA设计规范。
你应该查看TODO.md来定位当前进度

# 必须遵守的规则

1. 所有的代码必须使用UTF-8编码
2. 遵守Rust的语法和推荐风格，为保证这一规则，你必须在修改后运行cargo bulid检查
3. 禁止简化逻辑，如遇到任何问题，必须立刻暂停报告

# 工作流程：

1. 若任务较复杂，创建并进入一个分支，分支名格式为：feature/xxx；若简单，跳过这一步
2. 查阅TODO.md，确定当前进度
3. 执行任务
4. 运行cargo bulid检查，如有问题，回到3
5. 提交代码
6. 合并分支
7. 若修改了源代码(仅仅修改注释/写文档不算)，必须在develop.md中写:
   0. 修改时间
   1. 你做了什么？
   2. 具体修改的文件列表以及行号
   3. 遇到了什么困难？（如果没有可以跳过）
   4. 如果遇到了困难,总结经验和教训！（如果没有可以跳过）

# 项目介绍

## emu

待实现

## asm

待实现

# shy_isa_lib

shy_isa_lib 是 emu 与 asm 的公共库
当前结构：
.
├── Cargo.toml
└── src
    ├── error.rs
    ├── isa_def.rs
    ├── lib.rs
    ├── memory.rs
    └── types.rs
模块说明：

- lib.rs：库入口，导出 error/isa_def/memory/types。
- error.rs：统一错误类型 CoreError；实现 Display 与 Error。
- isa_def.rs：定义寄存器、操作码、内存与 I/O 映射；枚举 repr(u32)；提供 TryFrom`<Word>` 解码。
- memory.rs：32 位字宽线性内存；读/写（含边界检查）与镜像加载/保存（大端）；提供安全/不安全接口。
- types.rs：基本类型别名；Word/Addr = u32。
