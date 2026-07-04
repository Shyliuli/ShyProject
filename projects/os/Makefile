SHYCC = ../../target/bin/shycc
SHYASM = ../../target/bin/shyasm
SHYLD = ../../target/bin/shyld
SHYEMU = ../../target/bin/shyemu
PYTHON ?= python3

# Keep generated .shy assembly files (with source line numbers) by default.
.SECONDARY:

KERNEL_TARGET = target/kernel
USER_TARGET = target/user
FS_TARGET = target/fs

KERNEL_SFS = target/kernel.sfs
USER_SRCS := $(wildcard user/*.shyc user/*.shy)
BUSYBOX_APPLETS := sh echo cat ls mkdir rm pwd wc grep ps kill exit
USER_PROG_SRCS := $(filter-out user/usys.shy user/ulib.shyc user/sh.shyc user/echo.shyc user/cat.shyc,$(USER_SRCS))
USER_PROGS := $(basename $(notdir $(USER_PROG_SRCS)))
USER_SFS := $(addprefix $(USER_TARGET)/,$(addsuffix .sfs,$(USER_PROGS)))
BUSYBOX_APPLET_SFS := $(addprefix $(USER_TARGET)/,$(addsuffix .sfs,$(BUSYBOX_APPLETS)))
USER_LIB_OBJS = $(USER_TARGET)/usys.sobj $(USER_TARGET)/ulib.sobj
FS_INPUTS := $(USER_SFS) $(BUSYBOX_APPLET_SFS) test.txt

KERNEL_OBJS = \
	$(KERNEL_TARGET)/entry.sobj \
	$(KERNEL_TARGET)/console.sobj \
	$(KERNEL_TARGET)/lock.sobj \
	$(KERNEL_TARGET)/mem.sobj \
	$(KERNEL_TARGET)/proc.sobj \
	$(KERNEL_TARGET)/fs.sobj \
	$(KERNEL_TARGET)/string.sobj \
	$(KERNEL_TARGET)/ctx.sobj \
	$(KERNEL_TARGET)/trapentry.sobj \
	$(KERNEL_TARGET)/scheduler.sobj \
	$(KERNEL_TARGET)/trap.sobj \
	$(KERNEL_TARGET)/kmain.sobj \
	$(FS_TARGET)/fsdata.sobj

.PHONY: build run test clean dirs

build: $(KERNEL_SFS)

run: $(KERNEL_SFS)
	$(SHYEMU) $(KERNEL_SFS) --mem 128M

test: $(KERNEL_SFS)
	@set -eu; \
	out=target/test-output.txt; \
	printf "echo hello\ncat test.txt\nexit\n" | $(SHYEMU) $(KERNEL_SFS) --mem 128M > "$$out"; \
	grep -Fx 'ShyOS boot' "$$out" >/dev/null; \
	grep -Fx '$$ hello' "$$out" >/dev/null; \
	grep -F 'hello' "$$out" >/dev/null; \
	grep -Fx '$$ hello world' "$$out" >/dev/null; \
	grep -F 'hello world' "$$out" >/dev/null; \
	test "$$(tail -c 2 "$$out")" = '$$ '; \
	cat "$$out"; \
	exit_out=target/test-exit-output.txt; \
	printf "exit\n" | $(SHYEMU) $(KERNEL_SFS) --mem 128M > "$$exit_out"; \
	grep -Fx 'ShyOS boot' "$$exit_out" >/dev/null; \
	test "$$(tail -c 2 "$$exit_out")" = '$$ '; \
	stack_out=target/test-stack-output.txt; \
	printf "stackbig\n" | $(SHYEMU) $(KERNEL_SFS) --mem 128M > "$$stack_out"; \
	grep -Fx 'ShyOS boot' "$$stack_out" >/dev/null; \
	grep -Fx '$$ stack ok' "$$stack_out" >/dev/null; \
	grep -F 'stack ok' "$$stack_out" >/dev/null; \
	test "$$(tail -c 2 "$$stack_out")" = '$$ '; \
	p2_out=target/test-p2-output.txt; \
	printf "p2wait\np2wake\np2kill\n" | $(SHYEMU) $(KERNEL_SFS) --mem 128M > "$$p2_out"; \
	grep -Fx 'ShyOS boot' "$$p2_out" >/dev/null; \
	grep -Fx '$$ p2wait child exit' "$$p2_out" >/dev/null; \
	grep -F 'p2wait child exit' "$$p2_out" >/dev/null; \
	grep -F 'p2wait parent woke' "$$p2_out" >/dev/null; \
	grep -Fx '$$ p2wake child woke' "$$p2_out" >/dev/null; \
	grep -F 'p2wake child woke' "$$p2_out" >/dev/null; \
	grep -F 'p2wake parent done' "$$p2_out" >/dev/null; \
	grep -Fx '$$ p2kill child killed' "$$p2_out" >/dev/null; \
	grep -F 'p2kill child killed' "$$p2_out" >/dev/null; \
	test "$$(tail -c 2 "$$p2_out")" = '$$ '; \
	p3_out=target/test-p3-output.txt; \
	printf "p3lock\n" | $(SHYEMU) $(KERNEL_SFS) --mem 128M > "$$p3_out"; \
	grep -Fx 'ShyOS boot' "$$p3_out" >/dev/null; \
	grep -Fx '$$ p3lock count 200' "$$p3_out" >/dev/null; \
	grep -F 'p3lock count 200' "$$p3_out" >/dev/null; \
	grep -F 'p3lock ok' "$$p3_out" >/dev/null; \
	test "$$(tail -c 2 "$$p3_out")" = '$$ '; \
	p4_out=target/test-p4-output.txt; \
	printf "echo hello | cat\np4pipe\nexit\n" | $(SHYEMU) $(KERNEL_SFS) --mem 128M > "$$p4_out"; \
	grep -Fx 'ShyOS boot' "$$p4_out" >/dev/null; \
	grep -Fx '$$ hello' "$$p4_out" >/dev/null; \
	grep -F 'hello' "$$p4_out" >/dev/null; \
	grep -Fx '$$ p4pipe ok' "$$p4_out" >/dev/null; \
	grep -F 'p4pipe ok' "$$p4_out" >/dev/null; \
	test "$$(tail -c 2 "$$p4_out")" = '$$ '; \
	p7_out=target/test-p7-output.txt; \
	printf "p7malloc\nexit\n" | $(SHYEMU) $(KERNEL_SFS) --mem 128M > "$$p7_out"; \
	grep -Fx 'ShyOS boot' "$$p7_out" >/dev/null; \
	grep -Fx '$$ p7malloc ok' "$$p7_out" >/dev/null; \
	grep -F 'p7malloc ok' "$$p7_out" >/dev/null; \
	test "$$(tail -c 2 "$$p7_out")" = '$$ '; \
	echo "integration tests passed"

dirs:
	mkdir -p $(KERNEL_TARGET) $(USER_TARGET) $(FS_TARGET)

$(KERNEL_SFS): $(KERNEL_OBJS) | dirs
	$(SHYLD) $(KERNEL_OBJS) -o $@

$(KERNEL_TARGET)/entry.sobj: kernel/entry.shy | dirs
	$(SHYASM) $< -o $@

$(KERNEL_TARGET)/%.sobj: kernel/%.shy | dirs
	$(SHYASM) $< -o $@

$(KERNEL_TARGET)/%.shy: kernel/%.shyc | dirs
	$(SHYCC) -S --shy-emit-source-lines $< -o $@

$(KERNEL_TARGET)/%.sobj: $(KERNEL_TARGET)/%.shy | dirs
	$(SHYASM) $< -o $@

$(FS_TARGET)/fsdata.c: tools/bin2shy.py $(FS_INPUTS) | dirs
	$(PYTHON) tools/bin2shy.py -o $@ $(FS_INPUTS)

$(FS_TARGET)/%.shy: $(FS_TARGET)/%.c | dirs
	$(SHYCC) -S --shy-emit-source-lines $< -o $@

$(FS_TARGET)/%.sobj: $(FS_TARGET)/%.shy | dirs
	$(SHYASM) $< -o $@

$(USER_TARGET)/usys.sobj: user/usys.shy | dirs
	$(SHYASM) $< -o $@

$(USER_TARGET)/%.shy: user/%.shyc | dirs
	$(SHYCC) -S --shy-emit-source-lines $< -o $@

$(USER_TARGET)/%.sobj: $(USER_TARGET)/%.shy | dirs
	$(SHYASM) $< -o $@

$(USER_TARGET)/%.sfs: $(USER_TARGET)/%.sobj $(USER_LIB_OBJS) | dirs
	$(SHYLD) $^ -o $@

$(BUSYBOX_APPLET_SFS): $(USER_TARGET)/busybox.sfs | dirs
	cp $< $@

clean:
	rm -rf target
