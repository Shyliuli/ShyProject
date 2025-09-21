// String helper unit tests
#include "doctest.h"
#include "str_helper.hpp"

TEST_CASE("get_part extracts DEFINE section") {
    const char* src =
        "___DEFINE___\n"
        "SP sp\n"
        "PI 1\n"
        "___DATA___\n"
        "0x00200000 1\n"
        "___CODE___\n"
        "setn sp 0x00FFFFFF\n";

    auto r = get_part(src, part_t::DEFINE);
    REQUIRE(r.is_ok());
    CHECK(r.unwrap() == std::string{"SP sp\nPI 1\n"});
}

TEST_CASE("get_part extracts DATA section") {
    const char* src =
        "___DEFINE___\n"
        "SP sp\n"
        "___DATA___\n"
        "0x00200000 1\n"
        "___CODE___\n"
        "setn sp 0x00FFFFFF\n";

    auto r = get_part(src, part_t::DATA);
    REQUIRE(r.is_ok());
    CHECK(r.unwrap() == std::string{"0x00200000 1\n"});
}

TEST_CASE("get_part extracts CODE section") {
    const char* src =
        "___DEFINE___\n"
        "SP sp\n"
        "___DATA___\n"
        "0x00200000 1\n"
        "___CODE___\n"
        "setn sp 0x00FFFFFF\n";

    auto r = get_part(src, part_t::CODE);
    REQUIRE(r.is_ok());
    CHECK(r.unwrap() == std::string{"setn sp 0x00FFFFFF\n"});
}

TEST_CASE("get_part returns Err for missing section") {
    const char* src =
        "___CODE___\n"
        "setn sp 0\n";
    auto r = get_part(src, part_t::DATA);
    CHECK(r.is_err());
}

