# `c/` - musl, busybox and C programs

## What this is

The C userland: `Makefile` fetches musl 1.2.5 and busybox 1.36.1 (pinned SHA-256) into `third_party/` and builds
them into `target/c/` with Homebrew clang and the pinned toolchain's `rust-lld`; `crates/board/qemu-virt/build.rs`
runs it and bundles `target/c/bin/busybox` (as `sh`), `hello` and `cbench` into the boot archive. `musl/` is copied
over the musl release before it builds; `src/mogos/mogos.c` there is the MogOs syscall layer.

It is **NOT** a Linux compatibility layer for unmodified binaries (phase 9), and **NOT** a kernel interface: the
kernel's ABI is `crates/kernel/src/syscall.rs`, mirrored here by hand (numbers, rights, `KILLED`).

## Boundaries (hard)

- Soft-float (`-march=armv8-a+nofp -mabi=aapcs-soft`): the kernel never enables FP/SIMD, so musl's FP assembly
  (`src/{math,fenv,string}/aarch64`) is deleted before the build and `setjmp`/`fp_arch.h` are overridden; libcalls
  come from the toolchain's `compiler_builtins` rlib. Any FP instruction fails to assemble.
- Every Linux syscall goes through `__mog_syscall` (`musl/arch/aarch64/syscall_arch.h`); an unmapped number is
  `-ENOSYS`. `clone`/`fork` are `ENOSYS`, so `pthread_create` fails and musl's locks stay no-ops (one thread); the
  kernel mutex calls are not used (phase 5 replaces them with futexes).
- C programs link with `mog-cc`: static, `link.ld` (one RX and one RW `PT_LOAD` at 4 GiB, as the kernel's ELF
  parser requires), no crt1 stack layout: `__mog_start` builds argc/argv/envp/auxv (`AT_PAGESZ`) on a 128 KiB
  stack it maps.
- No system-wide installs: clang from `/opt/homebrew/opt/llvm`, `gsed` for busybox's kbuild, `curl`, `shasum`.

## The libc ABI (invariants)

- Handles at start: 0-2 stdin, stdout, stderr; 3 the root directory; 4 the boot archive. An absent one fails on
  use. libc spawns children with the same five (a transfer-only placeholder for an absent slot).
- Arguments: when the first string is `<argc> /<cwd>`, the next `argc` are argv and the rest envp, and the process
  starts in `<cwd>`; otherwise all strings are argv and it starts at the root. libc and msh's `Grant::Posix` write
  the header.
- libc state: the fd table (fd -> open file: handle, offset, append flag, path below the root), the current
  directory as a path below the root (every path resolves lexically, `..` stops at the root, then through handle 3),
  and the pid table (pid -> process handle). Offsets and the append flag do not cross a spawn.
- `vfork` returns 0 in a child mode on the parent's stack; fd changes then work on the live table, saved at
  `vfork`; `execve` spawns natively and `_exit` records an exited child, and both restore the table and return the
  child's pid from `vfork`. `execve` outside a child spawns, waits and exits with the child's code.
- Programs come only from the boot archive, by the last component of the path (`/bin/sh` is busybox).
- Signals: `kill` of SIGKILL, SIGTERM, SIGINT, SIGQUIT, SIGHUP kills a child through its process handle (the
  caller itself exits `128 + sig`); others are ignored; `sigaction` and `sigprocmask` succeed and do nothing.
- Memory: `brk` fails so malloc uses `mmap`, which chains 64 KiB native `map`s (the kernel places them
  contiguously); `munmap`, `mprotect`, `madvise` do nothing, so freed mappings stay charged to the budget.
- No stat call: a file's kind comes from a zero-length `readdir` (`ENOTDIR` for a file), its size from a binary
  search of one-byte reads (at most 17), its inode from a hash of its path. `TIOCGWINSZ` answers 80x24 on the stdio
  fds, any other ioctl is `ENOTTY`.
- A spawned child gets up to 1024 frames of the parent's budget, halved on `ENOMEM` down to 128.

## How it's tested

- End to end in `crates/e2e/tests/boot.rs`: `a_c_program_on_musl_prints_gets_enosys_and_exits_with_its_code`,
  `busybox_sh_changes_files_that_survive_a_reboot_once_synced`, `musl_bench_reports_round_trips` (`cbench`).
- `make -C c OUT=$PWD/target/c` alone builds everything; it is a no-op when nothing changed.

---

> After changing anything here, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes stale
> are updated in the same commit; changing a boundary, rule or invariant needs explicit user approval.
