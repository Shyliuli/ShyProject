#include "str_helper.hpp"

namespace str_helper {
    // 检查字符是否为空白字符(空格、制表符、换行符、回车符)
    bool is_whitespace(char c) {
        return c == ' ' || c == '\t' || c == '\n' || c == '\r';
    }
}