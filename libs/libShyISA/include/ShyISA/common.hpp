#pragma once

/**
 * @file common.hpp
 * @brief 公共头文件
 *
 * 包含整个ShyISA库的公共定义和依赖。
 * 定义编译器特性、导入必要的标准库和第三方库。
 */

// HUMAN: 编译时特性定义
#define ENABLE_RS_ERROR   ///< 启用Rust风格错误处理
#define ENABLE_RS_KEYWORD ///< 启用Rust风格关键字支持

// 第三方库导入
#include "rustic.hpp"  ///< Rust风格的C++扩展库

// 标准库导入
#include <iostream>  ///< 标准输入输出流
#include <print>     ///< C++23打印功能
#include <string>    ///< 字符串处理
#include <vector>    ///< 动态数组容器
#include <memory>    ///< 智能指针和内存管理

// 项目内部头文件
#include "CoreError.hpp"  ///< 核心错误类型定义