#ifndef RUSTIC_H
#define RUSTIC_H
/**
 *  @file  rustic.hpp
 *  @brief Rustic - 让 C++ 拥有 Rust 般安全体验的轻量级工具头
 *
 *  1. 简洁而明确的类型别名
 *     - u8 / u16 / u32 / u64   8/16/32/64 位无符号整数
 *     - i8 / i16 / i32 / i64   8/16/32/64 位有符号整数
 *     - f32 / f64              单/双精度浮点数
 *     - usize / isize          与指针同宽的无/有符号整数
 *
 *  2. 声明风格的语法糖（需定义 ENABLE_RS_KEYWORD）
 *     - fn  → 展开为 auto，用于函数声明
 *       例：fn add(int a, int b) -> int { ... }
 *     - let → 展开为 const auto，用于定义不可变变量
 *       例：let x = 42;
 *
 *  3. 空值与错误处理的显式化封装（需定义 ENABLE_RS_ERROR）
 *     - Option<T>   安全的“可有可无”值
 *       - Some(v) / None()  构造
 *       - is_some() / is_none()
 *       - unwrap() / unwrap_or(def) / map(...)
 *     - Result<T,E> 安全的“可能失败”值
 *       - Ok(v) / Err(e)    构造
 *       - is_ok() / is_err()
 *       - unwrap() / unwrap_err() / map(...) / and_then(...)
 *     全局辅助：Some(...), None(), Ok(...), Err(...)
 *
 *  使用示例：
 *  @code
 *  fn divide(i32 a, i32 b) -> Result<i32, std::string> {
 *      if (b == 0) return Err("divide by zero");
 *      return Ok(a / b);
 *  }
 *
 *  let r = divide(10, 0);
 *  if (r.is_err()) std::cerr << r.unwrap_err();
 *  @endcode
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
template<typename T>
using Vec = std::vector<T>;
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
