#pragma once
#include "common.hpp"
#include "str_helper.hpp"
#include <unordered_map>
#include <sstream>

// 类型别名，简化长类型名
using string = std::string;
using define_map_t = std::unordered_map<string, string>;

//示例：

// ___DEFINE___
// SP sp              // 定义栈指针别名
// PI 314159          // 定义常量PI
// ___DATA___
// // 数据初始化支持多种格式
// //这里相当于往堆空间初始化全局变量
// 0x00210000 "Hello!" // 字符串
// 0x00200000 'A'      // 单字符ASCII码
// 0x00200001 12345678 // 32位立即数
// 0x00200002 {111, 222, 114514}  // 数组初始化
// ___CODE___
// setn sp 0x00FFFFFF // 手动初始化栈指针
// setn 1x 1          // 将寄存器1x设置为1
// .start             // 定义标签，对应下一条指令地址(0x01000006)
// addn 0x00200001 1  // 地址0x00200001的值加1
// outaasc 0x00200000 // 输出ASCII字符
// outn PI            // 输出宏定义的常量
// addn 1x 1          // 寄存器1x加1
// sman 1x 10         // 比较1x是否小于10
// jmpn .start        // 如果条件成立，跳转到标签.start


class first_scanner {
private:
    string codes;
    first_scanner(string codes);

    // 私有辅助函数
    // 解析___DEFINE___区域，构建宏定义映射表
    fn parse_define_map(const string& code) -> define_map_t;
    // 在指定字符串段中进行宏替换
    fn replace_macros_in_section(string& section, const define_map_t& define_map) -> void;
    // 对整个代码的___DATA___和___CODE___区域应用宏替换
    fn apply_macro_replacements(const string& code, const define_map_t& define_map) -> string;

public:
    static fn create(string codes)->Result<unique_ptr<first_scanner>,CoreError>;
    //处理注释
    fn comment_processer()-> first_scanner&;
    //处理define
    fn define_processer()-> first_scanner&;
    //获取处理后的代码字符串
    fn to_str() const -> string;


};