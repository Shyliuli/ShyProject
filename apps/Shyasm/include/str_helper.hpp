/**
 * @file str_helper.hpp
 * @brief ShyAsm 各阶段共用的字符串工具函数。
 */

#include "common.hpp"

/**
 * @brief 汇编源码中不同部分的标识。
 */
enum class part_t{
    DEFINE, ///< 预处理定义段
    DATA, ///< 数据段
    CODE, ///< 代码段
};

/**
 * @brief 从源码中提取特定部分的片段。
 * @param input 原始汇编源码。
 * @param part 需要提取的部分类型。
 * @return 成功返回目标片段字符串，失败返回核心错误。
 */
fn get_part(string input,part_t part)->Result<string,CoreError>;
