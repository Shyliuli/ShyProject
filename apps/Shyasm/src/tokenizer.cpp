#include "tokenlizer.hpp"
#include <new>
#include <utility>
// 前置声明各种 is_xxx 函数
fn is_next_line(char c) -> bool;
fn is_line_comment(const string &input, usize pos) -> bool;
fn is_block_comment_start(const string &input, usize pos) -> bool;
fn is_block_comment_end(const string &input, usize pos) -> bool;
fn is_char(const string &input, usize pos) -> bool;
fn is_hex(const string &input, usize& pos) -> bool;
fn is_bin(const string &input, usize& pos) -> bool;
fn is_dec(const string &input, usize& pos) -> bool;
fn is_reg(const string &input, usize& pos) -> bool;
fn is_array(const string &input, usize& pos) -> bool;
fn is_string(const string &input, usize& pos) -> bool;
fn is_flag(const string& input, usize& pos) -> bool;
fn is_command(const string& input, usize& pos) -> bool;
fn analize_next_token(std::string input, usize &pos) -> Token;
// LINE_COMMNET,        ///< 单行注释
// BLOCK_COMMENT_START, ///< 块注释起始符
// BLOCK_COMMENT_END,   ///< 块注释结束符
// CHAR,                ///< 字符字面量，例如 'A'
// HEX,                 ///< 十六进制数值，例如 0x114514 0X1919810
// DEC,                 ///< 十进制数值，例如 114514
// BIN,                 ///< 二进制数值，例如 111000b
// REG,                 ///< 寄存器标识符，例如 1x
// ARRAY,               ///< 数组字面量，例如 {'a',2,3}
// STRING,              ///< 字符串字面量，例如 "11a4b5c14"
// FLAG,                ///< 标志位，例如 .xxx
// COMMAND,             ///< 汇编指令，例如 adda
// NEXT_LINE,           ///< 换行符
// ANY,                 ///< 通配字符
// END_OF_FILE,         ///< 文件结束符

fn Tokenizer::create(string input) -> Result<unique_ptr<Tokenizer>, CoreError>
{
    auto tokenizer = unique_ptr<Tokenizer>(new (std::nothrow) Tokenizer(std::move(input)));
    if (tokenizer == nullptr)
    {
        return Err<unique_ptr<Tokenizer>>(CoreError(AllocError{
            .message = "Failed to allocate Tokenizer"}));
    }

    return Ok<CoreError>(std::move(tokenizer));
}

fn Tokenizer::get_token(u32 i) -> Result<Token &, CoreError>
{
    (void)i;
    return Result<Token &, CoreError>::Err(CoreError{AllocError{"Tokenizer::get_token not implemented"}});
}

fn Tokenizer::next() -> Result<Token &, CoreError>
{
    if(now>=tokens.size())
    {
        return Result<Token &, CoreError>::Err(CoreError{AllocError{"Tokenizer::next out of range"}});
    }
    now++;
    return Result<Token &, CoreError>::Ok(tokens[now-1]);



}

fn Tokenizer::reset_index() -> Result<Unit, CoreError>
{
    return Result<Unit, CoreError>::Ok();
}

fn Tokenizer::to_string() -> Result<std::string, CoreError>
{
    return Result<std::string, CoreError>::Ok(std::string{});
}

Tokenizer::Tokenizer(std::string input)
    : tokens{}, now(0)
{
    usize pos=0;
    auto token = analize_next_token(input,pos);
    while(token.type() != Token::Type::END_OF_FILE){
        tokens.push_back(token);
        token = analize_next_token(input,pos);
    }

}

fn analize_next_token(std::string input, usize &pos) -> Token
{
    while (pos < input.length() && is_whitespace_without_n(input[pos]))
    {
        pos++;
    } // 跳过空格,制表符等
    if (pos >= input.length())
    {
        return Token("", Token::Type::END_OF_FILE);
    }
    usize start = pos;
    // 进行匹配
    if (is_next_line(input[pos]))
    {
        pos++;
        return Token(
            input.substr(start, pos - start), Token::Type::NEXT_LINE);
    }
    if (is_line_comment(input, pos))
    {
        pos += 2;
        return Token(
            input.substr(start, pos - start), Token::Type::LINE_COMMNET);
    }
    if (is_block_comment_start(input, pos))
    {
        pos += 2;
        return Token(
            input.substr(start, pos - start), Token::Type::BLOCK_COMMENT_START);
    }
    if (is_block_comment_end(input, pos))
    {
        pos += 2;
        return Token(
            input.substr(start, pos - start), Token::Type::BLOCK_COMMENT_END);
    }
    if (is_char(input, pos))
    {
        pos += 3;
        return Token(
            input.substr(start, pos - start), Token::Type::CHAR);
    }
    if (is_hex(input, pos))
    {
        // 十六进制因为长度可变，传pos引用进去直接改
        // 如果不是，则不改

        return Token(
            input.substr(start, pos - start), Token::Type::HEX);
    }
    if (is_bin(input, pos))
    {
        // 二进制因为长度可变，传pos引用进去直接改
        // 如果不是，则不改

        return Token(
            input.substr(start, pos - start), Token::Type::BIN);
    }
    if (is_dec(input, pos))
    {
        // 十进制因为长度可变，传pos引用进去直接改
        // 如果不是，则不改

        return Token(
            input.substr(start, pos - start), Token::Type::DEC);
    }
    if (is_reg(input, pos)){
        // reg因为长度可变，传pos引用进去直接改
        // 如果不是，则不改
        return Token(
            input.substr(start, pos - start), Token::Type::REG);
    }
    if(is_array(input, pos)){
        // array因为长度可变，传pos引用进去直接改
        // 如果不是，则不改
    }
    if (is_string(input, pos))
    {
        return Token(
            input.substr(start, pos - start), Token::Type::STRING);
    }
    if (is_flag(input, pos))
    {
        return Token(
            input.substr(start, pos - start), Token::Type::FLAG);
    }
    if (is_command(input, pos))
    {
        return Token(
            input.substr(start, pos - start), Token::Type::COMMAND);
    }

    // 如果都不匹配，则返回ANY(直到空白字符之前截断)
    while (pos < input.length() && !is_whitespace(input[pos])) {
        pos++;
    }
    return Token(
        input.substr(start, pos - start), Token::Type::ANY);
}

// helpers
fn is_next_line(char c)->bool
{
    return c == '\n';
}
fn is_line_comment(const string &input, usize pos) -> bool
{
    if (pos + 1 >= input.length())
    {
        return false;
    }
    return input[pos] == '/' && input[pos + 1] == '/';
}
fn is_block_comment_start(const string &input, usize pos) -> bool
{
    if(pos + 1 >= input.length()){
        return false;
    }
    return input[pos] == '/' && input[pos + 1] == '*';
}
fn is_block_comment_end(const string &input, usize pos) -> bool
{
    if(pos + 1 >= input.length()){
        return false;
    }
    return input[pos] == '*' && input[pos + 1] == '/';
}
fn is_char(const string &input, usize pos) -> bool
{
    if (pos + 2 >= input.length())
    {
        return false;
    }
    return input[pos] == '\'' && input[pos + 2] == '\'';
}
fn is_hex(const string &input, usize& pos)->bool{
    static const string hex_char = "0123456789abcdefABCDEF";
    if(pos + 2 >= input.length()){
        return false;
    }
    if(input[pos] != '0' || (input[pos + 1] != 'x'|| input[pos + 1] != 'X')){
        return false;
    }
    
    auto tmp=pos + 1;
    while(!is_whitespace(input[tmp])){
        tmp++;
        if(tmp >= input.length()){
             break;
        }
        for(const auto &c : hex_char){
            if(input[tmp] != c){
                return false;
            }
        }
    }
    pos = tmp;
    return true;
}
fn is_bin(const string &input,usize& pos)->bool{
    if(input[pos] != '0'&& input[pos] != '1'){
        return false;
    }
    auto tmp=pos;
    while(!is_whitespace(input[tmp])){
        tmp++;
        if(tmp >= input.length()){
            return false;
        }
        if(input[tmp] != '0'&& input[tmp] != '1'){
            if(input[tmp] == 'b'){
                pos=tmp;
                return true;
            }
            return false;
        }
    }
    return false;
}
fn is_dec(const string &input,usize& pos)->bool{
    auto tmp=pos;
    while(!is_whitespace(input[tmp])){
        tmp++;
        if(tmp >= input.length()){
            break;
        }
        if(input[tmp] < '0' || input[tmp] > '9'){
            return false;
        }
    }
    pos=tmp;
    return true;
}
fn is_reg(const string &input,usize& pos)->bool{
    auto tmp=pos;
    while(!is_whitespace(input[tmp])){
        tmp++;
        if(tmp >= input.length()){
            break;
        }
    }
    auto r=Reg::str2addr(input.substr(pos,tmp-1-pos));//-1去空白
    if(r.is_err()){
        return false;
    }
    pos=tmp;
    return true;
}
fn is_array(const string &input,usize& pos)->bool{
    if(input[pos]!='{'){
        return false;
    }
    auto tmp=pos;
    while(!is_whitespace(input[tmp])){
        tmp++;
        if(tmp >= input.length()){
            return false;
        }
    }
    if(input[tmp-1]!='}'){
        return false;
    }
    pos=tmp;
    return true;
}
fn is_string(const string &input,usize& pos)->bool{
    if(input[pos]!='"'){
        return false;
    }
    auto tmp=pos;
    while(!is_whitespace(input[tmp])){
        tmp++;
        if(tmp >= input.length()){
            return false;
        }
    }
    if(input[tmp-1]!='"'){
        return false;
    }
    pos=tmp;
    return true;
}
fn is_flag(const string& input,usize& pos)->bool{
    if(input[pos]!='.'){
        return false;
    }
    auto tmp=pos;
    while(!is_whitespace(input[tmp])){
        tmp++;
        if(tmp >= input.length()){
            return false;
        }
    }
    pos=tmp;
    return true;
}
fn is_command(const string& input,usize& pos) -> bool{
    auto tmp=pos;
    while(!is_whitespace(input[tmp])){
        tmp++;
        if(tmp >= input.length()){
            return false;
        }
    }
    auto t=Command::str_2_command(input.substr(pos,tmp-1-pos));
    if(t.is_err()){
        return false;
    }
    pos=tmp;
    return true;
}
