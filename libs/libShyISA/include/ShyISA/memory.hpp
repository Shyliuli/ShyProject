#pragma once
#include "common.hpp"
#include "address.hpp"
#include "CoreError.hpp"
#include <memory>
#include <array>

// HUMAN: 16M，后续应该由sys大小减去保留空间算出
#define MEM_SIZE 0x01000000 ///< 内存大小定义：16MB

/**
 * @class Memory
 * @brief 内存管理类
 *
 * 管理ShyISA架构处理器的主内存。提供统一的内存访问接口，
 * 支持读写操作和地址验证。使用固定大小的数组实现内存存储。
 *
 * 内存特性：
 * - 大小：16MB (0x01000000字节)
 * - 访问单位：32位字(u32)
 * - 地址验证：自动检查地址类型和范围
 * - 异常安全：使用Result类型处理错误
 */
class Memory
{
public:
    /**
     * @brief 创建Memory实例的工厂方法
     * @return Result<std::unique_ptr<Memory>, CoreError> 成功时返回Memory实例的智能指针，失败时返回分配错误
     *
     * 使用RAII和智能指针管理内存，确保异常安全性。
     * 内存分配失败时会返回AllocError。
     */
    fn static create() -> Result<std::unique_ptr<Memory>, CoreError>;

    /**
     * @brief 写入内存值
     * @param val 要写入的32位值
     * @param addr 目标内存地址
     * @return Result<Unit, CoreError> 成功时返回Unit，失败时返回错误信息
     *
     * 将指定值写入目标内存地址。地址会经过类型检查确保为有效的内存地址。
     * 支持的错误类型：InvalidAddress（地址无效）、InvalidType（地址类型不匹配）
     */
    fn write(u32 val, Address addr) -> Result<Unit, CoreError>;

    /**
     * @brief 读取内存值
     * @param addr 源内存地址
     * @return Result<u32, CoreError> 成功时返回内存值，失败时返回错误信息
     *
     * 从指定内存地址读取32位值。地址会经过类型检查确保为有效的内存地址。
     * 支持的错误类型：InvalidAddress（地址无效）、InvalidType（地址类型不匹配）
     */
    fn read(Address addr) -> Result<u32, CoreError>;

private:
    std::array<u32, MEM_SIZE> memory; ///< 内存存储数组，使用32位字为单位

    /**
     * @brief 私有构造函数
     *
     * 初始化所有内存为0。使用私有构造函数强制通过create()工厂方法创建实例。
     */
    Memory();
};