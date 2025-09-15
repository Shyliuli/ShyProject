#include "reg.hpp"

// 私有构造函数实现 - 初始化所有寄存器为0
Reg::Reg() : reg_map({
    {0x00, &gp_reg[0]},
    {0x01, &gp_reg[1]},
    {0x02, &gp_reg[2]},
    {0x03, &gp_reg[3]},
    {0x04, &gp_reg[4]},
    {0x05, &gp_reg[5]},
    {0x06, &gp_reg[6]},
    {0x07, &gp_reg[7]},
    {0x08, &gp_reg[8]},
    {0x09, &gp_reg[9]},
    {0x0A, &gp_reg[10]},
    {0x0B, &gp_reg[11]},
    {0x0C, &gp_reg[12]},
    {0x0D, &gp_reg[13]},
    {0x0E, &gp_reg[14]},
    {0x0F, &gp_reg[15]},
    {0x10, &pc},
    {0x11, &md},
    {0x12, &sp},
    {0x13, &tm},
    {0x14, &ta1},
    {0x15, &ta2},
    {0x16, &m1},
    {0x17, &m2},
    {0x18, &m3},
    {0x19, &m4},
    {0x1A, &rs},
    {0x1B, &ex},
    {0x1C, &blts},
    {0x1D, &bltl}
})
{
    // 初始化通用寄存器数组
    gp_reg.fill(0);

    // 初始化所有特殊功能寄存器
    pc = md = sp = tm = ta1 = ta2 = m1 = m2 = m3 = m4 = rs = ex = blts = bltl = 0;
}

// 静态工厂方法实现
fn Reg::create() -> Result<std::unique_ptr<Reg>, CoreError>
{
    // 使用placement new和nothrow确保内存分配异常安全
    auto reg = std::unique_ptr<Reg>(new(std::nothrow) Reg());

    if (reg == nullptr) {
        return Err<std::unique_ptr<Reg>>(CoreError(AllocError{"AllocError in Reg::create()"}));
    }

    return Ok<CoreError>(std::move(reg));
}

// 检查寄存器状态实现
fn Reg::check_once() -> Reg_Status
{
    // 检查退出标志 - 非0表示程序应退出
    auto exit = ex != 0;

    // 根据模式寄存器判断运行模式 - 0为文本模式，非0为图形模式
    auto mode = md == 0 ? Reg_Mode::Text : Reg_Mode::Graphic;

    // 检查定时器是否等于1 - 用于定时器中断检测
    auto tm_equal_1 = tm == 1;

    // 检查音乐功能是否开启 - 任一音乐寄存器非0即为开启
    auto music_on = m1 != 0 || m2 != 0 || m3 != 0 || m4 != 0;

    // 返回状态结构体
    return Reg_Status{
        exit,
        mode,
        tm_equal_1,
        music_on,
        ta1,
        ta2,
        m1, m2, m3, m4
    };
}

// 读取寄存器值实现
fn Reg::read(Address addr) -> Result<u32, CoreError>
{
    return addr.to_u32_with_check(Address::Type::Reg, "Reg::read()")
        .and_then([this](u32 raw_addr) -> Result<u32, CoreError>
        {
            // 在寄存器映射表中查找目标地址
            auto it = reg_map.find(raw_addr);

            if (it == reg_map.end()) {
                // 地址不存在，返回寄存器未找到错误
                return Err<u32>(CoreError(RegNotFind{
                    .message = "Invalid register address",
                    .reg_addr = raw_addr
                }));
            }

            // 返回寄存器值
            return Ok<CoreError>(*it->second);
        });
}

// 写入寄存器值实现
fn Reg::write(u32 val, Address addr) -> Result<Unit, CoreError>
{
    return addr.to_u32_with_check(Address::Type::Reg, "Reg::write()")
        .and_then([this, val](u32 raw_addr) -> Result<Unit, CoreError>
        {
            // 在寄存器映射表中查找目标地址
            auto it = reg_map.find(raw_addr);

            if (it == reg_map.end()) {
                // 地址不存在，返回寄存器未找到错误
                return Err<Unit>(CoreError(RegNotFind{
                    .message = "Invalid register address",
                    .reg_addr = raw_addr
                }));
            }

            // 写入寄存器值
            *it->second = val;

            // 返回成功标志
            return Ok<CoreError>(Unit{});
        });
}