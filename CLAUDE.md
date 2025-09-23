### 项目AI助手开发指南

你好，AI助手。

为了确保你在本项目中的协作高效且符合规范，请在执行任何编码、测试或重构任务时，严格遵守以下开发指南。

#### 0. 语言

* 没有特殊限制，必须使用汉语。
* 存在特殊限制时（如遵循通用技术规范），优先使用简明的英语。

#### **1. 项目概述**

* **技术栈**: C++23, xmake
* **核心库**: `rustic.cpp`，它为C++引入了部分Rust风格的语法和错误处理机制。
* **测试框架**: `dottest.h`
* **开发模式**: TDD (测试驱动开发)

#### **2. 核心开发规范**

你的代码实现必须优先遵循以下风格：

**2.1. 类型系统**

优先使用 `rustic.h` 中定义的类型别名，以保证类型宽度和意图的明确性。

* **整数**: `u8`, `i8`, `u16`, `i16`, `u32`, `i32`, `u64`, `i64`
* **浮点数**: `f32`, `f64`
* **指针宽度整数**: `usize`, `isize`

**2.2. 语法风格**

* **函数声明**: 使用 `fn` 关键字。
* **不可变变量定义**: 使用 `let` 关键字。

**2.3. 错误与空值处理**

**核心原则：在你的业务逻辑中，禁止使用 `try-catch` 语句处理可预见的失败情况。** 应始终使用 `Result<T, E>` 将错误作为函数返回值的一部分，这使得错误路径成为API契约的一部分，更加明确和安全。

`try-catch` 仅允许在非常特殊的情况下使用：即在与**外部的、只通过异常报告错误的第三方库**交互的最边界层，其唯一目的是捕获异常并立即将其转换为一个 `Result::Err`值，从而将不安全的外部世界与我们安全的内部逻辑隔离开。

* **`Option<T>`**: 用于表示一个值“可有可无”的情况。

  * **构造**: `Some(value)`, `None()`
  * **使用**: 优先使用 `map`, `and_then`, `unwrap_or`等组合子方法，而不是手动的 `is_some()`检查和 `unwrap()`。
* **`Result<T, E>`**: 用于表示一个操作“可能成功也可能失败”的情况。

  * **构造**: `Ok(value)`, `Err(error)`
  * **类型推导**: 使用全局辅助函数 `Ok<E>(value)` 和 `Err<T>(error)` 可以简化类型声明。
  * **链式调用与 `and_then`**: 当一个操作的成功结果是另一个可能失败操作的输入时，使用 `and_then` 可以避免深层嵌套的 `if` 判断，形成优雅的链式调用。这对于错误传递尤其有用。

  **示例：**

  ```cpp
  // 假设有一个配置项
  struct Config {
      Option<std::string> version;
  };

  // 第一步：获取配置，这可能失败
  fn get_config(const std::string& path) -> Result<Config, std::string> {
      if (path == "valid.conf") {
          // 假设配置中 version 字段是可选的
          return Ok<std::string>(Config{Some("1.0")});
      }
      return Err<Config>("配置文件读取失败");
  }

  // 第二步：从配置中提取版本号，这也可能失败（因为 version 是 Option）
  fn get_version_from_config(const Config& conf) -> Result<std::string, std::string> {
      if (conf.version.is_some()) {
          return Ok<std::string>(conf.version.unwrap());
      }
      return Err<std::string>("配置中缺少版本号");
  }

  // 使用 and_then 将两个可能失败的步骤链接起来
  fn main() {
      // 成功路径
      let result_ok = get_config("valid.conf").and_then(get_version_from_config);
      if (result_ok.is_ok()) {
          // 输出: "成功获取版本号: 1.0"
          std::println("成功获取版本号: {}", result_ok.unwrap());
      }

      // 失败路径（第一步就失败了）
      let result_err1 = get_config("invalid.conf").and_then(get_version_from_config);
      if (result_err1.is_err()) {
          // 输出: "错误: 配置文件读取失败"
          std::println("错误: {}", result_err1.unwrap_err());
      }
  }
  ```

  在这个例子中，`and_then` 只有在 `get_config` 返回 `Ok` 时才会执行 `get_version_from_config`。如果 `get_config` 返回 `Err`，整个链条会立即短路并返回那个 `Err`，代码清晰且健壮。

**2.4. 测试**

所有新功能或错误修复都必须遵循 **TDD (测试驱动开发)** 模式。

#### **3. 开发工作流**

请严格遵循以下基于Git的TDD工作流程：

1. **创建分支**

   * **命令**: `git checkout -b feature/your-task-description`
2. **规划与记录**

   * 创建 `PLAN.md` 文件并列出任务清单。
3. **编码与提交（测试先行）**

   * **第一步**: 编写第一个（失败的）测试。
   * **提交**: `git add .` & `git commit -m "测试: 为[功能名]添加[场景]的失败测试"`
4. **实现与提交（通过测试）**

   * **第二步**: 编写最少的代码让测试通过。
   * **提交**: `git add .` & `git commit -m "功能: 实现[功能名]以通过[场景]测试"`
   * **更新文档**: 在 `PLAN.md` 中勾选完成项，并使用 `git commit --amend --no-edit` 合并提交。
5. **循环**

   * 重复步骤3和4，直到所有任务点完成。
6. **重构**

   - 尝试去除3和4引入的冗余代码，同时优化代码性能，可读性等，但是每次修改需确保测试可正常通过

#### **4. 总结**

你的所有工作都应遵循这个 **"分支 -> 规划 -> 测试 -> 提交 -> 实现 -> 提交 -> 循环"** 的模式。

现在，请开始你的下一项任务。
