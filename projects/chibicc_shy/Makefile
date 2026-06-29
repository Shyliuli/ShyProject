SRCS := main.c runtime.c strings.c tokenize.c unicode.c hashmap.c parse.c type.c codegen_shy.c shy_runtime_softfloat.c
TARGET := $(CURDIR)/target
BUILD_SFS := $(TARGET)/chibicc_shy.sfs
BUILD_SYM := $(TARGET)/chibicc_shy.sym
BUILD_SHY := $(TARGET)/chibicc_shy.shy
BUILD_ASMS := $(SRCS:%.c=$(TARGET)/%.shy)
TEST_SHY := $(TARGET)/test.shy
TEST_SOBJ := $(TARGET)/test.sobj
TEST_SFS := $(TARGET)/test.sfs
TEST_SYM := $(TARGET)/test.sym
TEST_OUT := $(TARGET)/test.out

build: $(BUILD_SFS) $(BUILD_SYM) $(BUILD_SHY)

$(TARGET):
	mkdir -p $(TARGET)

$(BUILD_SFS): $(SRCS) chibicc.h | $(TARGET)
	cargo run -q -p shycc -- $(SRCS) -llibshy -o $(BUILD_SFS) --sym $(BUILD_SYM)

$(BUILD_SYM): $(BUILD_SFS)
	test -f $(BUILD_SYM)

$(TARGET)/%.shy: %.c chibicc.h | $(TARGET)
	cargo run -q -p shycc -- $< -S -o $@

$(BUILD_SHY): $(BUILD_ASMS) | $(TARGET)
	printf '' > $(BUILD_SHY)
	for f in $(BUILD_ASMS); do printf '\n# source: %s\n' "$$f" >> $(BUILD_SHY); cat "$$f" >> $(BUILD_SHY); done

run: $(TEST_SHY)

$(TEST_SHY): $(BUILD_SFS) test.shyc | $(TARGET)
	(cat test.shyc; printf '\n__SHYCC_END__\n') | cargo run -q -p emu -- $(BUILD_SFS) > $(TEST_SHY)

build_test: $(TEST_SFS) $(TEST_SYM)

$(TEST_SFS): $(TEST_SOBJ)
	cargo run -q -p linker -- $(TEST_SOBJ) -o $(TEST_SFS) --sym $(TEST_SYM)

$(TEST_SYM): $(TEST_SFS)
	test -f $(TEST_SYM)

$(TEST_SOBJ): $(TEST_SHY)
	cargo run -q -p asm -- $(TEST_SHY) -o $(TEST_SOBJ)

run_test: $(TEST_SFS) test.expected
	cargo run -q -p emu -- $(TEST_SFS) > $(TEST_OUT)
	diff -u $(CURDIR)/test.expected $(TEST_OUT)

test: run_test

clean:
	rm -rf $(TARGET)

.PHONY: build run build_test run_test test clean
