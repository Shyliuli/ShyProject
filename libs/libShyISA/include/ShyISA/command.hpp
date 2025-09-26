#pragma once
#include "common.hpp"
#include <unordered_map>

class Command{
    u32 command_id;
public:
    Command(u32 id);
    static std::unordered_map<std::string,Command> command_map;
    static fn str_2_command(const std::string& str)->Result<Command,CoreError>;
};