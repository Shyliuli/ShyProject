#pragma once
#include <string>
#include <variant>
#include "rustic.hpp"

/**
 * @file CoreError.hpp
 * @brief 核心错误类型定义
 *
 * 定义ShyISA核心组件中使用的各种错误类型。
 * 使用std::variant实现类型安全的错误处理。
 */

/**
 * @struct AllocError
 * @brief 内存分配错误
 *
 * 当内存分配失败时抛出的错误类型
 */
struct AllocError
{
    std::string message; ///< 错误描述信息
};

/**
 * @struct InvalidAddress
 * @brief 无效地址错误
 *
 * 当访问非法或无效地址时抛出的错误类型
 */
struct InvalidAddress
{
    std::string message;  ///< 错误描述信息
    u32 raw_address;      ///< 引起错误的原始地址值
};

/**
 * @struct InvalidType
 * @brief 无效类型错误
 *
 * 当类型检查失败或类型不匹配时抛出的错误类型
 */
struct InvalidType
{
    std::string message; ///< 错误描述信息
    std::string type;    ///< 引起错误的类型名称
};

/**
 * @struct RegNotFind
 * @brief 寄存器未找到错误
 *
 * 当访问不存在的寄存器地址时抛出的错误类型
 */
struct RegNotFind
{
    std::string message; ///< 错误描述信息
    u32 reg_addr;        ///< 引起错误的寄存器地址
};

/**
 * @typedef CoreError
 * @brief 核心错误类型别名
 *
 * 使用std::variant将所有可能的错误类型统一包装。
 * 支持类型安全的错误处理和模式匹配。
 */
using CoreError = std::variant<
    AllocError,      ///< 内存分配错误
    InvalidAddress,  ///< 无效地址错误
    InvalidType,     ///< 无效类型错误
    RegNotFind       ///< 寄存器未找到错误
>;