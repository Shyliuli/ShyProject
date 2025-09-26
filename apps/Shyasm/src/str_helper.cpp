#include "str_helper.hpp"

fn get_part(string input, part_t part) -> Result<string, CoreError> {
    (void)input;
    (void)part;
    return Result<string, CoreError>::Ok(string{});
}
fn is_whitespace(char c)->bool{
    return c==' '||c=='\t'||c=='\n';
}
fn is_whitespace_without_n(char c)->bool{
    return c==' '||c=='\t';
}