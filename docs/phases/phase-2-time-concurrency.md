# Phase 2: Time and Concurrency

Goal: the kernel runs several tasks and preempts them on a timer.

## Steps

Each done-when is an e2e assertion on the serial output; every e2e boot also asserts no `panic:` line.

| # | Step | Done when |
| --- | --- | --- |
| 7a | Prep for preemption | Kernel builds for `aarch64-unknown-none-softfloat` (no FP/SIMD in the kernel, so nothing to save on traps). Heap uses an IRQ-masking lock (no spinlock that can deadlock on one core). `kernel` defines `IrqMask` + `IrqLock`; `arch` provides the safe mask/restore. Existing e2e tests still pass. |
| 7 | GICv2 + EL1 virtual timer | QEMU pinned to `gic-version=2`; GIC base read from the DTB. The virtual timer (PPI 27, period derived from `CNTFRQ_EL0`) ticks while the kernel idles in `wfi`; after 20 ticks of 10 ms it prints `ticks: 20` and powers off. |
| 8 | Tasks + context switch | The trap frame is the task context; each task has its own kernel stack. Two tasks yielding via `svc #0` print `task a: 0`, `task b: 0`, `task a: 1`, ... in strict alternation. |
| 9 | Preemptive scheduler | Round-robin on the timer tick: one task busy-loops without yielding while the other still prints `task b: N` lines, proving preemption. |
| 10 | Synchronization | `IrqLock` guards all shared kernel state (scheduler, heap); no allocation in IRQ context; rules documented. |

## Benchmarks (hvf medians gate, TCG informational)

- IRQ latency: timer deadline (`CNTV_CVAL_EL0`) vs `CNTVCT_EL0` read at handler entry.
- Yield round trip: two tasks ping-ponging N times, elapsed / N.

## Notes

- The scheduler and trap path are hot paths: no heap allocation or dynamic dispatch on switch.
- One switch mechanism: IRQ preemption and `svc` yield both return the next task's trap frame.
- Spinning (multicore) is out of scope; on one core the lock is IRQ masking.

## What was done

Filled in as each step lands.
