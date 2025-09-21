/**
 * @file Io.hpp
 * @brief ShyAsm 使用的文件输入输出辅助函数。
 */

#include "common.hpp"

namespace Io{
    /**
     * @brief 从文件读取汇编源码。
     * @param path 需要读取的文件路径。
     * @return 成功返回文件内容，失败返回核心错误。
     */
    fn read_from_file(string path)->Result<string,CoreError>;

    /**
     * @brief 将内存数据写入目标文件。
     * @param path 输出文件路径。
     * @param memory 需要保存的内存对象。
     * @return 成功时返回 Unit，失败返回核心错误。
     */
    fn write_to_file(string path,unique_ptr<Memory> memory)->Result<Unit,CoreError>;
}
