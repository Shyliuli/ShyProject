/**
 * @file DataProcess.hpp
 * @brief ShyAsm 数据段处理相关的接口定义。
 */

#include "common.hpp"

/**
 * @brief 负责解析与构建数据段的处理器。
 */
class DataProcess {
public:
    /// 与数据段相关的原始或处理后代码。
    string code;
    /// 生成或加载的内存镜像。
    unique_ptr<Memory> memory;
    /// 指示当前对象是否已完成处理。
    bool has_processed;

    /**
     * @brief 基于源码创建数据段处理器。
     * @param input 包含数据段定义的源码。
     * @return 成功返回 DataProcess 指针，失败返回核心错误。
     */
    fn static create(string input)->Result<unique_ptr<DataProcess>,CoreError>;

    /**
     * @brief 执行数据段解析与转换，更新has_processed
     * @return 成功返回自身引用，失败返回核心错误。
     */
    fn process()->Result<DataProcess&,CoreError>;

    /**
     * @brief 输出数据段的二进制表示。
     * @return 成功返回内存对象，失败返回核心错误。
     */
    fn bin()->Result<unique_ptr<Memory>,CoreError>;

private:
    /**
     * @brief 使用源码构造数据段处理器。
     * @param input 原始数据段源码。
     */
    DataProcess(string input);
};
