# Phase 3: User Space

Goal: isolated programs at EL0 on a small, capability-based native ABI.

## Steps

| # | Step | Done when |
| --- | --- | --- |
| 11 | Address spaces + EL0 + first syscall | Kernel moves to the higher half (TTBR1); 4 KiB pages; each process gets an ASID-tagged TTBR0 table. A user program calls `svc` and gets a result; reading a kernel address faults; process B reading A's address gets a fault, not A's data. |
| 12 | Handle table + rights | Every kernel object a process uses (memory, tasks, later files, pipes, timers) is reached only through a per-process handle table; each handle carries rights (read, write, map, duplicate, transfer). Using a handle without the right fails with an error; there is no global path or object namespace in the native ABI. |
| 13 | Memory budgets, no overcommit | Each process has a memory budget charged on map, not on touch. Exceeding it fails the call with an error; nothing is ever killed for memory. e2e: a program allocating past its budget gets the error and keeps running. |
| 14 | Program loading + spawn | A read-only cpio (newc) archive bundled with the kernel holds ELF programs. `spawn(program, handles)` starts a process with exactly the handles passed to it, and nothing else. |
| 15 | Processes + pipes | `wait` for exit status; pipes as handles; a spawned child writes through a pipe and the parent reads it. Pipe round-trip benchmark recorded. |
| 16 | Priorities + priority inheritance | Fixed priorities on top of round-robin; a high-priority task blocked on a lock held by a low-priority task boosts it (e2e scenario proves the inversion is bounded). |

## Native ABI rules

- Small: target 20–30 syscalls, each taking handles; no `ioctl`-style multiplexers.
- POSIX lives in libc (phase 4): `fork` is built on native primitives after `spawn` works; signals are emulated in libc over native events; paths resolve through handles to directories.
- No overcommit anywhere, including the kernel heap: allocation failure returns an error; the kernel does not panic on out-of-memory.

## Task context (from the softfloat kernel)

- User FP/SIMD is enabled (`CPACR_EL1.FPEN = 0b11`) and its registers are saved and restored only on process switch, never on every trap.
- `SP_EL0` and `TPIDR_EL0` (thread pointer for libc) are part of the context.

## What was done

Filled in as each step lands.
