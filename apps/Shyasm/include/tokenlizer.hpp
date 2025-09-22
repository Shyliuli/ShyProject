/**
 * @file tokenlizer.hpp
 * @brief 将 ShyAsm 源码切分为词法单元的接口定义。
 */

#pragma once
#include "common.hpp"

/**
 * @brief ShyAsm 词法分析阶段产生的单个Token。
 *
 * 每个Token保存原始内容以及分类结果，供后续流程使用。
 */
class Tokenizer;

class Token
{
public:
    /**
     * @brief 汇编源代码中识别到的Token类型。
     */
    enum class Type
    {
        LINE_COMMNET,        ///< 单行注释
        BLOCK_COMMENT_START, ///< 块注释起始符
        BLOCK_COMMENT_END,   ///< 块注释结束符
        CHAR,                ///< 字符字面量，例如 'A'
        HEX,                 ///< 十六进制数值，例如 0x114514 0X1919810
        DEC,                 ///< 十进制数值，例如 114514
        BIN,                 ///< 二进制数值，例如 111000b
        REG,                 ///< 寄存器标识符，例如 1x
        ARRAY,               ///< 数组字面量，例如 {'a',2,3}
        STRING,              ///< 字符串字面量，例如 "11a4b5c14"
        FLAG,                ///< 标志位，例如 .xxx
        COMMAND,             ///< 汇编指令，例如 adda
        NEXT_LINE,           ///< 换行符
        ANY,                 ///< 通配字符
    };

private:
    friend class Tokenizer;
    /// Token 的分类类型。
    Type token_type;
    /// 从源代码中提取的原始字符串。
    string raw_str;

public:
    /**
     * @brief 使用指定原始字符串和类型构造Token。
     * @param raw 捕获的源文本。
     * @param type Token的类型标签。
     */
    Token(string raw, Type type);
    /**
     * @brief 获取Token的类型。
     * @return 标记当前Token所属的分类。
     */
    fn type() -> Type;

    /**
     * @brief 获取Token的原始字符串。
     * @return 未经处理的源代码片段。
     */
    fn str() -> string;

    /**
     * @brief 将Token解析为 32 位无符号整数。
     * @return 成功时返回数值，失败时返回对应的核心错误。
     * @note 仅支持十进制、十六进制、二进制、单字符。
     */
    fn to_u32() -> Result<u32, CoreError>;

    /**
     * @brief 对数组和字符串进行解析。
     * @return 成功时返回新的 tokenizer 实例，否则返回词法错误。
     */
    fn tokenizer() -> Result<Tokenizer, CoreError>;
};

/**
 * @brief 负责将源码分割为Token序列的状态化 tokenizer。
 */
class Tokenizer
{
    friend class Token;

public:
    /**
     * @brief 创建 tokenizer
     * @param input 原始汇编源码。
     * @return 成功时返回拥有权指针，失败返回核心错误。
     */
    fn static create(string input)->Result<unique_ptr<Tokenizer>, CoreError>;

    /**
     * @brief 按索引访问指定的Token。
     * @param i 目标Token的索引。
     * @return 成功时返回对Token引用，失败返回错误。
     */
    fn get_token(u32 i) -> Result<Token &, CoreError>;

    /**
     * @brief 返回当前索引处的Token，并将索引推进。
     * @return 成功时返回下一个Token，失败返回错误。
     */
    fn next() -> Result<Token &, CoreError>;

    /**
     * @brief 将内部游标重置到开始位置。
     * @return 成功返回 Unit，失败返回错误。
     */
    fn reset_index() -> Result<Unit, CoreError>;

    /**
     * @brief 将当前Token序列重新串联成字符串。
     * @return 成功返回序列化后的文本，失败返回错误。
     */
    fn to_string() -> Result<std::string, CoreError>;

private:
    /**
     * @brief 使用原始输入构造 tokenizer 并保存词法结果。
     * @param input 原始汇编源码。
     */
    Tokenizer(std::string input);

    /// 词法分析得到的Token列表。
    std::vector<Token> tokens;
    /// 当前遍历位置。
    u32 now;
};
