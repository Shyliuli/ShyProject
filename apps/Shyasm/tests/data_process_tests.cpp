// DataProcess unit tests
#include "doctest.h"
#include "DataProcess.hpp"

TEST_CASE("DataProcess::create 返回实例") {
    auto r = DataProcess::create("___DATA___\n");
    CHECK(r.is_ok());
}

TEST_CASE("DataProcess::process 对空 DATA 段成功处理") {
    auto r = DataProcess::create("___DATA___\n");
    REQUIRE(r.is_ok());
    auto dp = std::move(r.unwrap());
    auto pr = dp->process();
    CHECK(pr.is_ok());
}

TEST_CASE("DataProcess::process 对非法行返回 Err") {
    const char* src =
        "___DATA___\n"
        "0xZZZ 1\n"; // invalid address format
    auto r = DataProcess::create(src);
    REQUIRE(r.is_ok());
    auto dp = std::move(r.unwrap());
    auto pr = dp->process();
    CHECK(pr.is_err());
}

