# C userland for MogOs: musl with the MogOs syscall layer (`musl/` is copied over the release), busybox, and the C
# test programs. Sources are fetched into `third_party/` and checked against SHA-256; everything builds into $(OUT).
# The board's build.rs runs `make -C c OUT=<repo>/target/c`; when nothing changed it does nothing.

ROOT := $(abspath $(CURDIR)/..)
OUT ?= $(ROOT)/target/c
DL := $(ROOT)/third_party

MUSL := musl-1.2.5
MUSL_SHA := a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4
BUSYBOX := busybox-1.36.1
BUSYBOX_SHA := b8cc24c9574d809e7279c3be349795c5d5ceb6fdf19ca709f80cde50e47de314

LLVM ?= /opt/homebrew/opt/llvm/bin
export MOG_DYLIBS := $(shell rustc --print sysroot)/lib
RUSTLIB := $(MOG_DYLIBS)/rustlib
# Soft-float libcalls (`__adddf3`, `__multf3`, ...) from the pinned toolchain, built for the same ABI.
BUILTINS := $(firstword $(wildcard $(RUSTLIB)/aarch64-unknown-none-softfloat/lib/libcompiler_builtins-*.rlib))
SYSROOT := $(OUT)/sysroot
# The kernel never enables FP/SIMD, so C code is soft-float like the Rust programs.
CFLAGS := --target=aarch64-linux-musl -march=armv8-a+nofp -mabi=aapcs-soft -mno-outline-atomics \
	-fno-stack-protector -fno-unwind-tables -fno-asynchronous-unwind-tables -Os
export MOG_CLANG := $(LLVM)/clang
export MOG_CFLAGS := $(CFLAGS)
export MOG_SYSROOT := $(SYSROOT)
export MOG_LD := $(OUT)/host/ld.lld
export MOG_LINK := $(CURDIR)/link.ld
export MOG_BUILTINS := $(BUILTINS)
CC := $(CURDIR)/mog-cc
# busybox's kbuild scripts expect GNU sed.
HOSTPATH := $(OUT)/host:$(PATH)

BUSYBOX_VARS := HOSTCC=cc CC=$(CC) AR=$(LLVM)/llvm-ar NM=$(LLVM)/llvm-nm STRIP=$(LLVM)/llvm-strip \
	OBJCOPY=$(LLVM)/llvm-objcopy

PROGRAMS := hello cbench
BINS := $(OUT)/bin/busybox $(PROGRAMS:%=$(OUT)/bin/%)

all: $(BINS)

$(DL)/$(MUSL).tar.gz:
	mkdir -p $(DL)
	curl -fsSL -o $@.part https://musl.libc.org/releases/$(MUSL).tar.gz
	echo "$(MUSL_SHA)  $@.part" | shasum -a 256 -c
	mv $@.part $@

$(DL)/$(BUSYBOX).tar.bz2:
	mkdir -p $(DL)
	curl -fsSL -o $@.part https://busybox.net/downloads/$(BUSYBOX).tar.bz2
	echo "$(BUSYBOX_SHA)  $@.part" | shasum -a 256 -c
	mv $@.part $@

$(OUT)/host/ld.lld:
	mkdir -p $(OUT)/host
	ln -sf $(RUSTLIB)/aarch64-apple-darwin/bin/rust-lld $@
	ln -sf $(shell command -v gsed) $(OUT)/host/sed

MUSL_OVERLAY := $(shell find musl -type f)

# The release minus its FP/SIMD assembly (the generic C versions take over), plus the overlay.
$(SYSROOT)/lib/libc.a: $(DL)/$(MUSL).tar.gz $(MUSL_OVERLAY) | $(OUT)/host/ld.lld
	rm -rf $(OUT)/musl && mkdir -p $(OUT)/musl
	tar xzf $< -C $(OUT)/musl --strip-components 1
	cd $(OUT)/musl && rm -r src/math/aarch64 src/fenv/aarch64 src/string/aarch64
	cp -R musl/. $(OUT)/musl/
	cd $(OUT)/musl && ./configure --target=aarch64-linux-musl --prefix=$(SYSROOT) --disable-shared \
		CC="$(MOG_CLANG)" CFLAGS="$(CFLAGS)" AR=$(LLVM)/llvm-ar RANLIB=$(LLVM)/llvm-ranlib >/dev/null
	$(MAKE) -C $(OUT)/musl install >/dev/null

$(OUT)/bin/%: %.c $(SYSROOT)/lib/libc.a mog-cc link.ld
	mkdir -p $(OUT)/bin
	$(CC) -s -o $@ $<

$(OUT)/bin/busybox: $(DL)/$(BUSYBOX).tar.bz2 busybox.config $(SYSROOT)/lib/libc.a mog-cc link.ld
	rm -rf $(OUT)/busybox && mkdir -p $(OUT)/busybox $(OUT)/bin
	tar xjf $< -C $(OUT)/busybox --strip-components 1
	cp busybox.config $(OUT)/busybox/.config
	PATH=$(HOSTPATH) $(MAKE) -C $(OUT)/busybox $(BUSYBOX_VARS) oldconfig </dev/null >/dev/null
	PATH=$(HOSTPATH) $(MAKE) -C $(OUT)/busybox $(BUSYBOX_VARS) busybox_unstripped >/dev/null
	$(LLVM)/llvm-strip -o $@ $(OUT)/busybox/busybox_unstripped

.PHONY: all
