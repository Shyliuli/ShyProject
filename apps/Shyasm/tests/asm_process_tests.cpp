// AsmProcess tests (high-level orchestration)
#include "doctest.h"
#include "AsmProcess.hpp"

TEST_CASE("AsmProcess::create 使用内存成功返回实例") {
    const char* src =
        "___CODE___\n"
        "setn 1x 1\n";

    auto mem_res = Memory::create();
    REQUIRE(mem_res.is_ok());

    auto asm_res = AsmProcess::create(src, std::move(mem_res.unwrap()));
    CHECK(asm_res.is_ok());
}

TEST_CASE("AsmProcess 对最小程序的 process() 与 bin() 成功") {
    const char* src =
        "___DEFINE___\n"
        "SP sp\n"
        "___CODE___\n"
        "setn sp 0x00FFFFFF\n";

    auto mem_res = Memory::create();
    REQUIRE(mem_res.is_ok());
    auto proc_res = AsmProcess::create(src, std::move(mem_res.unwrap()));
    REQUIRE(proc_res.is_ok());
    auto proc = std::move(proc_res.unwrap());

    auto pr = proc->process();
    CHECK(pr.is_ok());

    auto bin = proc->bin();
    CHECK(bin.is_ok());
}

TEST_CASE("AsmProcess::create 为空内存返回 Err") {
    auto asm_res = AsmProcess::create("___CODE___\n", unique_ptr<Memory>{});
    CHECK(asm_res.is_err());
}

TEST_CASE("AsmProcess::process 将 has_processed 置为 true") {
    const char* src =
        "___DEFINE___\n"
        "SP sp\n"
        "___CODE___\n"
        "setn sp 1\n";
    auto mem_res = Memory::create();
    REQUIRE(mem_res.is_ok());
    auto proc_res = AsmProcess::create(src, std::move(mem_res.unwrap()));
    REQUIRE(proc_res.is_ok());
    auto proc = std::move(proc_res.unwrap());
    auto r = proc->process();
    CHECK(r.is_ok());
    CHECK(proc->has_processed == true);
}
