# Roadmap

Ordered by dependency: each phase builds on the previous one.

| Phase | Goal | Doc | Status |
| --- | --- | --- | --- |
| 1 | Foundations: the kernel survives on its own | [phase-1-foundations.md](phases/phase-1-foundations.md) | Done |
| 2 | Time and concurrency: the kernel multitasks | [phase-2-time-concurrency.md](phases/phase-2-time-concurrency.md) | In progress |
| 3 | User space: isolated programs, capability-based native ABI | [phase-3-user-space.md](phases/phase-3-user-space.md) | Not started |
| 4 | Shell, async I/O, MogFS | [phase-4-io-storage.md](phases/phase-4-io-storage.md) | Not started |

Later: Linux binary-compatibility layer, multicore (SMP), networking, graphics, power management, real hardware.

## Ongoing in every phase

- Host tests: pure-logic crates are `cargo test`-ed on macOS.
- QEMU integration: `cargo run` boots, prints progress, and powers off.
- Debugging: LLDB over QEMU's gdbstub; readable panic and fault dumps.
- Benchmarks ([BENCHMARKS.md](BENCHMARKS.md)): each hot path gets one when it lands (phase 1: frame allocation, boot time; phase 2: yield round trip; phase 3: syscall, pipe; phase 4: I/O ring).

## Differentiators vs Linux (when each lands)

Core decisions live in AGENTS.md; this is where each differentiator is scheduled.

| Differentiator | Lands in |
| --- | --- |
| Memory safety by construction (`unsafe` only in `arch`/`board`) | Phase 1 (done) |
| No overcommit: fallible allocation, per-process memory budgets, no OOM killer | Rule now; budgets in phase 3 step 13 |
| Capabilities: per-process handle table with rights, no ambient authority | Phase 3 step 12 |
| Small, async-first native ABI: about 20-30 syscalls, `spawn` not `fork`, completion-based I/O | Phase 3 (ABI, spawn); phase 4 step 20 (I/O ring) |
| Deterministic scheduling: priorities, priority inheritance, bounded IRQ latency | Phase 3 step 16; latency measured once a meaningful measurement exists |
| MogFS: checksummed copy-on-write filesystem with atomic transactions | Phase 4 step 21 |

## Open decisions

- Rust `std` for MogOs: a custom OS target needs nightly (`build-std`) or upstreaming the target into rustc, but the toolchain rule is stable-only. Decide before phase 4.
