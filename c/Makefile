# C userland for MogOs: musl with the MogOs syscall layer (`musl/` is copied over the release), busybox, and the C
# test programs. One cache serves every worktree, in the main checkout's `target/c-cache`: sources are fetched into
# `dl/` and checked against SHA-256, and everything builds into `<key>/`, where the key hashes every input below and
# the toolchain, so a worktree with the same inputs reuses the build and a changed input gets its own.
# `$(ROOT)/target/c` links to that directory. The board's build.rs runs `make -C c`; with a built key it does nothing.

ROOT := $(abspath $(CURDIR)/..)
MAIN := $(abspath $(dir $(shell git -C $(CURDIR) rev-parse --path-format=absolute --git-common-dir)))
INPUTS := Makefile mog-cc link.ld busybox.config hello.c cbench.c oscb.c oscnop.c tcpecho.c $(sort $(shell find musl -type f))
KEY := $(shell { cat $(INPUTS); rustc --version; } | shasum -a 256 | cut -c1-16)
CACHE := $(MAIN)/target/c-cache
OUT := $(CACHE)/$(KEY)
DL := $(CACHE)/dl

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

PROGRAMS := hello cbench oscb oscnop tcpecho
BINS := $(OUT)/bin/busybox $(PROGRAMS:%=$(OUT)/bin/%)

# One C build at a time across worktrees (lockf), so two agents never build into the same key.
all:
	mkdir -p $(CACHE)
	lockf -k $(CACHE)/.lock $(MAKE) build
	@[ -L $(ROOT)/target/c ] || rm -rf $(ROOT)/target/c
	ln -sfn $(OUT) $(ROOT)/target/c

build: $(BINS)

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

$(OUT)/host/sed:
	@command -v gsed >/dev/null || { echo "busybox needs GNU sed: brew install gnu-sed" >&2; exit 1; }
	mkdir -p $(OUT)/host
	ln -sf "$$(command -v gsed)" $@

# Outputs depend on the tarballs only: the key already changes with every input, and a fresh checkout's newer mtimes
# must not rebuild a key that is built.

# The release minus its FP/SIMD assembly (the generic C versions take over), plus the overlay.
$(SYSROOT)/lib/libc.a: | $(DL)/$(MUSL).tar.gz $(OUT)/host/ld.lld
	rm -rf $(OUT)/musl && mkdir -p $(OUT)/musl
	tar xzf $(DL)/$(MUSL).tar.gz -C $(OUT)/musl --strip-components 1
	cd $(OUT)/musl && rm -r src/math/aarch64 src/fenv/aarch64 src/string/aarch64
	cp -R musl/. $(OUT)/musl/
	cd $(OUT)/musl && ./configure --target=aarch64-linux-musl --prefix=$(SYSROOT) --disable-shared \
		CC="$(MOG_CLANG)" CFLAGS="$(CFLAGS)" AR=$(LLVM)/llvm-ar RANLIB=$(LLVM)/llvm-ranlib >/dev/null
	$(MAKE) -C $(OUT)/musl install >/dev/null

$(OUT)/bin/%: | $(SYSROOT)/lib/libc.a
	mkdir -p $(OUT)/bin
	$(CC) -s -o $@ $*.c

$(OUT)/bin/busybox: | $(DL)/$(BUSYBOX).tar.bz2 $(SYSROOT)/lib/libc.a $(OUT)/host/sed
	rm -rf $(OUT)/busybox && mkdir -p $(OUT)/busybox $(OUT)/bin
	tar xjf $(DL)/$(BUSYBOX).tar.bz2 -C $(OUT)/busybox --strip-components 1
	cp busybox.config $(OUT)/busybox/.config
	PATH=$(HOSTPATH) $(MAKE) -C $(OUT)/busybox $(BUSYBOX_VARS) oldconfig </dev/null >/dev/null
	PATH=$(HOSTPATH) $(MAKE) -C $(OUT)/busybox $(BUSYBOX_VARS) busybox_unstripped >/dev/null
	$(LLVM)/llvm-strip -o $@ $(OUT)/busybox/busybox_unstripped

.PHONY: all build
