# MogOs

A small operating system written in Rust. Goals, in priority order: **simple, fast, efficient**.

## Core decisions

- Monolithic kernel built from safe subsystem crates (`forbid(unsafe_code)`); `unsafe` only in `arch` and `board`.
- POSIX-compatible at the source level: native MogOs syscall ABI, a ported libc (musl) and a Rust `std` target. Programs are recompiled for `aarch64-unknown-mogos`.
- Optional Linux binary-compatibility layer later, translating Linux syscalls to native ones; it must never shape kernel internals.
- Built to beat Linux where its legacy blocks it: memory safety by construction, capabilities instead of ambient authority (handles with rights, no global namespace in the native ABI), a small async-first syscall ABI (`spawn`, completion-based I/O; `fork` and signals live in libc), no overcommit (allocation fails explicitly, never an OOM killer), deterministic scheduling, and MogFS (checksummed copy-on-write). Schedule: `docs/ROADMAP.md`.
- Kernel allocation is fallible: out-of-memory returns an error, never a panic.

## Docs (memory tree)

This file is the root. Load child docs only when the task needs them.

```
AGENTS.md
├── docs/DEVELOPMENT.md               toolchain, build/lint settings, test commands, rules for agents
├── docs/BENCHMARKS.md                benchmark kinds, workflow, regression rule, baselines
├── docs/WORKFLOW.md                  development loop, which agent does each step, reviewer pass
└── docs/ROADMAP.md                   phases, status, open decisions
    ├── docs/phases/phase-N-*.md      steps, done-when, what was done
    └── docs/research/linux-survey.md what Linux gets right/wrong; source of phases 5-12
```

Each crate carries a `CLAUDE.md` ownership contract (what it is and is not, boundaries, invariants, how it is
tested), auto-loaded when an agent works in that crate and linked from Layout below. A new crate gets one when it lands.

Every new `.md` file must be linked from its parent so it stays reachable from this root, and this tree (or Layout, for crate docs) must be updated when one is added.

## Target

- Architecture: AArch64 (matches the Apple Silicon host).
- Machine: QEMU `virt`, `cortex-a72`, 4 cores, 128 MiB RAM.
- Toolchain: stable Rust, target `aarch64-unknown-none-softfloat` (pinned in `rust-toolchain.toml`, links with bundled `rust-lld`).

## Commands

- Build: `cargo build`
- Run in QEMU: `cargo run` (prints to the terminal via PL011 UART, exits via PSCI `SYSTEM_OFF`)
- Shell on a persistent disk: `[ -f disk.img ] || cargo mkfs; cargo shell` (msh builtins `cd`, `pwd`, `exit`, `help`; programs `ls`, `mkdir`, `touch`, `write`, `cat`, `rm`, `mv`, `echo`, `sync`, and `sh`: busybox on musl)
- HTTP echo server on QEMU's user network: `cargo httpd`, then `curl -v http://localhost:8080/anything -d hello` from the Mac shows its own request back
- Run in a QEMU window: `cargo window` (mouse stays free; Ctrl+Option+G releases a grab)
- Quit a hung QEMU: `Ctrl-A` then `X`
- Test: `cargo test-host` (host tests, `crates/user`'s included, plus the QEMU boot tests in `crates/e2e`; must pass)
- Benchmarks: `cargo bench-host` (host); kernel boot time is the `boot: <N> us` line; per-call and per-command A/B under hvf: `scripts/bench.sh` (`docs/BENCHMARKS.md`)
- Debug: `cargo run -- -s -S`, then attach `lldb` / `gdb` to `localhost:1234`
- Lint/format: `cargo clippy`, `cargo fmt` (must be clean; `crates/user` is outside the workspace, see `docs/DEVELOPMENT.md`). Settings, rules for agents: `docs/DEVELOPMENT.md`
- Roadmap and current phase: `docs/ROADMAP.md` (one doc per phase in `docs/phases/`; update "What was done" when a step lands)

## Layout

- `crates/kernel` ([CLAUDE.md](crates/kernel/CLAUDE.md)) — OS logic, `#![no_std]`, **no `unsafe`** (`forbid`). Defines ports (traits) like `Board` (and re-exports `mogfs`'s `Disk`), the scheduler and process table, handles, pipes, mutexes, the console line discipline, syscall decoding, the network (sockets over `crates/net`'s stacks) and the boot archive's cpio and ELF parsers.
- `crates/mm` ([CLAUDE.md](crates/mm/CLAUDE.md)) — arch-independent memory management (`PhysAddr`, frame allocator). Safe, host-tested.
- `crates/dtb` ([CLAUDE.md](crates/dtb/CLAUDE.md)) — minimal FDT parser. Safe, host-tested.
- `crates/mogfs` ([CLAUDE.md](crates/mogfs/CLAUDE.md)) — MogFS: checksummed copy-on-write file system over a `Disk` trait (format at the top of `src/lib.rs`). Safe, `no_std`, host-tested; `examples/mkfs.rs` writes an empty image.
- `crates/net` ([CLAUDE.md](crates/net/CLAUDE.md)) — network stack over a `Nic` trait: Ethernet, ARP, IPv4, ICMP echo, UDP, TCP with NewReno, in caller-supplied memory with time as an input. Safe, `no_std`, host-tested over a seeded simulated link (`tests/sim/mod.rs`).
- `crates/arch` ([CLAUDE.md](crates/arch/CLAUDE.md)) — the only arch-specific crate, `unsafe` allowed; AArch64 code in `src/aarch64/` (boot, traps, MMU and page tables, GICv2, timer, the lock and per-CPU primitives).
- `crates/mogfs2` ([CLAUDE.md](crates/mogfs2/CLAUDE.md)) — MogFS v2 (phase 7): a checksummed copy-on-write B+tree over v1's `Disk` (format at the top of `src/lib.rs`) in fixed memory its caller gives. Safe, `no_std`, no `alloc`, host-tested; replaces `crates/mogfs` and takes its name in step 39b.
- `crates/board/qemu-virt` ([CLAUDE.md](crates/board/qemu-virt/CLAUDE.md)) — board crate, `unsafe` allowed: drivers, memory map, `linker.ld`, `#[global_allocator]`, trap hooks (switch, syscall, fault), process setup and ELF loading, asm user programs (`user.s`); builds the `mog_os` binary. Its `build.rs` builds `crates/user` and bundles the programs as the boot archive (cpio).
- `crates/user` ([CLAUDE.md](crates/user/CLAUDE.md)) — user programs (`src/bin/*.rs`, static ELFs at 4 GiB via `link.ld`) and their syscall stubs (`src/lib.rs`); user space, outside the workspace, `unsafe` only for `svc`.
- `crates/e2e` ([CLAUDE.md](crates/e2e/CLAUDE.md)) — host-only QEMU boot tests (`tests/boot.rs`).
- `c` ([CLAUDE.md](c/CLAUDE.md)) — the C userland: musl with the MogOs syscall layer (`c/musl`), busybox, C test programs; `c/Makefile`, run by the board's `build.rs`, fetches the pinned sources and builds into one cache shared by every worktree (the main checkout's `target/c-cache`, keyed by a hash of the inputs); `target/c` links to it.
- `.cargo/config.toml` — default target, build/link thread caps, QEMU runners.
- `scripts/bench.sh` — boots kernels under hvf and prints each `bench` line's median and min, base vs new interleaved.

## Architecture

- Cargo workspace. Dependencies point inward: board crates depend on `kernel`, never the reverse. New subsystems (`mm`, `sched`, ...) get their own crate.
- `unsafe_code = "forbid"` is a workspace lint, so every crate is safe by default. Only board/arch crates opt out, and they expose safe wrappers.
- Hardware sits behind small traits (ports) implemented per board (adapters), so core logic stays hardware-free and testable on the host.
- Static dispatch (generics) across boundaries; no `dyn`, `Arc`, or heap in hot paths (exception entry, context switch, page faults).

## Development philosophy: test-driven

- Test-driven: write the failing test that states the expected behavior first, then the code that makes it pass.
- Prefer end-to-end and integration tests over unit tests. The main test is booting the kernel in QEMU and asserting on its serial output; next is testing a crate through its public API.
- Unit tests are for tricky pure logic only (parsers, allocators, encodings). Don't unit-test every function.
- A step is done when its end-to-end test passes, not when the code compiles.
- Every change follows `docs/WORKFLOW.md`: failing test, smallest diff, checks, then a `reviewer` pass for correctness and simplicity.

## Performance: benchmark-driven

- Speed is a feature, so it is measured, not assumed. Details and baselines: `docs/BENCHMARKS.md`.
- Every hot path gets a benchmark when it lands. Every change to a hot path reports before/after numbers.
- Speed with complete safety is the moat. A tracked benchmark slowing down (hvf or host medians, never TCG) is a failure: an explanation does not excuse it. Remove it, or show with numbers that no safe faster form exists.
- Invariants checked at compile time are part of the moat: they cost nothing at run time and their bug class cannot return.

## Code rules

- `#![no_std]`. Edition 2024: use `#[unsafe(no_mangle)]`, `unsafe extern`.
- Host-testable crates use `#![cfg_attr(not(test), no_std)]`.
- Make invalid states unrepresentable: typestate, newtypes and ownership when the state is known at compile time; exhaustive enums when it comes from input (packets, user handles, tables of mixed states). It must cost nothing at run time; a type-level encoding that adds code size or generic bloat on a hot path is measured.
- `unsafe` only in `crates/arch` and board crates (`crates/board/*`), each block with a one-line `// SAFETY:` reason; user space (`crates/user`) only for its syscall stubs.
- After editing `linker.ld`, `crates/board/qemu-virt/build.rs` triggers a relink automatically.
