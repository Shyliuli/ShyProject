#include "tokenlizer.hpp"
#include <utility>

Token::Token(string raw, Type type)
    : token_type(type), raw_str(std::move(raw)) {}

fn Token::type() -> Type {
    return token_type;
}

fn Token::str() -> string {
    return raw_str;
}

fn Token::to_u32() -> Result<u32, CoreError> {
    switch (token_type)
    {
        case Type::CHAR:
        return Ok<CoreError>(static_cast<u32>(raw_str[0]));
        case Type::HEX:
        return Ok<CoreError>(static_cast<u32>(std::stoul(raw_str, nullptr, 16)));
        case Type::DEC:
        return Ok<CoreError>(static_cast<u32>(std::stoul(raw_str)));
        case Type::BIN:
        return Ok<CoreError>(static_cast<u32>(std::stoul(raw_str, nullptr, 2)));
        case Type::REG:
        return Reg::str2addr(raw_str)
            .and_then([](Address addr) -> Result<u32, CoreError> {
                return addr.to_u32();
            });
        default:
            let msg=std::format("Cannot convert {} to u32,type is {}",raw_str,type_to_string(token_type));
            return Err<u32>(CoreError(InvalidType{
                .message=msg,
                .type=type_to_string(token_type)
            }
            ));
    }
}

fn Token::tokenizer() -> Result<Tokenizer, CoreError> {
    return Result<Tokenizer, CoreError>::Err(CoreError{AllocError{"Token::tokenizer not implemented"}});
}
fn Token::type_to_string(Type type)->string{
        switch (type)
        {
        case Type::CHAR:
            return "char";
        case Type::HEX:
            return "hex";
        case Type::DEC:
            return "dec";
        case Type::BIN:
            return "bin";
        case Type::REG:
            return "reg";
        case Type::ARRAY:
            return "array";
        case Type::STRING:
            return "string";
        case Type::FLAG:
            return "flag";
        case Type::COMMAND:
            return "command";
        case Type::NEXT_LINE:
            return "next_line";
        case Type::ANY:
            return "any";
        case Type::LINE_COMMNET:
            return "line_comment";
        case Type::BLOCK_COMMENT_START:
            return "block_comment_start";
        case Type::BLOCK_COMMENT_END:
            return "block_comment_end";
        default:
            return "unknown";
        }
    }
