#ifndef RUSTIC_H
#define RUSTIC_H
/*
 * Rustic.hpp - C++安全编程辅助库
 * 
 * 本头文件提供了一套安全的数据类型和错误处理机制，帮助C++程序员避免常见的编程错误。
 * 这些工具借鉴了现代编程语言的设计理念，让C++代码更加安全可靠。
 * 
 * 主要功能：
 * 
 * 1. 明确的数据类型别名：
 *    - u8, u16, u32, u64：无符号整数类型（8位、16位、32位、64位）
 *    - i8, i16, i32, i64：有符号整数类型（8位、16位、32位、64位）
 *    - f32, f64：浮点数类型（32位、64位）
 *    - usize, isize：与系统指针大小相同的整数类型
 *    - fn：函数定义的简化写法，等同于auto
 *    - let：不可变变量声明，等同于const auto
 * 
 * 2. Option<T> 类 - 安全的可选值处理：
 *    传统C++中，函数返回指针或特殊值（如-1）来表示"没有结果"，容易导致空指针解引用或忘记检查。
 *    Option<T>强制程序员明确处理"有值"和"无值"两种情况：
 *    
 *    - Option<T>::Some(value)：表示有一个类型为T的值
 *    - Option<T>::None()：表示没有值
 *    - is_some()/is_none()：检查是否有值
 *    - unwrap()：获取值（如果没有值会抛出异常，强制程序员意识到风险）
 *    - unwrap_or(default)：获取值或提供默认值
 *    - map(function)：如果有值则应用函数变换，否则返回None
 *    
 *    示例：
 *    Option<int> find_index(const std::vector<int>& vec, int target);
 *    auto result = find_index(numbers, 42);
 *    if (result.is_some()) {
 *        std::cout << "找到了，位置：" << result.unwrap() << std::endl;
 *    } else {
 *        std::cout << "没找到" << std::endl;
 *    }
 * 
 * 3. Result<T, E> 类 - 明确的错误处理：
 *    传统C++错误处理要么用异常（可能被忽略），要么用错误码（容易忘记检查）。
 *    Result<T, E>强制程序员同时考虑成功和失败情况：
 *    
 *    - Result<T, E>::Ok(value)：表示操作成功，包含类型为T的结果
 *    - Result<T, E>::Err(error)：表示操作失败，包含类型为E的错误信息
 *    - is_ok()/is_err()：检查操作是否成功
 *    - unwrap()：获取成功值（如果是错误会抛异常）
 *    - unwrap_err()：获取错误值（如果是成功会抛异常）
 *    - unwrap_or(default)：获取成功值或提供默认值
 *    - map(function)：如果成功则变换值，如果失败则传递错误
 *    
 *    示例：
 *    Result<int, std::string> divide(int a, int b);
 *    auto result = divide(10, 2);
 *    if (result.is_ok()) {
 *        std::cout << "结果：" << result.unwrap() << std::endl;
 *    } else {
 *        std::cout << "错误：" << result.unwrap_err() << std::endl;
 *    }
 * 
 * 4. Unit 结构体：
 *    表示"无意义的值"，用于不需要返回具体内容的函数
 * 
 * 安全性提升：
 * - 消除空指针解引用：Option强制检查值是否存在
 * - 明确错误处理：Result让错误无法被忽视
 * - 类型安全：明确的数据类型减少隐式转换错误
 * - 函数式编程：map、and_then等方法支持安全的数据变换链
 * 
 * 这套工具不会完全阻止所有错误，但能帮助程序员在编写代码时更加注意潜在问题，
 * 减少运行时错误和难以调试的bug。
 */

#include <cstdlib>
#include<stdint.h>
#include <iostream>
#include <variant>
#include <functional>
#include <stdexcept>
#include <type_traits>
#include <utility>
#include <print>

using u8 = uint8_t;
using u16 = uint16_t;
using u32 = uint32_t;
using u64 = uint64_t;
using i8 = int8_t;
using i16 = int16_t;
using i32 = int32_t;
using i64 = int64_t;
using f32 = float;
using f64 = double;
using usize = size_t;
using isize = ssize_t;

#ifdef ENABLE_RS_KEYWORD

#define fn auto
#define let const auto
#endif

#ifdef ENABLE_RS_ERROR
// Unit type - equivalent to Rust's ()
struct Unit {
    bool operator==(const Unit&) const { return true; }
    bool operator!=(const Unit&) const { return false; }
};

// Forward declarations
template<typename T>
class Option;

template<typename T, typename E>
class Result;

// Option implementation
template<typename T>
class Option {
private:
    std::variant<std::monostate, T> value;

public:
    // Constructors
    Option() : value(std::monostate{}) {}
    Option(const T& val) : value(val) {}
    Option(T&& val) : value(std::move(val)) {}
    
    // Static factory methods
    static Option<T> Some(const T& val) {
        return Option<T>(val);
    }
    
    static Option<T> Some(T&& val) {
        return Option<T>(std::move(val));
    }
    
    static Option<T> None() {
        return Option<T>();
    }
    
    // Query methods
    bool is_some() const {
        return std::holds_alternative<T>(value);
    }
    
    bool is_none() const {
        return std::holds_alternative<std::monostate>(value);
    }
    
    // Access methods
    T& unwrap() {
        if (is_none()) {
            std::println("try to unwarp a none value");
             throw std::runtime_error("Called `Result::unwrap()` on an `Err` value");
        }
        return std::get<T>(value);
    }
    
    const T& unwrap() const {
        if (is_none()) {
            std::println("try to unwarp a none value");
             throw std::runtime_error("Called `Result::unwrap()` on an `Err` value");
        }
        return std::get<T>(value);
    }
    
    T unwrap_or(const T& default_val) const {
        return is_some() ? std::get<T>(value) : default_val;
    }
    
    template<typename F>
    T unwrap_or_else(F&& f) const {
        return is_some() ? std::get<T>(value) : f();
    }
    
    T* operator->() {
        return is_some() ? &std::get<T>(value) : nullptr;
    }
    
    const T* operator->() const {
        return is_some() ? &std::get<T>(value) : nullptr;
    }
    
    // Functional methods
    template<typename F>
    auto map(F&& f) const -> Option<decltype(f(std::declval<T>()))> {
        using RetType = decltype(f(std::declval<T>()));
        if (is_some()) {
            return Option<RetType>::Some(f(std::get<T>(value)));
        }
        return Option<RetType>::None();
    }
    
    template<typename F>
    auto and_then(F&& f) const -> decltype(f(std::declval<T>())) {
        if (is_some()) {
            return f(std::get<T>(value));
        }
        using RetType = decltype(f(std::declval<T>()));
        return RetType::None();
    }
    
    template<typename F>
    Option<T> filter(F&& predicate) const {
        if (is_some() && predicate(std::get<T>(value))) {
            return *this;
        }
        return Option<T>::None();
    }
};

// Result implementation
template<typename T, typename E>
class Result {
private:
    using OkStorage = std::conditional_t<std::is_reference_v<T>,
                                         std::reference_wrapper<std::remove_reference_t<T>>,
                                         T>;
    using ErrStorage = std::conditional_t<std::is_reference_v<E>,
                                          std::reference_wrapper<std::remove_reference_t<E>>,
                                          E>;

    std::variant<OkStorage, ErrStorage> value;
    bool is_ok_value;

    using OkRef = std::add_lvalue_reference_t<std::remove_reference_t<T>>;
    using OkConstRef = std::add_lvalue_reference_t<const std::remove_reference_t<T>>;
    using ErrRef = std::add_lvalue_reference_t<std::remove_reference_t<E>>;
    using ErrConstRef = std::add_lvalue_reference_t<const std::remove_reference_t<E>>;

    OkRef ok_ref() {
        if constexpr (std::is_reference_v<T>) {
            return std::get<OkStorage>(value).get();
        } else {
            return std::get<OkStorage>(value);
        }
    }

    OkConstRef ok_ref() const {
        if constexpr (std::is_reference_v<T>) {
            return std::get<OkStorage>(value).get();
        } else {
            return std::get<OkStorage>(value);
        }
    }

    ErrRef err_ref() {
        if constexpr (std::is_reference_v<E>) {
            return std::get<ErrStorage>(value).get();
        } else {
            return std::get<ErrStorage>(value);
        }
    }

    ErrConstRef err_ref() const {
        if constexpr (std::is_reference_v<E>) {
            return std::get<ErrStorage>(value).get();
        } else {
            return std::get<ErrStorage>(value);
        }
    }

public:
    // Constructors for success values
    template<typename U, typename = std::enable_if_t<!std::is_same_v<std::decay_t<U>, Result> &&
                                                     std::is_constructible_v<OkStorage, U>>>
    Result(U&& ok_val, std::true_type)
        : value(std::in_place_index<0>, std::forward<U>(ok_val)), is_ok_value(true) {}

    // Constructors for error values
    template<typename U, typename = std::enable_if_t<!std::is_same_v<std::decay_t<U>, Result> &&
                                                     std::is_constructible_v<ErrStorage, U>>>
    Result(U&& err_val, std::false_type)
        : value(std::in_place_index<1>, std::forward<U>(err_val)), is_ok_value(false) {}

    // Static factory methods
    template<typename U = T>
    static Result<T, E> Ok(U&& val) {
        return Result<T, E>(std::forward<U>(val), std::true_type{});
    }

    // Special overload for Unit type
    static Result<Unit, E> Ok() {
        return Result<Unit, E>(Unit{}, std::true_type{});
    }

    template<typename U = E>
    static Result<T, E> Err(U&& err) {
        return Result<T, E>(std::forward<U>(err), std::false_type{});
    }

    // Query methods
    bool is_ok() const {
        return is_ok_value;
    }

    bool is_err() const {
        return !is_ok_value;
    }

    // Access methods
    OkRef unwrap() {
        if (is_err()) {
            std::println("try to unwarp an err value");
            throw std::runtime_error("Called `Result::unwrap()` on an `Err` value");
        }
        return ok_ref();
    }

    OkConstRef unwrap() const {
        if (is_err()) {
            std::println("try to unwarp an err value");
            throw std::runtime_error("Called `Result::unwrap()` on an `Err` value");
        }
        return ok_ref();
    }

    ErrRef unwrap_err() {
        if (is_ok()) {
            throw std::runtime_error("called `Result::unwrap_err()` on an `Ok` value");
        }
        return err_ref();
    }

    ErrConstRef unwrap_err() const {
        if (is_ok()) {
            throw std::runtime_error("called `Result::unwrap_err()` on an `Ok` value");
        }
        return err_ref();
    }

    std::remove_reference_t<T> unwrap_or(const std::remove_reference_t<T>& default_val) const {
        return is_ok() ? static_cast<std::remove_reference_t<T>>(ok_ref()) : default_val;
    }

    template<typename F>
    std::remove_reference_t<T> unwrap_or_else(F&& f) const {
        if (is_ok()) {
            return static_cast<std::remove_reference_t<T>>(ok_ref());
        }
        return f(err_ref());
    }

    // Functional methods
    template<typename F>
    auto map(F&& f) const -> Result<decltype(f(std::declval<T>())), E> {
        using RetType = decltype(f(std::declval<T>()));
        if (is_ok()) {
            return Result<RetType, E>::Ok(f(ok_ref()));
        }
        return Result<RetType, E>::Err(err_ref());
    }

    template<typename F>
    auto map_err(F&& f) const -> Result<T, decltype(f(std::declval<E>()))> {
        using ErrType = decltype(f(std::declval<E>()));
        if (is_err()) {
            return Result<T, ErrType>::Err(f(err_ref()));
        }
        return Result<T, ErrType>::Ok(ok_ref());
    }

    template<typename F>
    auto and_then(F&& f) const -> decltype(f(std::declval<T>())) {
        using RetType = decltype(f(std::declval<T>()));
        if (is_ok()) {
            return f(ok_ref());
        }
        return RetType::Err(err_ref());
    }

    Option<std::remove_reference_t<T>> ok() const {
        if (is_ok()) {
            return Option<std::remove_reference_t<T>>::Some(ok_ref());
        }
        return Option<std::remove_reference_t<T>>::None();
    }

    Option<std::remove_reference_t<E>> err() const {
        if (is_err()) {
            return Option<std::remove_reference_t<E>>::Some(err_ref());
        }
        return Option<std::remove_reference_t<E>>::None();
    }
};

// Helper functions for creating Options and Results
template<typename T>
Option<T> Some(const T& val) {
    return Option<T>::Some(val);
}

template<typename T>
Option<T> Some(T&& val) {
    return Option<T>::Some(std::move(val));
}

template<typename T>
Option<T> None() {
    return Option<T>::None();
}

// Smart Ok/Err helpers with automatic type deduction
template<typename E>
Result<Unit, E> Ok() {
    return Result<Unit, E>::Ok();
}

// Ok with value - only need to specify error type
template<typename E, typename T>
Result<std::decay_t<T>, E> Ok(T&& val) {
    return Result<std::decay_t<T>, E>::Ok(std::forward<T>(val));
}

// Err with value - only need to specify value type  
template<typename T, typename E>
Result<T, std::decay_t<E>> Err(E&& err) {
    return Result<T, std::decay_t<E>>::Err(std::forward<E>(err));
}
#endif
#endif
