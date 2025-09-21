/**
 * @file FirstProcess.hpp
 * @brief 负责注释、宏、标志等预处理步骤的链式管线。
 */

#include "common.hpp"

/**
 * @brief 预处理的第一阶段，采用链式调用处理源码。
 *
 * 该阶段封装了注释、宏与标志等预处理步骤，允许调用者按需串联执行。
 */
class FirstProcess {
    /// 当前阶段正在处理的源码内容。
    string code;
public:
    /**
     * @brief 使用原始源码创建预处理阶段对象。
     * @param input 待处理的原始汇编文本。
     * @return 成功返回 FirstProcess 指针，失败返回核心错误。
     */
    fn create(string input)->Result<unique_ptr<FirstProcess>,CoreError>;

    /**
     * @brief 移除源码中的注释内容。
     * @return 返回自身以便继续链式调用。
     */
    fn comment_process()->FirstProcess&;

    /**
     * @brief 展开或处理宏定义。
     * @return 成功返回自身引用，失败返回核心错误。
     */
    fn macro_process()->Result<FirstProcess&,CoreError>;

    /**
     * @brief 处理段落或标志指令。
     * @return 成功返回自身引用，失败返回核心错误。
     */
    fn flag_process()->Result<FirstProcess&,CoreError>;

    /**
     * @brief 获取当前阶段处理后的源码。
     * @return 返回处理后的字符串。
     */
    fn to_string()->string;

private:
    /**
     * @brief 使用原始源码构造预处理阶段对象。
     * @param input 原始汇编文本。
     */
    FirstProcess(string input);
};
