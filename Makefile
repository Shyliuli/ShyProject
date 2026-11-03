PREFIX ?= /usr/local
CARGO ?= cargo
MAKE ?= make
INSTALL ?= install

BIN_DIR := target/bin
TOOL_BINS := shyasm shyemu shyld shycc
CHIBICC := third_party/chibicc/chibicc

# C headers generated from the native ShyC headers. Order matters: stddef and
# stdint are included by the others, so they are generated first. The list is
# also the recipe order when make runs without -j; the explicit prerequisites
# below keep the order under -j.
# core/stddef.h and core/stdint.h are hand-written routers: their types follow
# the target's pointer width, which a generated header would freeze.
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

.PHONY: bin install-bin clean-bin test cargo-test test-chibicc-shy os-build os-run libshy-headers

bin: libshy-headers $(BIN_DIR)
	$(CARGO) build --release -p asm -p emu -p linker -p shycc -p shyc
	$(MAKE) -C third_party/chibicc chibicc
	$(INSTALL) -m 0755 target/release/asm $(BIN_DIR)/shyasm
	$(INSTALL) -m 0755 target/release/emu $(BIN_DIR)/shyemu
	$(INSTALL) -m 0755 target/release/linker $(BIN_DIR)/shyld
	$(INSTALL) -m 0755 target/release/shycc $(BIN_DIR)/shycc
	$(INSTALL) -m 0755 target/release/shyc $(BIN_DIR)/shyc
	$(INSTALL) -m 0755 $(CHIBICC) $(BIN_DIR)/chibicc

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
	for bin in $(TOOL_BINS) shyc chibicc; do \
		$(INSTALL) -m 0755 $(BIN_DIR)/$$bin $(DESTDIR)$(PREFIX)/bin/$$bin; \
	done

clean-bin:
	rm -rf $(BIN_DIR)

test: libshy-headers cargo-test test-chibicc-shy

cargo-test:
	$(CARGO) test

test-chibicc-shy:
	test/chibicc-shy/run.sh

os-build: libshy-headers
	$(MAKE) -C projects/os build

os-run: libshy-headers
	$(MAKE) -C projects/os run

$(BIN_DIR):
	$(INSTALL) -d $(BIN_DIR)
