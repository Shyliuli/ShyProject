/**
 * @file AsmProcess.hpp
 * @brief ShyAsm 的汇编主流程调度器。
 */

#include "common.hpp"

/**
 * @brief 管理完整汇编流程的处理器。
 */
class AsmProcess {
public:
    /// 当前待处理或已处理的汇编源码。
    string code;
    /// 汇编生成的内存对象。
    unique_ptr<Memory> memory;
    /// 标记是否已完成汇编处理。
    bool has_processed;

    /**
     * @brief 基于源码与内存上下文创建汇编处理器。
     * @param input 原始汇编源码。
     * @param memory 目标内存上下文。
     * @return 成功返回 AsmProcess 指针，失败返回核心错误。
     */
    fn static create(string input,unique_ptr<Memory> memory)->Result<unique_ptr<AsmProcess>,CoreError>;

    /**
     * @brief 执行汇编处理流程,更新has_processed
     * @return 成功返回自身引用，失败返回核心错误。
     */
    fn process()->Result<AsmProcess&,CoreError>;

    /**
     * @brief 导出最终的内存镜像。
     * @return 成功返回内存对象，失败返回核心错误。
     */
    fn bin()->Result<unique_ptr<Memory>,CoreError>;

private:
    /**
     * @brief 使用源码与内存构造汇编处理器。
     * @param input 原始汇编源码。
     * @param memory 目标内存上下文。
     */
    AsmProcess(string input,unique_ptr<Memory> memory);
};
