#pragma once
#include "common.hpp"
#include "address.hpp"
#include "CoreError.hpp"
#include <memory>
#include <array>

#define MEM_SIZE 0x01000000 // 16M，后续应该由sys大小减去保留空间算出

class Memory
{
public:
    fn static create()->Result<std::unique_ptr<Memory>, CoreError>;

    fn write(u32 val, Address addr) -> Result<Unit, CoreError>;
    fn read(Address addr) -> Result<u32, CoreError>;

private:
    std::array<u32, MEM_SIZE> memory;
    Memory();
};