# Phase 3: User Space

Goal: isolated programs at EL0 on a small, capability-based native ABI.

## Steps

Clean-boot e2e tests assert no `panic:` line; a faulting process never takes the kernel down.

| # | Step | Done when |
| --- | --- | --- |
| 11 | Address spaces + EL0 + first syscall | 4 KiB pages; each process has an ASID-tagged TTBR0 table (ASID = process slot, at most 255 processes) holding the kernel's EL1-only identity blocks plus user mappings at 4 GiB and above (no higher-half kernel until Linux compatibility needs it). A user program (hand-written asm blob; no user Rust crate until step 14) calls `svc` and gets a result. With the timer on, process B reading A's address is terminated: the kernel prints `fault: <proc>`, A keeps printing, no `panic:`. Syscall round-trip benchmark recorded. |
| 12 | Handle table + rights | Each process reaches kernel objects only through its handle table; handles carry rights (read, write, map, duplicate, transfer, exec, wait, kill) and a generation, so a closed handle never reaches a recycled object. init starts with: console, the boot-archive directory, itself. e2e: writing to the console succeeds; a duplicate without write fails with a named error; a closed handle fails. |
| 13 | Memory budgets, no overcommit | Map allocates and zeroes frames immediately (no demand paging) and charges the process budget; so do the kernel objects it causes (page tables, kernel stack, handle table, pipe buffers). Over budget, the call fails. e2e: a program creating pipes or mapping memory in a loop gets an error and keeps running; no `panic:`. |
| 14 | Spawn from the boot archive | A cpio (newc) archive built on the host and bundled with `include_bytes!` holds static ELF programs (`PT_LOAD` only). `spawn(exe_handle, handles[])` takes an executable handle opened from a directory handle; passed handles are moved (the parent duplicates any it keeps); budget is moved from parent to child, so budgets never exceed RAM. e2e: a child using a handle value it was not given gets an error. |
| 15 | Completion I/O, pipes, blocking | I/O is submit/complete on handles; one `io_submit_wait` syscall covers the blocking case (libc `read`/`write` use it). Pipes are the first I/O object; `wait` returns exit status. Blocked tasks are not scheduled; with nothing runnable the CPU idles in `wfi`. e2e: the parent blocks on an empty pipe until the child writes. Pipe round-trip benchmark recorded. |
| 16 | Mutex + priority inheritance | Strict priorities, round-robin within a level. A mutex handle (`lock`/`unlock`, kernel tracks the owner) is shareable by handle transfer. e2e: L takes the mutex, H blocks on it, Mid spins forever at middle priority; the test powers off only from H after it acquires, which happens only with inheritance. |

## Native ABI rules

- Small: target 20-30 syscalls, each taking handles; no `ioctl`-style multiplexers; no global names (paths resolve from directory handles).
- Kernel objects live in fixed-capacity tables per type (index + generation); creation fails when a table is full. No `Box`/`Arc` for kernel objects: `Box::try_new`/`Arc::try_new` are unstable, and an aborting allocation would break the no-overcommit rule.
- Syscalls do bounded work: IRQs are masked in trap context, so the longest syscall bounds IRQ latency.

## Task context

- User FP/SIMD stays off (`CPACR_EL1.FPEN = 0`) until a test needs it: a user FP instruction traps and kills the process. When enabled (`FPEN = 0b11`, which also allows it at EL1; the kernel stays FP-free only because of the softfloat target), FP/SIMD registers are saved and restored on process switch, never on every trap.
- Syscall convention: `x8` = number, `x0`-`x5` = arguments, `x0` = result (negative = error), `svc #0`; matches what musl expects.
- `SP_EL0` and `TPIDR_EL0` (thread pointer for libc) are part of the context.

## What was done

Filled in as each step lands.
