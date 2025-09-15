#include "address.hpp"

// HUMAN: 地址空间分布定义
#define MEM_START 0x00100100     ///< 内存地址空间起始地址
#define VRAM_START 0x100         ///< 显存地址空间起始地址
#define IO_START 0x70           ///< IO端口地址空间起始地址
#define COMMAND_START 0x20      ///< 命令地址空间起始地址
#define REG_START 0x00          ///< 寄存器地址空间起始地址

// Address类构造函数实现
Address::Address(u32 addr)
{
    // HUMAN: 32位，无需检测错误
    raw_addr = addr;
}

// 地址类型到字符串转换实现
fn Address::type_to_string(Type t) -> std::string
{
    switch (t)
    {
    case Type::Mem:
        return "Mem";        // 内存地址
    case Type::Reg:
        return "Reg";        // 寄存器地址
    case Type::Command:
        return "Command";    // 命令地址
    case Type::IO:
        return "IO";         // IO端口地址
    case Type::VRAM:
        return "VRAM";       // 显存地址
    default:
        return "Unknown";    // 未知类型
    }
}

// 获取地址类型实现
fn Address::type() -> Result<Type, CoreError>
{
    // 根据地址范围判断地址类型
    if (raw_addr < 0x20)
    {
        return Ok<CoreError>(Type::Reg);      // 寄存器地址范围: 0x00-0x1F
    }
    if (raw_addr < 0x70)
    {
        return Ok<CoreError>(Type::Command);  // 命令地址范围: 0x20-0x6F
    }
    if (raw_addr < 0xFF)
    {
        return Ok<CoreError>(Type::IO);       // IO地址范围: 0x70-0xFE
    }
    if (raw_addr < 0x00100100)
    {
        return Ok<CoreError>(Type::VRAM);     // 显存地址范围: 0xFF-0x001000FF
    }
    // HUMAN: 32位地址，其他情况都是Mem类型
    return Ok<CoreError>(Type::Mem);          // 内存地址范围: 0x00100100及以上
}

// 转换为32位地址实现
fn Address::to_u32() -> Result<u32, CoreError>
{
    // 获取地址类型
    auto type_result = this->type();
    if (type_result.is_err())
    {
        return Err<u32>(type_result.unwrap_err());
    }

    auto t = type_result.unwrap();

    // 根据地址类型计算相对地址偏移
    if (t == Type::Mem)
    {
        return Ok<CoreError>(raw_addr - MEM_START);      // 内存相对地址
    }
    if (t == Type::Reg)
    {
        return Ok<CoreError>(raw_addr - REG_START);      // 寄存器相对地址
    }
    if (t == Type::Command)
    {
        return Ok<CoreError>(raw_addr - COMMAND_START);  // 命令相对地址
    }
    if (t == Type::IO)
    {
        return Ok<CoreError>(raw_addr - IO_START);       // IO相对地址
    }
    if (t == Type::VRAM)
    {
        return Ok<CoreError>(raw_addr - VRAM_START);     // 显存相对地址
    }

    // 不应该到达这里，返回类型错误
    return Err<u32>(CoreError(InvalidType{
        .message = "Invalid type",
        .type = type_to_string(t)
    }));
}

// 带类型检查的地址转换实现
fn Address::to_u32_with_check(Type expected_type, const std::string &context) -> Result<u32, CoreError>
{
    // 获取实际地址类型
    auto type_result = this->type();
    if (type_result.is_err())
    {
        return Err<u32>(type_result.unwrap_err());
    }

    auto t = type_result.unwrap();

    // 检查类型是否匹配
    if (t != expected_type)
    {
        return Err<u32>(CoreError(InvalidType{
            .message = "Invalid address type! in " + context,
            .type = type_to_string(t)
        }));
    }

    // 类型匹配，返回转换后的地址
    return this->to_u32();
}