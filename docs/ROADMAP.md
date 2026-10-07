# Roadmap

Ordered by dependency: each phase builds on the previous one.

| Phase | Goal | Doc | Status |
| --- | --- | --- | --- |
| 1 | Foundations: the kernel survives on its own | [phase-1-foundations.md](phases/phase-1-foundations.md) | Done |
| 2 | Time and concurrency: the kernel multitasks | [phase-2-time-concurrency.md](phases/phase-2-time-concurrency.md) | Done |
| 3 | User space: isolated programs, capability-based native ABI | [phase-3-user-space.md](phases/phase-3-user-space.md) | Done |
| 4 | Shell, async I/O, MogFS | [phase-4-io-storage.md](phases/phase-4-io-storage.md) | Not started |

Later: Linux binary-compatibility layer, multicore (SMP), networking, graphics, power management, real hardware.

## Ongoing in every phase

- Host tests: pure-logic crates are `cargo test`-ed on macOS.
- QEMU integration: `cargo run` boots, prints progress, and powers off.
- Debugging: LLDB over QEMU's gdbstub; readable panic and fault dumps.
- Benchmarks ([BENCHMARKS.md](BENCHMARKS.md)): each hot path gets one when it lands (phase 1: frame allocation, boot time; phase 2: yield round trip; phase 3: syscall, pipe; phase 4: virtio-blk throughput).

## Differentiators vs Linux (when each lands)

Core decisions live in AGENTS.md; this is where each differentiator is scheduled.

| Differentiator | Lands in |
| --- | --- |
| Memory safety by construction (`unsafe` only in `arch`/`board`) | Phase 1 (done) |
| No overcommit: fallible allocation, per-process memory budgets, no OOM killer | Rule now; fixed-capacity kernel object tables and budgets in phase 3 step 13 |
| Capabilities: per-process handle table with rights, no ambient authority | Phase 3 step 12 |
| Small, async-first native ABI: about 20-30 syscalls, `spawn` not `fork`, completion-based I/O | Phase 3 steps 11-15 (completion I/O from the first pipe) |
| Deterministic scheduling: priorities, priority inheritance, bounded IRQ latency | Phase 3 step 16 (done; a wake does not preempt yet); IRQ latency bounded by the longest syscall (measured under hvf from phase 3) |
| MogFS: checksummed copy-on-write filesystem with atomic commits | Phase 4 steps 21-22 |

## Open decisions

- Rust `std` for MogOs programs: a custom OS target needs nightly (`build-std`) or upstreaming into rustc, against the stable-only rule. Not needed by phases 3-4 (user programs are `no_std` Rust or C on musl). A stable route to verify: build for `aarch64-unknown-linux-musl` and link against MogOs' musl, relying on its Linux-number dispatcher.
