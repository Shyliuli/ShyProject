#include "first_scanner.hpp"

// 构造函数：初始化代码字符串
first_scanner::first_scanner(string codes)
{
    this->codes = codes;
}

// 静态工厂方法：创建 first_scanner 实例
fn first_scanner::create(string codes) -> Result<unique_ptr<first_scanner>, CoreError>
{
    auto fs = unique_ptr<first_scanner>(new (std::nothrow) first_scanner(codes));
    if (fs == nullptr)
    {
        return Err<unique_ptr<first_scanner>>(
            CoreError(
                AllocError("first_scanner alloc error")));
    }
    return Ok<CoreError>(std::move(fs));
}
fn first_scanner::comment_processer() -> first_scanner&
{
    string &code = this->codes; // 直接修改当前实例的代码
    size_t pos = 0;

    while (pos < code.length()) {
        // 检查是否遇到行注释 //
        if (pos < code.length() - 1 && code[pos] == '/' && code[pos + 1] == '/') {
            // 找到行尾，删除从//到行尾的所有内容
            size_t line_end = code.find('\n', pos);
            if (line_end == string::npos) {
                // 如果没有找到换行符，说明注释到文件末尾
                code.erase(pos);
                break;
            } else {
                // 删除从//到换行符之前的内容，保留换行符
                code.erase(pos, line_end - pos);
            }
            continue; // 重新检查当前位置
        }

        // 检查是否遇到块注释 /*
        if (pos < code.length() - 1 && code[pos] == '/' && code[pos + 1] == '*') {
            // 找到块注释结束标记 */
            size_t comment_end = code.find("*/", pos + 2);
            if (comment_end == string::npos) {
                // 如果没有找到结束标记，删除从/*到文件末尾的所有内容
                code.erase(pos);
                break;
            } else {
                // 删除从/*到*/的所有内容（包括*/）
                code.erase(pos, comment_end + 2 - pos);
            }
            continue; // 重新检查当前位置
        }

        pos++; // 移动到下一个字符
    }

    // 返回当前实例的引用，支持链式调用
    return *this;
}
// 宏定义处理器：解析 ___DEFINE___ 区域并进行宏替换
fn first_scanner::define_processer() -> first_scanner&
{
    let code = this->codes;
    let define_start = code.find("___DEFINE___");
    // 到___DATA___ / ___CODE___ 结束
    let define_end = std::min(
        code.find("___DATA___"), code.find("___CODE___"));
    if (define_start == string::npos || define_end == string::npos)
    {
        // 没有DEFINE区域，直接返回当前实例的引用
        return *this;
    }

    // 解析宏定义映射表
    auto define_map = parse_define_map(code);

    // 应用宏替换，直接修改当前实例的codes
    this->codes = apply_macro_replacements(code, define_map);

    // 返回当前实例的引用，支持链式调用
    return *this;
}

// 解析___DEFINE___区域，构建宏定义映射表
fn first_scanner::parse_define_map(const string &code) -> define_map_t
{
    // 使用unordered_map是因为哈希表查找效率O(1)，比线性查找更优
    define_map_t define_map;

    // 查找区域边界
    u32 define_start = code.find("___DEFINE___");
    u32 data_pos = code.find("___DATA___");
    u32 code_pos = code.find("___CODE___");

    // 计算define区域的结束位置：取最近的边界
    u32 define_end = std::min(data_pos, code_pos);

    if (define_start == string::npos || define_end == string::npos)
    {
        return define_map; // 返回空映射表
    }

    // 简单的状态机，用于交替读取名称和值
    enum parse_state
    {
        NEED_NAME,
        NEED_VALUE
    };
    parse_state current_state = NEED_NAME;

    // 提取define区域内容，跳过"___DEFINE___"标记(12个字符)
    string define_section = code.substr(define_start + 12, define_end - define_start - 12);

    // 用简单的字符串查找方法
    size_t pos = 0;
    string current_name;

    while (pos < define_section.length())
    {
        // 跳过空白字符：找到下一个非空白字符的位置
        while (pos < define_section.length() && str_helper::is_whitespace(define_section[pos]))
        {
            pos++;
        }

        // 到达字符串末尾
        if (pos >= define_section.length())
            break;

        // 找到下一个空白字符的位置，确定token的结束
        size_t token_end = pos;
        while (token_end < define_section.length() && !str_helper::is_whitespace(define_section[token_end]))
        {
            token_end++;
        }

        // 提取token
        string token = define_section.substr(pos, token_end - pos);
        pos = token_end; // 移动到下一个位置

        if (token.empty())
            continue; // 跳过空token

        // 状态机：交替读取宏名称和宏值
        if (current_state == NEED_NAME)
        {
            current_name = token; // 保存宏名称
            current_state = NEED_VALUE;
        }
        else if (current_state == NEED_VALUE)
        {
            define_map[current_name] = token; // 存储 名称->值 的映射
            current_state = NEED_NAME;
        }
    }

    return define_map;
}

// 在指定字符串段中进行宏替换
fn first_scanner::replace_macros_in_section(string &section, const define_map_t &define_map) -> void
{
   
    for (const auto &define_pair : define_map)
    // 遍历宏定义映射表
    {
        const string &macro_name = define_pair.first;   // 宏名称
        const string &macro_value = define_pair.second; // 宏值

        size_t pos = 0;
        // 在字符串中查找所有宏名称的出现位置
        while ((pos = section.find(macro_name, pos)) != string::npos)
        {
            // 检查单词边界，避免误替换（如SP不应该匹配SPAC）
            // 检查前面是否为字母、数字或下划线
            bool is_word_start = (pos == 0) ||
                                 !(isalnum(section[pos - 1]) || section[pos - 1] == '_');

            // 检查后面是否为字母、数字或下划线
            bool is_word_end = (pos + macro_name.length() >= section.length()) ||
                               !(isalnum(section[pos + macro_name.length()]) || section[pos + macro_name.length()] == '_');

            if (is_word_start && is_word_end)
            {
                // 确实是完整单词，进行替换
                section.replace(pos, macro_name.length(), macro_value);
                pos += macro_value.length(); // 跳过已替换的内容
            }
            else
            {
                pos += 1; // 继续搜索下一个位置
            }
        }
    }
}

// 对整个代码的___DATA___和___CODE___区域应用宏替换
fn first_scanner::apply_macro_replacements(const string &code, const define_map_t &define_map) -> string
{
    string processed_code = code; // 复制原始代码，避免修改原始数据

    // 查找各个区域的起始位置
    size_t data_start = processed_code.find("___DATA___");
    size_t code_start = processed_code.find("___CODE___");

    // 处理___DATA___区域（如果存在）
    if (data_start != string::npos)
    {
        // 计算DATA区域的结束位置：到CODE区域开始或字符串末尾
        size_t data_end = (code_start != string::npos) ? code_start : processed_code.length();

        // 提取DATA区域的字符串
        string data_section = processed_code.substr(data_start, data_end - data_start);

        // 在DATA区域中进行宏替换
        replace_macros_in_section(data_section, define_map);

        // 将替换后的内容放回原字符串
        processed_code.replace(data_start, data_end - data_start, data_section);
    }

    // 处理___CODE___区域（如果存在）
    if (code_start != string::npos)
    {
        // CODE区域从标记开始到字符串末尾
        string code_section = processed_code.substr(code_start);

        // 在CODE区域中进行宏替换
        replace_macros_in_section(code_section, define_map);

        // 将替换后的内容放回原字符串
        processed_code.replace(code_start, processed_code.length() - code_start, code_section);
    }

    return processed_code;
}

// 获取处理后的代码字符串
fn first_scanner::to_str() const -> string {
    return codes; // 返回codes的拷贝
}
