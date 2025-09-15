#include "memory.hpp"
using std::unique_ptr;

// Memory class implementation
Memory::Memory()
{
    memory.fill(0);
}

fn Memory::create() -> Result<unique_ptr<Memory>, CoreError>
{
    auto mem = unique_ptr<Memory>(new (std::nothrow) Memory());
    if (mem == nullptr)
    {
        return Err<unique_ptr<Memory>>(CoreError(AllocError{
            .message = "Failed to allocate memory for memory class!"}));
    }
    return Ok<CoreError>(mem);
}

fn Memory::write(u32 val, Address addr) -> Result<Unit, CoreError>
{
    // 1. 调用第一个可能失败的操作
    return addr.to_u32_with_check(Address::Type::Mem, "Memory::write()")
        .and_then([this, val](u32 concrete_addr) -> Result<Unit, CoreError>
                  {
                   this->memory[concrete_addr] = val;
                   return Ok<CoreError>(Unit{}); });
}

fn Memory::read(Address addr) -> Result<u32, CoreError>
{
    return addr.to_u32_with_check(Address::Type::Mem, "Memory::read()")
        .and_then([this](u32 concrete_addr) -> Result<u32, CoreError>
                  { return Ok<CoreError>(this->memory[concrete_addr]); });
}