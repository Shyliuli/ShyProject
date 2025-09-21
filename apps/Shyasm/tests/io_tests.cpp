// IO helper tests
#include "doctest.h"
#include "Io.hpp"
#include <fstream>
#include <filesystem>

TEST_CASE("Io::read_from_file reads text content") {
    namespace fs = std::filesystem;
    auto dir = fs::path{"build/test_io"};
    fs::create_directories(dir);
    auto path = dir / "sample.asm";

    const char* content = "___CODE___\nsetn 1x 1\n";
    {
        std::ofstream ofs(path, std::ios::binary);
        REQUIRE(ofs.good());
        ofs << content;
    }

    auto r = Io::read_from_file(path.string());
    REQUIRE(r.is_ok());
    CHECK(r.unwrap() == std::string{content});
}

TEST_CASE("Io::write_to_file dumps memory to file") {
    namespace fs = std::filesystem;
    auto dir = fs::path{"build/test_io"};
    fs::create_directories(dir);
    auto path = dir / "out.sfs";

    auto mem_res = Memory::create();
    REQUIRE(mem_res.is_ok());
    auto mem = std::move(mem_res.unwrap());

    // Write a known value
    auto w = mem->write(0xABCD1234u, Address(0x00100100u));
    REQUIRE(w.is_ok());

    auto wr = Io::write_to_file(path.string(), std::move(mem));
    REQUIRE(wr.is_ok());

    CHECK(fs::exists(path));
    CHECK(fs::file_size(path) > 0);
}

