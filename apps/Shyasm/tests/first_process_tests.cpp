// FirstProcess unit tests
#include "doctest.h"
#include "FirstProcess.hpp"

TEST_CASE("FirstProcess::create returns instance") {
    auto r = FirstProcess::create("___CODE___\n");
    CHECK(r.is_ok());
}

TEST_CASE("comment_process removes // and /* ... */ comments") {
    const char* src =
        "___CODE___\n"
        "setn 1x 1 // inline comment\n"
        "addn 1x 1 /* block\ncomment */  \n";
    auto r = FirstProcess::create(src);
    REQUIRE(r.is_ok());
    auto fp = std::move(r.unwrap());

    auto &after = fp->comment_process();
    auto out = after.to_string();
    // For comment removal we only assert comments are gone
    CHECK(out.find("//") == std::string::npos);
    CHECK(out.find("/*") == std::string::npos);
    CHECK(out.find("*/") == std::string::npos);
}

TEST_CASE("macro_process expands simple symbol macros from DEFINE") {
    const char* src =
        "___DEFINE___\n"
        "SP sp\n"
        "PI 10\n"
        "___CODE___\n"
        "setn sp 0x10\n"
        "outn PI\n";
    auto r = FirstProcess::create(src);
    REQUIRE(r.is_ok());
    auto fp = std::move(r.unwrap());

    fp->comment_process();
    auto mr = fp->macro_process();
    REQUIRE(mr.is_ok());
    auto out = fp->to_string();
    // Expect register alias replaced to canonical form and constants expanded
    CHECK(out.find("setn SP 0x10") != std::string::npos);
    CHECK(out.find("outn 10") != std::string::npos);
}

