#pragma once
#include "common.hpp"
#include "address.hpp"
#include <array>
#include <unordered_map>
/*让AI补了下注释，检察过没问题
 *这玩意写注释真准啊（*/
/**
 * @enum Reg_Mode
 * @brief 寄存器运行模式枚举
 *
 * 定义处理器的运行模式，影响图形和文本处理
 */
enum Reg_Mode{
    Text,     ///< 文本模式
    Graphic   ///< 图形模式
};

/**
 * @struct Reg_Status
 * @brief 寄存器状态结构体
 *
 * 包含处理器当前状态的各种标志和重要寄存器值
 */
struct Reg_Status{
    bool exit;        ///< 退出标志
    Reg_Mode mode;    ///< 当前运行模式
    bool tm_equal_1;  ///< 定时器是否等于1
    bool music_on;    ///< 音乐功能是否开启
    u32 ta1;          ///< 定时器地址1（中断处理程序）
    u32 ta2;          ///< 定时器地址2（返回地址）
    u32 m1, m2, m3, m4; ///< 音乐相关寄存器
};
/**
 * @class Reg
 * @brief 处理器寄存器管理类
 *
 * 管理ShyISA架构处理器的所有寄存器，包括通用寄存器和特殊功能寄存器。
 * 提供统一的寄存器访问接口，支持读写操作和状态查询。
 *
 * 寄存器映射:
 * - 0x00-0x0F: 16个通用寄存器
 * - 0x10: PC (程序计数器)
 * - 0x11: MD (模式寄存器)
 * - 0x12: SP (栈指针)
 * - 0x13: TM (定时器)
 * - 0x14-0x15: TA1, TA2 (定时器地址)
 * - 0x16-0x19: M1-M4 (音乐寄存器)
 * - 0x1A: RS (结果寄存器)
 * - 0x1B: EX (退出寄存器)
 * - 0x1C-0x1D: BLTS, BLTL (块传输寄存器)
 */
class Reg
{
private:
    /// 16个通用寄存器 (0x00-0x0F)
    std::array<u32, 16> gp_reg;

    /// 寄存器地址到内存地址的映射表，用于统一的寄存器访问
    const std::unordered_map<u32,u32*> reg_map;

    // 特殊功能寄存器
    u32 pc;          ///< 0x10: 程序计数器 (Program Counter)
    u32 md;          ///< 0x11: 模式寄存器 (Mode Register) - 0=文本模式, 非0=图形模式
    u32 sp;          ///< 0x12: 栈指针 (Stack Pointer)
    u32 tm;          ///< 0x13: 定时器 (Timer)
    u32 ta1;         ///< 0x14: 定时器地址1 (Timer Address 1) - 中断处理程序地址
    u32 ta2;         ///< 0x15: 定时器地址2 (Timer Address 2) - 中断返回地址
    u32 m1, m2, m3, m4; ///< 0x16-0x19: 音乐相关寄存器 (Music Registers)
    u32 rs;          ///< 0x1A: 结果寄存器 (Result Register)
    u32 ex;          ///< 0x1B: 退出寄存器 (Exit Register) - 非0表示程序应退出
    u32 blts;        ///< 0x1C: 块传输起始地址 (Block Transfer Start Address)
    u32 bltl;        ///< 0x1D: 块传输长度 (Block Transfer Length)

    /**
     * @brief 私有构造函数
     *
     * 初始化所有寄存器为0。使用私有构造函数强制通过create()工厂方法创建实例。
     */
    Reg();

public:
    /**
     * @brief 创建Reg实例的工厂方法
     * @return Result<std::unique_ptr<Reg>, CoreError> 成功时返回Reg实例的智能指针，失败时返回分配错误
     *
     * 使用RAII和智能指针管理内存，确保异常安全性。
     */
    static fn create() -> Result<std::unique_ptr<Reg>, CoreError>;

    /**
     * @brief 检查寄存器状态
     * @return Reg_Status 当前寄存器状态的快照
     *
     * 返回处理器的重要状态信息，包括：
     * - 是否需要退出
     * - 当前运行模式
     * - 定时器状态
     * - 音乐功能状态
     * - 相关寄存器值
     */
    fn check_once() -> Reg_Status;

    /**
     * @brief 读取寄存器值
     * @param addr 寄存器地址
     * @return Result<u32, CoreError> 成功时返回寄存器值，失败时返回错误信息
     *
     * 根据地址从对应寄存器读取32位值。地址会经过类型检查确保为有效的寄存器地址。
     */
    fn read(Address addr) -> Result<u32, CoreError>;

    /**
     * @brief 写入寄存器值
     * @param val 要写入的32位值
     * @param addr 目标寄存器地址
     * @return Result<Unit, CoreError> 成功时返回Unit，失败时返回错误信息
     *
     * 将指定值写入目标寄存器。地址会经过类型检查确保为有效的寄存器地址。
     */
    fn write(u32 val, Address addr) -> Result<Unit, CoreError>;
};