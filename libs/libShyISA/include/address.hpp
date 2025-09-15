#pragma once
#include "common.hpp"
#include "CoreError.hpp"

/**
 * @class Address
 * @brief 地址封装类
 *
 * 管理ShyISA架构中的各种地址类型，提供地址验证和类型检查功能。
 * 支持内存地址、寄存器地址、命令地址、IO地址和显存地址等不同类型。
 */
class Address
{
public:
    /**
     * @enum Type
     * @brief 地址类型枚举
     *
     * 定义处理器支持的各种地址空间类型
     */
    enum class Type
    {
        Mem,     ///< 内存地址
        Reg,     ///< 寄存器地址
        Command, ///< 命令地址
        IO,      ///< IO端口地址
        VRAM     ///< 显存地址
    };

    /**
     * @brief 将地址类型转换为字符串
     * @param t 地址类型
     * @return std::string 类型的字符串表示
     *
     * 用于调试和错误信息输出
     */
    fn static type_to_string(Type t) -> std::string;

    // 禁用动态内存分配
    void *operator new(size_t) = delete;
    void *operator new[](size_t) = delete;

    /**
     * @brief 构造函数
     * @param addr 原始地址值
     *
     * 从32位原始地址创建Address对象
     */
    Address(u32 addr);

    /**
     * @brief 获取地址类型
     * @return Result<Type, CoreError> 成功时返回地址类型，失败时返回错误
     *
     * 根据地址值判断其属于哪种地址类型
     */
    fn type() -> Result<Type, CoreError>;

    /**
     * @brief 转换为32位无符号整数
     * @return Result<u32, CoreError> 成功时返回地址值，失败时返回错误
     *
     * 获取原始地址值，用于底层地址操作
     */
    fn to_u32() -> Result<u32, CoreError>;

    /**
     * @brief 带类型检查的地址转换
     * @param expected_type 期望的地址类型
     * @param context 上下文信息，用于错误报告
     * @return Result<u32, CoreError> 成功时返回地址值，失败时返回错误
     *
     * 验证地址类型是否符合预期，然后返回原始地址值
     */
    fn to_u32_with_check(Type expected_type, const std::string &context) -> Result<u32, CoreError>;

private:
    u32 raw_addr; ///< 原始地址值
};