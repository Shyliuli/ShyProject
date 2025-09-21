#include "ShyISA/memory.hpp"
#include <new>
using std::unique_ptr;

// Memory类构造函数实现
Memory::Memory()
{
    // 初始化所有内存为0
    memory.fill(0);
}

// 静态工厂方法实现
fn Memory::create() -> Result<unique_ptr<Memory>, CoreError>
{
    // 使用placement new和nothrow确保内存分配异常安全
    auto mem = unique_ptr<Memory>(new (std::nothrow) Memory());
    if (mem == nullptr)
    {
        return Err<unique_ptr<Memory>>(CoreError(AllocError{
            .message = "Failed to allocate memory for memory class!"}));
    }
    return Ok<CoreError>(std::move(mem));
}

// 写入内存值实现
fn Memory::write(u32 val, Address addr) -> Result<Unit, CoreError>
{
    // 验证地址类型并获取具体地址偏移
    return addr.to_u32_with_check(Address::Type::Mem, "Memory::write()")
        .and_then([this, val](u32 concrete_addr) -> Result<Unit, CoreError>
        {
            // 将值写入内存数组
            this->memory[concrete_addr] = val;
            // 返回成功标志
            return Ok<CoreError>(Unit{});
        });
}

// 读取内存值实现
fn Memory::read(Address addr) -> Result<u32, CoreError>
{
    // 验证地址类型并获取具体地址偏移
    return addr.to_u32_with_check(Address::Type::Mem, "Memory::read()")
        .and_then([this](u32 concrete_addr) -> Result<u32, CoreError>
        {
            // 从内存数组读取值并返回
            return Ok<CoreError>(this->memory[concrete_addr]);
        });
}
