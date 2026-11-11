PREFIX ?= /usr/local
CARGO ?= cargo
MAKE ?= make
INSTALL ?= install

BIN_DIR := target/bin
TEST_BIN_DIR := build/bin
TOOL_BINS := shyasm shyemu shyemu-user shyld shycc shy-dwarf
LSP_DIR := vscode-shyc/server

# C headers generated from the native ShyC headers. Order matters: stddef and
# stdint are included by the others, so they are generated first. The list is
# also the recipe order when make runs without -j; the explicit prerequisites
# below keep the order under -j.
# stddef, stdint and errno are hand-written routers: the first two follow the
# target's pointer width, and errno must use the host libc on hosted builds.
LIBSHY_HEADERS := \
	libshy/c/stdtype.h \
	libshy/c/stdarg.h \
	libshy/core/option.h \
	libshy/core/result.h \
	libshy/c/ctype.h \
	libshy/c/fcntl.h \
	libshy/c/stdio.h \
	libshy/c/stdlib.h \
	libshy/c/string.h \
	libshy/c/unistd.h
LIBSHY_C_HEADERS := $(filter libshy/c/%.h,$(LIBSHY_HEADERS))
# Same lookup order as shycc: c/ is the default layer, libshy/ only for the
# explicit c/ and core/ spellings, include/ for the flat compatibility wrappers.
LIBSHY_INCLUDES := -Ilibshy/c -Ilibshy -Ilibshy/include
SHYC_RELEASE := target/release/shyc
SHYC_SOURCES := $(wildcard shyc/src/*.rs shyc/src/*/*.rs) shyc/Cargo.toml

.PHONY: bin tools testpath install-bin clean-bin test cargo-test test-shyc os-build os-run libshy-headers

bin: tools
	$(CARGO) build --release --manifest-path $(LSP_DIR)/Cargo.toml
	$(INSTALL) -m 0755 $(LSP_DIR)/target/release/shyc-lsp $(BIN_DIR)/shyc-lsp

# The toolchain under the names shycc looks for: it runs the assembler and
# linker that sit next to its own binary, so the installed names are what make
# `$(BIN_DIR)/shycc` work without any SHYCC_* variable.
tools: libshy-headers $(BIN_DIR)
	$(CARGO) build --release -p asm -p emu -p linker -p shycc -p shyc -p shy-dwarf
	$(INSTALL) -m 0755 target/release/asm $(BIN_DIR)/shyasm
	$(INSTALL) -m 0755 target/release/emu $(BIN_DIR)/shyemu
	$(INSTALL) -m 0755 target/release/shyemu-user $(BIN_DIR)/shyemu-user
	$(INSTALL) -m 0755 target/release/linker $(BIN_DIR)/shyld
	$(INSTALL) -m 0755 target/release/shycc $(BIN_DIR)/shycc
	$(INSTALL) -m 0755 target/release/shyc $(BIN_DIR)/shyc
	$(INSTALL) -m 0755 target/release/shy-dwarf $(BIN_DIR)/shy-dwarf

# A staging directory for running the toolchain from a test or by hand:
# build/bin/shycc finds shyasm and shyld beside itself, so no SHYCC_* variable is
# needed. It lives outside target/ so `cargo clean` cannot take it away, and the
# line it prints can be evaluated: `eval "$(make -s testpath)"`.
testpath: tools
	$(INSTALL) -d $(TEST_BIN_DIR)
	$(INSTALL) -m 0755 $(BIN_DIR)/shyasm $(BIN_DIR)/shyemu $(BIN_DIR)/shyemu-user $(BIN_DIR)/shyld $(BIN_DIR)/shycc $(BIN_DIR)/shyc $(BIN_DIR)/shy-dwarf $(TEST_BIN_DIR)/
	@echo "export PATH=$(CURDIR)/$(TEST_BIN_DIR):$$PATH"

libshy-headers: $(LIBSHY_HEADERS)

# The generated headers are committed, so this rule only runs after a native
# header changed. shyc is the only tool needed, which keeps the target free of
# a bin -> headers -> bin bootstrap cycle.
$(SHYC_RELEASE): $(SHYC_SOURCES)
	$(CARGO) build --release -p shyc

# c/ 是给 C 的 include 视图，只有 .h：声明源在 libc/(C 库风格) 与 core/(ShyC 风格)。
libshy/c/%.h: libshy/libc/%.shyh $(SHYC_RELEASE)
	$(SHYC_RELEASE) --emit=c-header $(LIBSHY_INCLUDES) -o $@ $<

# core/ 的 C 拼写留在本层：Option/Result 是 ShyC 的类型，不是 C 库头文件。
libshy/core/%.h: libshy/core/%.shyh $(SHYC_RELEASE)
	$(SHYC_RELEASE) --emit=c-header $(LIBSHY_INCLUDES) -o $@ $<



install-bin: bin
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/bin
	for bin in $(TOOL_BINS) shyc; do \
		$(INSTALL) -m 0755 $(BIN_DIR)/$$bin $(DESTDIR)$(PREFIX)/bin/$$bin; \
	done
	printf '# Where this project'"'"'s C library lives, so the compiler can resolve\n# `use std::io::*;` from any directory.\nshylib = "%s"\n' "$(CURDIR)/libshy" > $(HOME)/.shycc.toml

clean-bin:
	rm -rf $(BIN_DIR)

test: libshy-headers cargo-test test-shyc

cargo-test:
	$(CARGO) test

test-shyc: testpath
	test/shyc/run.sh

os-build: libshy-headers
	$(MAKE) -C projects/os build

os-run: libshy-headers
	$(MAKE) -C projects/os run

$(BIN_DIR):
	$(INSTALL) -d $(BIN_DIR)
