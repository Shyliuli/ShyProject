#include <iostream>
#include "memory.hpp"
int main(int argc, char** argv) {
    auto memory = Memory::create();
    memory.unwrap_or_else([](CoreError err) {
        err.print();
    });
}
