#pragma once
#include <string>
#include <variant>
#include "rustic.hpp"

struct AllocError
{
    std::string message;
};
struct InvalidAddress
{
    std::string message;
    u32 raw_address;
};
struct InvalidType
{
    std::string message;
    std::string type;
};
using CoreError = std::variant<
    AllocError,
    InvalidAddress,
    InvalidType>;