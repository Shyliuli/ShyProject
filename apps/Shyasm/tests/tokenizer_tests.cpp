// Tokenizer unit tests for ShyAsm
#include "doctest.h"
#include "tokenlizer.hpp"

TEST_CASE("Tokenizer parses decimal literal") {
    auto tkr_res = Tokenizer::create("123");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());

    auto tok_res = tkr->next();
    REQUIRE(tok_res.is_ok());
    auto &tok = tok_res.unwrap();

    CHECK(tok.type() == Token::Type::DEC);
    auto val = tok.to_u32();
    REQUIRE(val.is_ok());
    CHECK(val.unwrap() == 123u);
}

TEST_CASE("Tokenizer parses hex literal 0x10 -> 16") {
    auto tkr_res = Tokenizer::create("0x10");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());

    auto tok_res = tkr->next();
    REQUIRE(tok_res.is_ok());
    auto &tok = tok_res.unwrap();

    CHECK(tok.type() == Token::Type::HEX);
    auto val = tok.to_u32();
    REQUIRE(val.is_ok());
    CHECK(val.unwrap() == 16u);
}

TEST_CASE("Tokenizer parses bin literal 101b -> 5") {
    auto tkr_res = Tokenizer::create("101b");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());

    auto tok_res = tkr->next();
    REQUIRE(tok_res.is_ok());
    auto &tok = tok_res.unwrap();

    CHECK(tok.type() == Token::Type::BIN);
    auto val = tok.to_u32();
    REQUIRE(val.is_ok());
    CHECK(val.unwrap() == 5u);
}

TEST_CASE("Tokenizer parses char literal 'A' -> 65") {
    auto tkr_res = Tokenizer::create("'A'");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());

    auto tok_res = tkr->next();
    REQUIRE(tok_res.is_ok());
    auto &tok = tok_res.unwrap();

    CHECK(tok.type() == Token::Type::CHAR);
    auto val = tok.to_u32();
    REQUIRE(val.is_ok());
    CHECK(val.unwrap() == static_cast<u32>('A'));
}

TEST_CASE("Tokenizer categorizes COMMAND, REG, DEC and NEXT_LINE") {
    auto tkr_res = Tokenizer::create("addn 1x 1\n");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());

    auto a = tkr->next();
    REQUIRE(a.is_ok());
    CHECK(a.unwrap().type() == Token::Type::COMMAND);

    auto b = tkr->next();
    REQUIRE(b.is_ok());
    CHECK(b.unwrap().type() == Token::Type::REG);

    auto c = tkr->next();
    REQUIRE(c.is_ok());
    CHECK(c.unwrap().type() == Token::Type::DEC);

    auto d = tkr->next();
    REQUIRE(d.is_ok());
    CHECK(d.unwrap().type() == Token::Type::NEXT_LINE);
}

TEST_CASE("Tokenizer reset_index rewinds to first token") {
    auto tkr_res = Tokenizer::create("1\n2\n");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());

    REQUIRE(tkr->next().is_ok());
    REQUIRE(tkr->next().is_ok());
    // rewind
    auto r = tkr->reset_index();
    REQUIRE(r.is_ok());
    auto first = tkr->next();
    REQUIRE(first.is_ok());
    CHECK(first.unwrap().type() == Token::Type::DEC);
    auto v = first.unwrap().to_u32();
    REQUIRE(v.is_ok());
    CHECK(v.unwrap() == 1u);
}

TEST_CASE("Token::tokenizer for ARRAY -> iterates elements") {
    auto tkr_res = Tokenizer::create("{'A',2,3}");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());
    auto t_res = tkr->next();
    REQUIRE(t_res.is_ok());
    auto &t = t_res.unwrap();
    CHECK(t.type() == Token::Type::ARRAY);

    auto sub_res = t.tokenizer();
    REQUIRE(sub_res.is_ok());
    auto sub = std::move(sub_res.unwrap());

    auto e1 = sub.next();
    REQUIRE(e1.is_ok());
    CHECK(e1.unwrap().type() == Token::Type::CHAR);
    CHECK(e1.unwrap().to_u32().unwrap() == static_cast<u32>('A'));

    auto e2 = sub.next();
    REQUIRE(e2.is_ok());
    CHECK(e2.unwrap().type() == Token::Type::DEC);
    CHECK(e2.unwrap().to_u32().unwrap() == 2u);

    auto e3 = sub.next();
    REQUIRE(e3.is_ok());
    CHECK(e3.unwrap().type() == Token::Type::DEC);
    CHECK(e3.unwrap().to_u32().unwrap() == 3u);
}

TEST_CASE("Token::tokenizer for STRING -> iterates characters") {
    auto tkr_res = Tokenizer::create("\"Hi\"");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());
    auto t_res = tkr->next();
    REQUIRE(t_res.is_ok());
    auto &t = t_res.unwrap();
    CHECK(t.type() == Token::Type::STRING);

    auto sub_res = t.tokenizer();
    REQUIRE(sub_res.is_ok());
    auto sub = std::move(sub_res.unwrap());

    auto c1 = sub.next();
    REQUIRE(c1.is_ok());
    CHECK(c1.unwrap().type() == Token::Type::CHAR);
    CHECK(c1.unwrap().to_u32().unwrap() == static_cast<u32>('H'));

    auto c2 = sub.next();
    REQUIRE(c2.is_ok());
    CHECK(c2.unwrap().type() == Token::Type::CHAR);
    CHECK(c2.unwrap().to_u32().unwrap() == static_cast<u32>('i'));
}

TEST_CASE("Tokenizer to_string reassembles source text") {
    auto src = std::string{"addn 1x 1\n"};
    auto tkr_res = Tokenizer::create(src);
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());
    auto out = tkr->to_string();
    REQUIRE(out.is_ok());
    CHECK(out.unwrap() == src);
}

TEST_CASE("Token::to_u32 returns Err for non-numeric token") {
    auto tkr_res = Tokenizer::create("addn");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());
    auto tok_res = tkr->next();
    REQUIRE(tok_res.is_ok());
    auto &tok = tok_res.unwrap();
    CHECK(tok.type() == Token::Type::COMMAND);
    auto v = tok.to_u32();
    CHECK(v.is_err());
}

TEST_CASE("Tokenizer invalid hex literal yields to_u32 error") {
    auto tkr_res = Tokenizer::create("0x");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());
    auto tok_res = tkr->next();
    REQUIRE(tok_res.is_ok());
    auto &tok = tok_res.unwrap();
    CHECK(tok.type() == Token::Type::HEX);
    auto v = tok.to_u32();
    CHECK(v.is_err());
}

TEST_CASE("Tokenizer next beyond end returns Err") {
    auto tkr_res = Tokenizer::create("1");
    REQUIRE(tkr_res.is_ok());
    auto tkr = std::move(tkr_res.unwrap());
    auto a = tkr->next();
    REQUIRE(a.is_ok());
    // at end
    auto b = tkr->next();
    CHECK(b.is_err());
}
