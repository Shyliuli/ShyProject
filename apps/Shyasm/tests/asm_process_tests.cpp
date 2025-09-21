// AsmProcess tests (high-level orchestration)
#include "doctest.h"
#include "AsmProcess.hpp"

TEST_CASE("AsmProcess::create returns instance with memory") {
    const char* src =
        "___CODE___\n"
        "setn 1x 1\n";

    auto mem_res = Memory::create();
    REQUIRE(mem_res.is_ok());

    auto asm_res = AsmProcess::create(src, std::move(mem_res.unwrap()));
    CHECK(asm_res.is_ok());
}

TEST_CASE("AsmProcess .process() and .bin() succeed for minimal program") {
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

TEST_CASE("AsmProcess::create returns Err for null memory") {
    auto asm_res = AsmProcess::create("___CODE___\n", unique_ptr<Memory>{});
    CHECK(asm_res.is_err());
}

TEST_CASE("AsmProcess::process marks has_processed true") {
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
