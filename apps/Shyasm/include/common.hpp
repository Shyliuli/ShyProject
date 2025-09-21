/**
 * @file common.hpp
 * @brief ShyAsm 汇编器各模块共享的公共包含与别名。
 */

#include "libShyISA.hpp"

/// 指向 `std::unique_ptr` 的别名，用于减少书写冗余。
using std::unique_ptr;

/// 指向标准字符串的别名，用于保存汇编源码或中间结果。
using std::string;
