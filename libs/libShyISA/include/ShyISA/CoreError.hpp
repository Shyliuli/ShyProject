#pragma once
#include <string>
#include <variant>
#include <type_traits>
#include <print>
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
struct AllocError {
    std::string message;
    
    void print() const {
        std::println("AllocError: {}", message);
    }
};

/**
 * @struct InvalidAddress
 * @brief 无效地址错误
 *
 * 当访问非法或无效地址时抛出的错误类型
 */
struct InvalidAddress {
    std::string message;
    u32 raw_address;
    
    void print() const {
        std::println("InvalidAddress: {}, raw_address: 0x{:08x}", message, raw_address);
    }
};

/**
 * @struct InvalidType
 * @brief 无效类型错误
 *
 * 当类型检查失败或类型不匹配时抛出的错误类型
 */
struct InvalidType {
    std::string message;
    std::string type;
    
    void print() const {
        std::println("InvalidType: {}, type: {}", message, type);
    }
};

/**
 * @struct RegNotFind
 * @brief 寄存器未找到错误
 *
 * 当访问不存在的寄存器地址时抛出的错误类型
 */
struct RegNotFind {
    std::string message;
    u32 reg_addr;
    
    void print() const {
        if (reg_addr == 0) {
            std::println("RegNotFind: {}", message);
        } else {
            std::println("RegNotFind: {}, reg_addr: 0x{:08x}", message, reg_addr);
        }
    }
};

 /**
 * @struct OverFlow
 * @brief 溢出错误
 *
 * 当数据溢出时抛出的错误类型
 */
struct OverFlow {
    std::string message;
    
    void print() const {
        std::println("OverFlow: {}", message);
    }
};

/**
 * @class CoreError
 * @brief 核心错误容器类
 *
 * 使用std::variant存储错误，提供统一的接口
 */
class CoreError {
private:
    std::variant<AllocError, InvalidAddress, InvalidType, RegNotFind, OverFlow> error;
    
public:
    // 构造函数，接受任意错误类型
    template<typename T,
             typename = std::enable_if_t<!std::is_same_v<std::decay_t<T>, CoreError>>>
    CoreError(T&& err) : error(std::forward<T>(err)) {}
    
    // 统一的print接口
    void print() const {
        std::visit([](const auto& e) { e.print(); }, error);
    }
    
    // 获取底层variant（如果需要类型检查）
    const auto& get() const { return error; }
    
    // 类型检查
    template<typename T>
    bool is() const {
        return std::holds_alternative<T>(error);
    }
    
    // 获取具体类型
    template<typename T>
    const T& as() const {
        return std::get<T>(error);
    }
};
