# Roadmap

Ordered by dependency: each phase builds on the previous one.

| Phase | Goal | Doc | Status |
| --- | --- | --- | --- |
| 1 | Foundations: the kernel survives on its own | [phase-1-foundations.md](phases/phase-1-foundations.md) | Done |
| 2 | Time and concurrency: the kernel multitasks | [phase-2-time-concurrency.md](phases/phase-2-time-concurrency.md) | Not started |
| 3 | User space: isolated programs | [phase-3-user-space.md](phases/phase-3-user-space.md) | Not started |
| 4 | I/O and storage | [phase-4-io-storage.md](phases/phase-4-io-storage.md) | Not started |

Later: multicore (SMP), networking, graphics, power management, real hardware.

## Ongoing in every phase

- Host tests: pure-logic crates are `cargo test`-ed on macOS.
- QEMU integration: `cargo run` boots, prints progress, and powers off.
- Debugging: LLDB over QEMU's gdbstub; readable panic and fault dumps.
- Benchmarks ([BENCHMARKS.md](BENCHMARKS.md)): each hot path gets one when it lands (phase 1: frame allocation, boot time; phase 2+: exception entry, context switch, syscall, IPC).

## Open decisions (settle before phase 3)

- Monolithic vs microkernel vs hybrid (shapes IPC and syscalls).
- Unix "everything is a file" vs capability handles.
