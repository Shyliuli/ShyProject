#define DOCTEST_CONFIG_IMPLEMENT_WITH_MAIN
#include "../../../third_party/doctest.h"
#include "../include/first_scanner.hpp"
#include <memory>

using std::string;
using std::unique_ptr;

TEST_CASE("first_scanner creation") {
    SUBCASE("successful creation") {
        string code = "test code";
        auto result = first_scanner::create(code);
        CHECK(result.is_ok());
        auto& scanner = result.unwrap();
        CHECK(scanner->to_str() == "test code");
    }
}

TEST_CASE("comment processing") {
    SUBCASE("single line comment removal") {
        string code = "setn sp 0x00FFFFFF // set stack pointer\nsetn 1x 1 // set register";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer();
        string result = scanner->to_str();

        CHECK(result.find("//") == string::npos);
        CHECK(result.find("setn sp 0x00FFFFFF") != string::npos);
        CHECK(result.find("setn 1x 1") != string::npos);
    }

    SUBCASE("block comment removal") {
        string code = "setn sp 0x00FFFFFF /* this is a block comment */ setn 1x 1";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer();
        string result = scanner->to_str();

        CHECK(result.find("/*") == string::npos);
        CHECK(result.find("*/") == string::npos);
        CHECK(result.find("setn sp 0x00FFFFFF") != string::npos);
        CHECK(result.find("setn 1x 1") != string::npos);
    }

    SUBCASE("multiline block comment") {
        string code = "setn sp 0x00FFFFFF\n/* this is a\n   multiline comment\n   spanning several lines */\nsetn 1x 1";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer();
        string result = scanner->to_str();

        CHECK(result.find("/*") == string::npos);
        CHECK(result.find("*/") == string::npos);
        CHECK(result.find("multiline comment") == string::npos);
        CHECK(result.find("setn sp 0x00FFFFFF") != string::npos);
        CHECK(result.find("setn 1x 1") != string::npos);
    }

    SUBCASE("unclosed block comment") {
        string code = "setn sp 0x00FFFFFF /* unclosed comment\nsetn 1x 1";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer();
        string result = scanner->to_str();

        CHECK(result.find("/*") == string::npos);
        CHECK(result.find("unclosed comment") == string::npos);
        CHECK(result.find("setn 1x 1") == string::npos);
        CHECK(result.find("setn sp 0x00FFFFFF") != string::npos);
    }

    SUBCASE("comment at end of file") {
        string code = "setn sp 0x00FFFFFF //comment at end";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer();
        string result = scanner->to_str();

        CHECK(result.find("//") == string::npos);
        CHECK(result.find("comment at end") == string::npos);
        CHECK(result.find("setn sp 0x00FFFFFF") != string::npos);
    }

    SUBCASE("mixed comments") {
        string code = "setn sp 0x00FFFFFF // line comment\n/* block comment */ setn 1x 1 // another line";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer();
        string result = scanner->to_str();

        CHECK(result.find("//") == string::npos);
        CHECK(result.find("/*") == string::npos);
        CHECK(result.find("*/") == string::npos);
        CHECK(result.find("setn sp 0x00FFFFFF") != string::npos);
        CHECK(result.find("setn 1x 1") != string::npos);
    }
}

TEST_CASE("macro definition processing") {
    SUBCASE("simple macro replacement") {
        string code = "___DEFINE___\nSP sp\nPI 314159\n___CODE___\nsetn SP 0x00FFFFFF\noutn PI";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->define_processer();
        string result = scanner->to_str();

        CHECK(result.find("setn sp 0x00FFFFFF") != string::npos);
        CHECK(result.find("outn 314159") != string::npos);
        // 确保宏已被替换（不再包含原始的宏名称）
        bool sp_replaced = (result.find("setn SP") == string::npos);
        bool pi_replaced = (result.find("outn PI") == string::npos);
        CHECK(sp_replaced);
        CHECK(pi_replaced);
    }

    SUBCASE("macro replacement in DATA section") {
        string code = "___DEFINE___\nSIZE 1024\nADDR 0x200000\n___DATA___\nSIZE bytes at ADDR\n___CODE___\nsetn 1x SIZE";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->define_processer();
        string result = scanner->to_str();

        CHECK(result.find("1024 bytes at 0x200000") != string::npos);
        CHECK(result.find("setn 1x 1024") != string::npos);
    }

    SUBCASE("word boundary checking") {
        string code = "___DEFINE___\nSP sp\n___CODE___\nSPAC test\ntest SP test\nSPtest";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->define_processer();
        string result = scanner->to_str();

        // SP should be replaced, but SPAC and SPtest should not
        CHECK(result.find("SPAC test") != string::npos);
        CHECK(result.find("test sp test") != string::npos);
        CHECK(result.find("SPtest") != string::npos);
    }

    SUBCASE("no DEFINE section") {
        string code = "___CODE___\nsetn sp 0x00FFFFFF\nsetn 1x 1";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->define_processer();
        string result = scanner->to_str();

        // Should remain unchanged
        CHECK(result == code);
    }

    SUBCASE("multiple macro definitions") {
        string code = "___DEFINE___\nREG1 0x1\nREG2 0x2\nVAL1 100\nVAL2 200\n___CODE___\nsetn REG1 VAL1\nsetn REG2 VAL2";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->define_processer();
        string result = scanner->to_str();

        CHECK(result.find("setn 0x1 100") != string::npos);
        CHECK(result.find("setn 0x2 200") != string::npos);
    }
}

TEST_CASE("chained processing") {
    SUBCASE("comment then define processing") {
        string code = "___DEFINE___\n// Define stack pointer\nSP sp\nPI 314159 // Pi constant\n___CODE___\nsetn SP 0x00FFFFFF // Initialize stack\noutn PI // Output pi";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer().define_processer();
        string result = scanner->to_str();

        // Comments should be removed and macros replaced
        CHECK(result.find("//") == string::npos);
        CHECK(result.find("setn sp 0x00FFFFFF") != string::npos);
        CHECK(result.find("outn 314159") != string::npos);
    }

    SUBCASE("define then comment processing") {
        string code = "___DEFINE___\nSP sp\nPI 314159\n___CODE___\nsetn SP 0x00FFFFFF // comment\noutn PI // another comment";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->define_processer().comment_processer();
        string result = scanner->to_str();

        // Macros should be replaced and comments removed
        CHECK(result.find("//") == string::npos);
        CHECK(result.find("setn sp 0x00FFFFFF") != string::npos);
        CHECK(result.find("outn 314159") != string::npos);
    }
}

TEST_CASE("edge cases") {
    SUBCASE("empty input") {
        string code = "";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer().define_processer();
        CHECK(scanner->to_str() == "");
    }

    SUBCASE("only whitespace") {
        string code = "   \n\t  \n  ";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer().define_processer();
        string result = scanner->to_str();
        CHECK(result == code); // Should remain unchanged
    }

    SUBCASE("only comments") {
        string code = "// just a comment\n/* another comment */";
        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer();
        string result = scanner->to_str();
        CHECK(result.find("//") == string::npos);
        CHECK(result.find("/*") == string::npos);
        CHECK(result.find("*/") == string::npos);
    }
}

TEST_CASE("complex realistic example") {
    SUBCASE("full assembly program") {
        string code = R"(
___DEFINE___
// Stack pointer alias
SP sp
PI 314159          // Pi constant
HELLO_ADDR 0x00210000
COUNT_ADDR 0x00200001
___DATA___
// Data initialization
HELLO_ADDR "Hello!" // String
0x00200000 'A'      // Character
COUNT_ADDR 12345678 // 32-bit value
___CODE___
setn SP 0x00FFFFFF // Initialize stack pointer
setn 1x 1          // Set register 1x to 1
.start             // Label for loop start
addn COUNT_ADDR 1  // Increment counter
outaasc 0x00200000 // Output ASCII character
outn PI            // Output pi constant
addn 1x 1          // Increment 1x
sman 1x 10         // Compare 1x with 10
jmpn .start        // Jump if condition met
)";

        auto res = first_scanner::create(code);
        CHECK(res.is_ok());
        auto& scanner = res.unwrap();
        scanner->comment_processer().define_processer();
        string result = scanner->to_str();

        // Check that comments are removed
        CHECK(result.find("//") == string::npos);

        // Check that macros are replaced correctly
        CHECK(result.find("setn sp 0x00FFFFFF") != string::npos);
        CHECK(result.find("outn 314159") != string::npos);
        CHECK(result.find("0x00210000 \"Hello!\"") != string::npos);
        CHECK(result.find("addn 0x00200001 1") != string::npos);

        // Check that labels and other elements remain
        CHECK(result.find(".start") != string::npos);
        CHECK(result.find("sman 1x 10") != string::npos);
    }
}