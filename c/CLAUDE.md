# `c/` - musl, busybox and C programs

## What this is

The C userland: `Makefile` fetches musl 1.2.5 and busybox 1.36.1 (pinned SHA-256) and builds them with Homebrew
clang and the pinned toolchain's `rust-lld` into one cache shared by every worktree, the main checkout's
`target/c-cache/<key>`, the key a hash of the inputs and the toolchain (`docs/DEVELOPMENT.md`); `target/c` links to
it. `crates/board/qemu-virt/build.rs` runs it and bundles `target/c/bin/busybox` (as `sh`), `hello`, `cbench`, and
`oscb` with its spawn target `oscnop` (the cross-OS benchmarks, `scripts/oscompare.sh`) into the boot archive.
`musl/` is copied over the musl release before it builds; `src/mogos/mogos.c` there is the MogOs syscall layer.

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

- Handles at start: 0-2 stdin, stdout, stderr; 3 the root directory; 4 the boot archive; 5 the NetStack. An absent
  one fails on use. libc spawns children with the first five (a transfer-only placeholder for an absent slot), never
  the NetStack: a child inherits no network (least privilege); a program that needs one is spawned natively with
  it.
- Sockets: `AF_INET` `SOCK_STREAM` only. `socket` makes a native socket on handle 5; `bind` takes the port and
  `INADDR_ANY` (every interface) or 127.0.0.1 (loopback only; any other address is `EADDRNOTAVAIL`); `connect`, `accept`, `read`/`write`,
  `send`/`recv`(`to`/`from`) submit one native op and wait for it at once (one thread, so it is the only one in
  flight); `listen` passes its backlog (the kernel clamps it to 1..=8 and charges it at once); `shutdown` ends the send side (`SHUT_RD` alone does nothing); `setsockopt(SO_REUSEADDR)` succeeds (ports
  rebind once closed), other options are `ENOPROTOOPT`; `accept` fills `sockaddr_in` with the peer's address and sets `*addrlen` (a loopback peer is 127.0.0.1).
- Arguments: when the first string is `<argc> <stdin> <stdout> <stderr> /<cwd>` (each stdio fd `t` console, `p`
  pipe, `d` directory, `f<offset>` a file, `a<offset>` a file opened to append; msh's `Grant::Posix` writes
  `<argc> /<cwd>`, stdio on the console), the next `argc` are argv and the rest envp, and the process starts in
  `<cwd>`; otherwise all strings are argv and it starts at the root with stdio on the console. It is parsed before
  TLS exists, so without libc calls that may set errno.
- libc state: the fd table (fd -> open file: handle, offset, append flag, path below the root), the current
  directory as a path below the root (every path resolves lexically, `..` stops at the root, then through handle 3),
  and the pid table (pid -> process handle). A redirected stdio fd crosses a spawn with its kind and offset; the
  offset is the child's own from then on (no shared offset with the parent).
- `vfork` returns 0 in a child mode on the parent's stack; fd changes then work on the live table, saved at
  `vfork`; `execve` spawns natively and `_exit` records an exited child, and both restore the table and return the
  child's pid from `vfork`. `execve` outside a child spawns, waits and exits with the child's code.
- Programs come only from the boot archive, by the last component of the path (`/bin/sh` is busybox), and only the
  C programs (`sh`, `hello`, `cbench`, `oscb`, `oscnop`): the native ones expect other handles, and `sh` must not
  reach programs outside msh's table (`ENOENT`).
- `wait4` has no native wait-for-any: it takes an exited pseudo-child, else blocks on the newest child (the
  foreground one); `WNOHANG` sees only pseudo-children, so background jobs (`&`) are not reaped until waited for.
  A vfork inside a vfork child is `EAGAIN`.
- Signals: `kill` of SIGKILL, SIGTERM, SIGINT, SIGQUIT, SIGHUP kills a child through its process handle (the
  caller itself exits `128 + sig`); others are ignored; `sigaction` and `sigprocmask` succeed and do nothing.
- Memory: `brk` fails so malloc uses `mmap`, which chains 64 KiB native `map`s (the kernel places them
  contiguously); `munmap`, `mprotect`, `madvise` do nothing, so freed mappings stay charged to the budget.
- No stat call: a file's kind comes from a zero-length `readdir` (`ENOTDIR` for a file), its size from a binary
  search of one-byte reads (a probe at 64 KiB, doubling while it reads, then a bisect: 17 under 64 KiB, about 2 log2
  of the size above), its inode from a hash of its path. `getdents` keeps the native `readdir` cursor as the directory
  offset in one native call sized so every listed name fits as a dirent; every entry of one call shares it as `d_off`,
  so a `seekdir` to a `telldir` taken mid-call resumes after that call's entries. `TIOCGWINSZ` answers 80x24 on the stdio
  fds, any other ioctl is `ENOTTY`.
- A spawned child gets up to 1024 frames of the parent's budget, halved on `ENOMEM` down to 128.

## How it's tested

- End to end in `crates/e2e/tests/boot.rs`: `a_c_program_on_musl_prints_gets_enosys_and_exits_with_its_code`,
  `busybox_sh_changes_files_that_survive_a_reboot_once_synced`, `musl_bench_reports_round_trips` (`cbench`),
  `oscb_runs_the_cross_os_benchmarks`, and `tcpecho` (server and client on BSD sockets) in
  `sockets_echo_over_loopback_wait_for_any_and_need_the_net_handle_and_budget`.
- `make -C c` alone builds everything; it is a no-op once the key is built. A new input file in `c/` must join
  `INPUTS` in the Makefile, or a change to it reuses a stale build.

---

> After changing anything here, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes stale
> are updated in the same commit; changing a boundary, rule or invariant needs explicit user approval.
