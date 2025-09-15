#include "address.hpp"

#define MEM_START 0x00100100
#define VRAM_START 0x100
#define IO_START 0x70
#define COMMAND_START 0x20
#define REG_START 0x00

// Address class implementation
Address::Address(u32 addr)
{
    // 32位，无需检测错误
    raw_addr = addr;
}

fn Address::type_to_string(Type t) -> std::string
{
    switch (t)
    {
    case Type::Mem:
        return "Mem";
    case Type::Reg:
        return "Reg";
    case Type::Command:
        return "Command";
    case Type::IO:
        return "IO";
    case Type::VRAM:
        return "VRAM";
    default:
        return "Unknown";
    }
}
fn Address::type() -> Result<Type, CoreError>
{
        if (raw_addr < 0x20)
        {
            return Ok<CoreError>(Type::Reg);
        }
        if (raw_addr < 0x70)
        {
            return Ok<CoreError>(Type::Command);
        }
        if (raw_addr < 0xFF)
        {
            return Ok<CoreError>(Type::IO);
        }
        if (raw_addr < 0x00100100)
        {
            return Ok<CoreError>(Type::VRAM);
        }
        // 32位地址，其他情况都是Mem类型
        return Ok<CoreError>(Type::Mem);
    }
fn Address::to_u32() -> Result<u32, CoreError>
{
        let type = this->type();
        if (type.is_err())
        {
            return Err<u32>(type.unwrap_err());
        }
        else
        {
            let t = type.unwrap();
            if (t == Type::Mem)
            {
                return Ok<CoreError>(raw_addr - MEM_START);
            }
            if (t == Type::Reg)
            {
                return Ok<CoreError>(raw_addr - REG_START);
            }
            if (t == Type::Command)
            {
                return Ok<CoreError>(raw_addr - COMMAND_START);
            }
            if (t == Type::IO)
            {
                return Ok<CoreError>(raw_addr - IO_START);
            }
            if (t == Type::VRAM)
            {
                return Ok<CoreError>(raw_addr - VRAM_START);
            }
            return Err<u32>(CoreError(InvalidType{
                .message = "Invalid type",
                .type = type_to_string(t)}));
        }
    }
fn Address::to_u32_with_check(Type expected_type, const std::string &context) -> Result<u32, CoreError>
{
        let type_result = this->type();
        if (type_result.is_err())
        {
            return Err<u32>(type_result.unwrap_err());
        }

        let t = type_result.unwrap();
        if (t != expected_type)
        {
            return Err<u32>(CoreError(InvalidType{
                .message = "Invalid address type! in " + context,
                .type = type_to_string(t)}));
        }

        return this->to_u32();
    }