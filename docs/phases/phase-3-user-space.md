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

- Step 11a: the kernel's two 1 GiB blocks stay in a static boot table (EL1-only: UXN added to RAM, global) and are copied into each process's level-1 table; `arch::map_page` walks L1, L2, L3, taking missing tables from a fallible frame allocator, and `user_page` makes 4 KiB EL0 leaves (`nG`, PXN; code read-only and executable, stack read-write and UXN), with build-time asserts on every descriptor. A scheduler slot holds a frame and an address space: 0 is the boot table with ASID 0, otherwise the process's level-1 table with ASID = slot (`MAX_TASKS` 8, asserted at most 256). `task_switch` writes TTBR0 (+ `isb`, no TLB flush) and swaps SP_EL0 only when the space changes, so kernel-only switches pay nothing: SP_EL0 lives in the trap frame's spare slot, written by the switch, not by the vector asm (saving it on every trap cost about 2 ns of the 63 ns yield). hvf yield median 63/63 ns, boot 177/176 us (before/after, 21 interleaved).
- Step 11b: lower-EL AArch64 sync goes to the board: `svc` to `board_syscall` (`x8` number, `x0` result), any other EC to `board_user_fault`, which prints `fault: <slot>` and drops the process; lower-EL IRQ takes the timer preemption path. Two syscalls: `exit(code)` (code unused until `wait`) and `print(ptr, len)`; `kernel::syscall::decode` bounds the range to user VA (4 GiB to 512 GiB) and 4 KiB, then the board checks each page with `at s1e0r` and returns `EFAULT` (-14) for bad pointers; unknown numbers return `ENOSYS` (-38). On exit or kill TTBR0 moves to the next task before `tlbi aside1` flushes the dead ASID, so a reused slot starts clean. User programs are `global_asm!` blobs (`board/qemu-virt/src/user.s`) copied into a code page (I-cache synced) at their own address, with one stack page; SPSR EL0t, IRQs unmasked; each process takes 5 frames (3 tables, code, stack). `test=user`: process A checks that `print` rejects a kernel address and an unmapped user address, then prints `A: 0`..`A: 9` with a spin between lines; B, mapped one page higher, reads A's code address (absent in B's table; A's TLB entries carry another ASID) and is killed; boot idles until only it remains, then powers off. Passes under TCG and hvf. Deferred: exited processes' frames and kernel stack are not reclaimed (step 13), nor are frames from a spawn that fails midway; `TPIDR_EL0` is not in the context yet.
- Step 11c: `CNTKCTL_EL1.EL0VCTEN` lets user code read `CNTVCT_EL0`/`CNTFRQ_EL0`. `test=bench-syscall` runs a process that times 100000 `print(sp, 0)` calls (a real syscall doing no I/O) and prints `syscall: <ns> ns/round-trip`, timer off. hvf median 27 ns (min 25), TCG 637 ns; yield 64/65 ns, boot 174/178 us vs before step 11 (within 5%; [BENCHMARKS.md](../BENCHMARKS.md)).
