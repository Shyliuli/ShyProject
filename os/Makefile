ROOT := ..
BUILD := build

.PHONY: all build run clean

all: build

build: $(BUILD)/shyos.sfs

run: $(BUILD)/shyos.sfs
	cd $(ROOT) && cargo run -q -p emu -- os/$(BUILD)/shyos.sfs

$(BUILD)/.dir:
	mkdir -p $(BUILD)
	touch $(BUILD)/.dir

$(BUILD)/shell.sfs: user/crt0.shy user/exit.shy user/syscall.shyc user/shell.shyc | $(BUILD)/.dir
	cd $(ROOT) && cargo run -q -p shycc -- \
		-Ios/kernel \
		os/user/crt0.shy os/user/exit.shy os/user/syscall.shyc os/user/shell.shyc \
		-o os/$(BUILD)/shell.sfs --sym os/$(BUILD)/shell.sym

$(BUILD)/hello.sfs: user/crt0.shy user/exit.shy user/syscall.shyc user/hello.shyc | $(BUILD)/.dir
	cd $(ROOT) && cargo run -q -p shycc -- \
		-Ios/kernel \
		os/user/crt0.shy os/user/exit.shy os/user/syscall.shyc os/user/hello.shyc \
		-o os/$(BUILD)/hello.sfs --sym os/$(BUILD)/hello.sym

$(BUILD)/ticker.sfs: user/crt0.shy user/exit.shy user/syscall.shyc user/ticker.shyc | $(BUILD)/.dir
	cd $(ROOT) && cargo run -q -p shycc -- \
		-Ios/kernel \
		os/user/crt0.shy os/user/exit.shy os/user/syscall.shyc os/user/ticker.shyc \
		-o os/$(BUILD)/ticker.sfs --sym os/$(BUILD)/ticker.sym

$(BUILD)/ramdisk.shy: $(BUILD)/shell.sfs $(BUILD)/hello.sfs $(BUILD)/ticker.sfs ramdisk/readme.txt ramdisk/hello.txt tools/embed_ramdisk.py | $(BUILD)/.dir
	python3 tools/embed_ramdisk.py $(BUILD)/ramdisk.shy \
		shell $(BUILD)/shell.sfs \
		hello $(BUILD)/hello.sfs \
		ticker $(BUILD)/ticker.sfs \
		readme.txt ramdisk/readme.txt \
		hello.txt ramdisk/hello.txt

$(BUILD)/shyos.sfs: kernel/boot.shy kernel/trap_entry.shy kernel/kernel.shyc kernel/sched.shyc kernel/syscall.shyc kernel/fault.shyc kernel/ramdisk.shyc $(BUILD)/ramdisk.shy | $(BUILD)/.dir
	cd $(ROOT) && cargo run -q -p shycc -- \
		-Ios/kernel \
		os/kernel/boot.shy os/kernel/trap_entry.shy \
		os/kernel/kernel.shyc os/kernel/sched.shyc os/kernel/syscall.shyc os/kernel/fault.shyc os/kernel/ramdisk.shyc \
		os/$(BUILD)/ramdisk.shy \
		-o os/$(BUILD)/shyos.sfs --sym os/$(BUILD)/shyos.sym

clean:
	rm -rf $(BUILD)
