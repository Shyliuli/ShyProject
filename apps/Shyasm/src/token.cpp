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
    return Result<u32, CoreError>::Err(CoreError{AllocError{"Token::to_u32 not implemented"}});
}

fn Token::tokenizer() -> Result<Tokenizer, CoreError> {
    return Result<Tokenizer, CoreError>::Err(CoreError{AllocError{"Token::tokenizer not implemented"}});
}
