#include "tokenlizer.hpp"
#include <new>
#include <utility>

fn Tokenizer::create(string input) -> Result<unique_ptr<Tokenizer>, CoreError> {
    auto tokenizer = unique_ptr<Tokenizer>(new (std::nothrow) Tokenizer(std::move(input)));
    if (tokenizer == nullptr) {
        return Err<unique_ptr<Tokenizer>>(CoreError(AllocError{
            .message = "Failed to allocate Tokenizer"}));
    }
    return Ok<CoreError>(std::move(tokenizer));
}

fn Tokenizer::get_token(u32 i) -> Result<Token&, CoreError> {
    (void)i;
    return Result<Token&, CoreError>::Err(CoreError{AllocError{"Tokenizer::get_token not implemented"}});
}

fn Tokenizer::next() -> Result<Token&, CoreError> {
    return Result<Token&, CoreError>::Err(CoreError{AllocError{"Tokenizer::next not implemented"}});
}

fn Tokenizer::reset_index() -> Result<Unit, CoreError> {
    return Result<Unit, CoreError>::Ok();
}

fn Tokenizer::to_string() -> Result<std::string, CoreError> {
    return Result<std::string, CoreError>::Ok(std::string{});
}

Tokenizer::Tokenizer(std::string input)
    : tokens{}, now(0) {
    (void)input;
}
