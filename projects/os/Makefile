SHYCC = ../../target/bin/shycc
SHYASM = ../../target/bin/shyasm
SHYLD = ../../target/bin/shyld
SHYEMU = ../../target/bin/shyemu
PYTHON ?= python3

# small_mem 用它切到 4 MiB 内存布局。
SHYC_DEFS ?=

# Keep generated .shy assembly files (with source line numbers) by default.
.SECONDARY:

KERNEL_TARGET = target/kernel
USER_TARGET = target/user
FS_TARGET = target/fs

KERNEL_SFS = target/kernel.sfs
SMALL_KERNEL_SFS = target/small/kernel.sfs
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
	$(KERNEL_TARGET)/arch_shy.sobj \
	$(KERNEL_TARGET)/arch_shy_memory.sobj \
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

.PHONY: build run test clean dirs small_mem small_run riscv riscv_run riscv_test

build: $(KERNEL_SFS)

run: $(KERNEL_SFS)
	$(SHYEMU) $(KERNEL_SFS) --mem 128M

small_mem:
	$(MAKE) SHYC_DEFS=-DSHYOS_SMALL \
		KERNEL_TARGET=target/small/kernel \
		USER_TARGET=target/small/user \
		FS_TARGET=target/small/fs \
		KERNEL_SFS=$(SMALL_KERNEL_SFS) build

small_run: small_mem
	$(SHYEMU) $(SMALL_KERNEL_SFS) --mem 4M

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

# RISC-V port: the same kernel core and userland sources, built for RV64 and
# booted under QEMU virt. Address spaces use Sv39 page tables instead of the
# ShyISA segment registers.
RISCV_TARGET = target/riscv
RISCV_ELF = $(RISCV_TARGET)/kernel.elf
RISCV_USER = $(RISCV_TARGET)/user
RISCV_FS = $(RISCV_TARGET)/fs
RISCV_KERNEL_SRCS = arch/riscv/boot.S arch/riscv/trap.S arch/riscv/platform.S \
	kernel/kmain.shyc kernel/console.shyc kernel/mem.shyc kernel/proc.shyc \
	kernel/fs.shyc kernel/lock.shyc kernel/trap.shyc \
	arch/riscv/platform.shyc arch/riscv/vm.shyc arch/riscv/ctx.shyc arch/riscv/string.shyc
RISCV_USER_SRCS = arch/riscv/user/header.S arch/riscv/user/entry.S arch/riscv/user/syscall.S \
	arch/riscv/user/usys.shyc
RISCV_USER_PROGS := $(basename $(notdir $(USER_PROG_SRCS))) $(BUSYBOX_APPLETS)
RISCV_USER_SFS := $(addprefix $(RISCV_USER)/,$(addsuffix .sfs,$(RISCV_USER_PROGS)))
# Missing user images must be rebuilt even when an older fsdata.c exists.
.NOTINTERMEDIATE: $(RISCV_USER_SFS)
.PHONY: riscv_user

riscv: $(RISCV_ELF)

riscv_user: $(RISCV_USER_SFS)

$(RISCV_USER)/%.sfs: $(RISCV_USER_SRCS) user/%.shyc user/ulib.shyc user/usys.h user/ulib.h kernel/memlayout.h arch/riscv/user/user.ld Makefile $(SHYCC) ../../target/bin/shyc | dirs
	mkdir -p $(RISCV_USER)
	$(SHYCC) --target=riscv64-unknown-none -DSHYOS_RISCV -Iuser \
		-Wl,--oformat=binary -Wl,-T,arch/riscv/user/user.ld \
		-o $@ user/$*.shyc user/ulib.shyc $(RISCV_USER_SRCS)

# The busybox applets are the same image under different names.
$(addprefix $(RISCV_USER)/,$(addsuffix .sfs,$(BUSYBOX_APPLETS))): $(RISCV_USER)/busybox.sfs
	cp $< $@

$(RISCV_FS)/fsdata.c: tools/bin2shy.py $(RISCV_USER_SFS) test.txt | dirs
	mkdir -p $(RISCV_FS)
	$(PYTHON) tools/bin2shy.py -o $@ $(RISCV_USER_SFS) test.txt

$(RISCV_ELF): $(RISCV_KERNEL_SRCS) $(wildcard kernel/*.h) $(RISCV_FS)/fsdata.c arch/riscv/kernel.ld Makefile $(SHYCC) ../../target/bin/shyc | dirs
	mkdir -p $(RISCV_TARGET)
	$(SHYCC) --target=riscv64-unknown-none -DSHYOS_RISCV \
		-Wl,-T,arch/riscv/kernel.ld \
		-o $@ $(RISCV_KERNEL_SRCS) $(RISCV_FS)/fsdata.c

riscv_run: riscv
	qemu-system-riscv64 -M virt -kernel $(RISCV_ELF) -nographic

riscv_test: riscv
	@set -eu; \
	out=$(RISCV_TARGET)/boot-output.txt; \
	{ sleep 3; printf "echo hello\ncat test.txt\nexit\n"; } | timeout 30 qemu-system-riscv64 -M virt -kernel $(RISCV_ELF) -nographic > $$out || test $$? -eq 124; \
	grep -Fx 'ShyOS boot' $$out >/dev/null; \
	grep -F 'hello' $$out >/dev/null; \
	cat $$out; \
	echo "riscv boot test passed"

dirs:
	mkdir -p $(KERNEL_TARGET) $(USER_TARGET) $(FS_TARGET)

$(KERNEL_SFS): $(KERNEL_OBJS) | dirs
	$(SHYLD) $(KERNEL_OBJS) -o $@

$(KERNEL_TARGET)/entry.sobj: kernel/entry.shy | dirs
	$(SHYASM) $< -o $@

$(KERNEL_TARGET)/arch_shy.shy: arch/shy/platform.shyc | dirs
	$(SHYCC) -S $(SHYC_DEFS) --shy-emit-source-lines $< -o $@

$(KERNEL_TARGET)/arch_shy.sobj: $(KERNEL_TARGET)/arch_shy.shy | dirs
	$(SHYASM) $< -o $@

$(KERNEL_TARGET)/arch_shy_memory.shy: arch/shy/memory.shyc | dirs
	$(SHYCC) -S $(SHYC_DEFS) --shy-emit-source-lines $< -o $@

$(KERNEL_TARGET)/arch_shy_memory.sobj: $(KERNEL_TARGET)/arch_shy_memory.shy | dirs
	$(SHYASM) $< -o $@

$(KERNEL_TARGET)/%.sobj: kernel/%.shy | dirs
	$(SHYASM) $< -o $@

$(KERNEL_TARGET)/%.shy: kernel/%.shyc | dirs
	$(SHYCC) -S $(SHYC_DEFS) --shy-emit-source-lines $< -o $@

$(KERNEL_TARGET)/%.sobj: $(KERNEL_TARGET)/%.shy | dirs
	$(SHYASM) $< -o $@

$(FS_TARGET)/fsdata.c: tools/bin2shy.py $(FS_INPUTS) | dirs
	$(PYTHON) tools/bin2shy.py -o $@ $(FS_INPUTS)

$(FS_TARGET)/%.shy: $(FS_TARGET)/%.c | dirs
	$(SHYCC) -S $(SHYC_DEFS) --shy-emit-source-lines $< -o $@

$(FS_TARGET)/%.sobj: $(FS_TARGET)/%.shy | dirs
	$(SHYASM) $< -o $@

$(USER_TARGET)/usys.sobj: user/usys.shy | dirs
	$(SHYASM) $< -o $@

$(USER_TARGET)/%.shy: user/%.shyc | dirs
	$(SHYCC) -S $(SHYC_DEFS) --shy-emit-source-lines $< -o $@

$(USER_TARGET)/%.sobj: $(USER_TARGET)/%.shy | dirs
	$(SHYASM) $< -o $@

$(USER_TARGET)/%.sfs: $(USER_TARGET)/%.sobj $(USER_LIB_OBJS) | dirs
	$(SHYLD) $^ -o $@

$(BUSYBOX_APPLET_SFS): $(USER_TARGET)/busybox.sfs | dirs
	cp $< $@

clean:
	rm -rf target
