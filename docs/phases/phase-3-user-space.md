# Phase 3: User Space

Goal: programs run at EL0, isolated from the kernel and from each other.

## Steps

| # | Step | Done when |
| --- | --- | --- |
| 11 | EL0 + system calls | A user program calls the kernel via `svc` and gets a result back. |
| 12 | Address spaces / processes | Kernel moves to the higher half (TTBR1); each process has its own TTBR0 table; touching kernel or foreign memory faults. |
| 13 | Program loading | An ELF embedded in the kernel image is loaded and run. |
| 14 | IPC | Processes exchange messages; latency measured. |

## Notes

- The syscall ABI and IPC design depend on the kernel-architecture decision (see ROADMAP open decisions).
