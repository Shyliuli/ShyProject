#pragma once
#include "common.hpp"
#include "CoreError.hpp"

class Address
{
public:
    enum class Type
    {
        Mem,
        Reg,
        Command,
        IO,
        VRAM
    };

    fn static type_to_string(Type t)->std::string;

    void *operator new(size_t) = delete;
    void *operator new[](size_t) = delete;

    Address(u32 addr);

    fn type() -> Result<Type, CoreError>;
    fn to_u32() -> Result<u32, CoreError>;
    fn to_u32_with_check(Type expected_type, const std::string &context) -> Result<u32, CoreError>;

private:
    u32 raw_addr;
};