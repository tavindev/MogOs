# Roadmap

Ordered by dependency: each phase builds on the previous one.

| Phase | Goal | Doc | Status |
| --- | --- | --- | --- |
| 1 | Foundations: the kernel survives on its own | [phase-1-foundations.md](phases/phase-1-foundations.md) | Done |
| 2 | Time and concurrency: the kernel multitasks | [phase-2-time-concurrency.md](phases/phase-2-time-concurrency.md) | Done |
| 3 | User space: isolated programs, capability-based native ABI | [phase-3-user-space.md](phases/phase-3-user-space.md) | Done |
| 4 | Shell, async I/O, MogFS | [phase-4-io-storage.md](phases/phase-4-io-storage.md) | Done (step 23: musl, busybox; `^C` deferred) |
| 5 | SMP with no practical core cap (512 under TCG), threads, fair scheduling, resource groups, no fixed object limits; the locking model fixed first, and the big lock split (per process, then per core) ahead of the per-core scheduler; a direct-switch call for ping-pong | [phase-5-smp-threads.md](phases/phase-5-smp-threads.md) | In progress (steps 24, 25a, 25b, 25c, 26 done) |
| 6 | Virtual memory: demand paging under no-overcommit, page cache, file mmap, CoW fork in libc, higher-half kernel, KASLR (kernel W^X moved to phase 10 step 60c); from phase 5: all RAM mapped (past the 3 GiB identity window), user image and stack sized from the ELF with a stack budget, kernel stack guard pages, the GIC's 256 GiB redistributor hole leaving user VA | [phase-6-virtual-memory.md](phases/phase-6-virtual-memory.md) | |
| 7 | Storage that scales: MogFS v2 (extents, snapshots, scrub), async block path, multi-queue NVMe | [phase-7-storage.md](phases/phase-7-storage.md) | In progress (steps 39, 39b done) |
| 8 | Networking: safe TCP/IP, sockets as handles, virtio-net | [phase-8-networking.md](phases/phase-8-networking.md) | In progress (steps 46, 49, 50, 51 done) |
| 9 | POSIX completeness and Linux binary compatibility; native ABI frozen | not written | |
| 10 | Observability, debugging, security hardening; early hardening baseline (Spectre-BHB, v1 masking, kernel W^X: steps 60a-60c) runs beside phase 5 | [phase-10-hardening.md](phases/phase-10-hardening.md) | Steps 60a-60c done; 60-66 not written |
| 11 | Real hardware, boot, power | not written | |
| 12 | Graphics, desktop, virtualization | not written | |

Phases 5-12 come from [research/linux-survey.md](research/linux-survey.md): what Linux gets right that we must match, what it got wrong, and the order (dependencies first, then the largest competitive gain per effort). Each phase doc is written and plan-reviewed for simplicity and speed before its first step.

## Decided (from the Linux survey)

- Page cache is kernel-owned and reclaimable under a global cap, outside per-process budgets; budgets cover anonymous memory and kernel objects (Linux charging cache to the first toucher is its worst memcg complaint).
- Demand paging keeps no-overcommit: the budget is the commit charge, frames materialize lazily.
- The locking model (spinlocks, per-CPU cells in `arch`/`board`, a safe `Lock<T>` for the kernel) is fixed before SMP or any new shared table.
- Scheduling: the strict-priority RT class with priority inheritance stays above a fair (EEVDF-style) class.
- `fork` is not in the native ABI; libc builds it as a budget-charged CoW clone for compat, so a fork can fail cleanly. `spawn` stays the fast path.
- Signals: the kernel offers an exception channel and a notify bit on handles; libc builds POSIX signals from them.
- Containers need no namespaces: a container is a process tree with restricted handles.
- Completion I/O avoids io_uring's traps: a small fixed op set checked against handle rights at submit, no kernel worker with the caller's authority, ring memory charged to the budget.
- All drivers stay in the kernel (monolithic); Rust safety with `unsafe` confined to arch/board/driver code is the isolation.
- The native ABI is frozen after Linux compatibility (phase 9) has exercised it, not before.
- No practical core cap and no fixed object limits (user, 2026-10-07): GICv3, per-CPU areas sized at boot, tree bring-up and a queued lock; every object bounded by its owner's budget, and a child spawned without an explicit budget charges its parent's resource group, like a cgroup (phase 5 steps 25c, 30-32).

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
