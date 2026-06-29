PREFIX ?= /usr/local
CARGO ?= cargo
MAKE ?= make
INSTALL ?= install

BIN_DIR := target/bin
TOOL_BINS := shyasm shyemu shyld shycc
CHIBICC := third_party/chibicc/chibicc

.PHONY: bin install-bin clean-bin test cargo-test test-chibicc-shy

bin: $(BIN_DIR)
	$(CARGO) build --release -p asm -p emu -p linker -p shycc
	$(MAKE) -C third_party/chibicc chibicc
	$(INSTALL) -m 0755 target/release/asm $(BIN_DIR)/shyasm
	$(INSTALL) -m 0755 target/release/emu $(BIN_DIR)/shyemu
	$(INSTALL) -m 0755 target/release/linker $(BIN_DIR)/shyld
	$(INSTALL) -m 0755 target/release/shycc $(BIN_DIR)/shycc
	$(INSTALL) -m 0755 $(CHIBICC) $(BIN_DIR)/chibicc

install-bin: bin
	$(INSTALL) -d $(DESTDIR)$(PREFIX)/bin
	for bin in $(TOOL_BINS) chibicc; do \
		$(INSTALL) -m 0755 $(BIN_DIR)/$$bin $(DESTDIR)$(PREFIX)/bin/$$bin; \
	done

clean-bin:
	rm -rf $(BIN_DIR)

test: cargo-test test-chibicc-shy

cargo-test:
	$(CARGO) test

test-chibicc-shy:
	test/chibicc-shy/run.sh

$(BIN_DIR):
	$(INSTALL) -d $(BIN_DIR)
