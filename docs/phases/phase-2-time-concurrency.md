# Phase 2: Time and Concurrency

Goal: the kernel runs several tasks and preempts them on a timer.

## Steps

| # | Step | Done when |
| --- | --- | --- |
| 7 | Interrupt controller (GICv2/v3) + generic timer | A periodic tick increments a counter while the kernel does other work. |
| 8 | Tasks + context switch | Two kernel tasks with their own stacks alternate printing. |
| 9 | Scheduler | Preemptive round-robin on the timer tick; switch cost measured. |
| 10 | Synchronization | Spinlock and interrupt-masking primitives; no data races across interrupt handlers. |

## Notes

- The scheduler is a hot path: no heap allocation or dynamic dispatch on switch.
- Decide monolithic vs microkernel before leaving this phase.
