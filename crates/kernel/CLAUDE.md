# `crates/kernel` - OS logic behind the `Board` port

## What this crate is

The hardware-free core: the boot sequence (`run`), the `Board` port each board implements, the scheduler, handle
tables, pipes, mutexes, syscall decoding, and the boot archive's cpio and ELF parsers.

It is **NOT** where registers, page tables, trap entry or MMIO live (`crates/arch`, `crates/board/*`), and it never
touches memory through raw addresses: the board reads user buffers, copies pages and frees frames.

## Responsibilities

- `Board` trait and `Program` enum (`src/lib.rs`); `run` drives boot and the `test=*` bootargs scenarios.
- `Scheduler<N>` (`src/sched.rs`): slots, states (`Ready`, `Blocked`, `Exited`, `Zombie`), priorities, `reap`, `kill`.
- `Handles` (`src/handle.rs`): per-process handle tables, rights, `dup`, `split` for `spawn`.
- `Pipes<N>` (`src/pipe.rs`), `Mutexes<N>` (`src/mutex.rs`): fixed tables of kernel objects.
- `syscall::dispatch` (`src/syscall.rs`): decodes `x8`/`x0`-`x5`, checks handles and rights, returns a `Call` for the
  board to execute. Syscall numbers and error constants are defined here.
- `cpio::find`, `elf::Elf::parse` (`src/cpio.rs`, `src/elf.rs`).

## Boundaries (hard)

- `#![no_std]` with `extern crate alloc`; workspace `unsafe_code = "forbid"` applies, no opt-out ever.
- Depends only on `mm` and `dtb`. Never on `arch` or a board crate: dependencies point inward, boards depend on it.
- Hardware reaches it only through `Board` (generic `B: Board`); AGENTS.md Architecture rules apply.
- Callers: `crates/board/qemu-virt` (implements `Board`, calls `run`, `dispatch` and the table types) and its host
  tests in `tests/`. The user ABI it decodes is mirrored by hand in `crates/user/src/lib.rs`.

## Vocabulary

- **Slot**: a scheduler index; slot 0 is the **boot context**. A process's slot is also its ASID (board side).
- **Generation**: per-slot (and per pipe/mutex entry) counter that tells a live object from a later one in the same place.
- **Zombie**: an exited process whose slot is kept because some handle table still holds a `Process` handle to it.
- **Budget**: frames a process may hold (`mm::Budget`); `spawn` moves part of the parent's to the child.
- **Boot archive**: the cpio of `crates/user` programs; `Object::Archive` / `Object::File` reach it.

## Invariants & rules

- Handle value is `generation << 32 | index`; `close` bumps the entry's generation; an entry retires at `1 << 31`, so
  values stay positive and never wrap (`src/handle.rs` header, `RETIRED`).
- `dup` needs `DUPLICATE` and only narrows rights; `split` moves only `TRANSFER` handles and works on a copy, so a
  failed `spawn` changes nothing (`Handles::split`).
- `Scheduler::add` takes the generation from `free_slot` (old + 1); `reap`, `kill`, `budget` return `EBADF`/`None`
  for a stale generation. Slot 0 never exits (`assert!` in `Scheduler::exit`).
- `end` decides `Zombie` vs `Exited` by whether any table holds a handle to the process; `reap` hands out the budget
  limit once (later calls get 0); `close` on a zombie's handle frees its slot like `reap` (`src/sched.rs`).
- `advance` runs the highest effective priority, round robin within a level, boot context when none is ready.
  Priority inheritance is one level only (`unboost` doc). The board calls `unboost(slot, ..)` when an owner loses a
  waiter (an unlock that woke one, or a kill of a task blocked on `Lock`); after such an unlock it switches at once if
  `outranked()` (any ready task beats the caller).
- Pipes and mutexes: entry reached by `index` + `generation`, counted handles, freed when the count hits zero.
  `End::index` and `Mutex::index` are `u32` so copying an `Object` stays a plain move on the syscall path.
- Pipe writes are all-or-nothing (`Pipe::write`); `MAX_BUFFER <= pipe::SIZE` is const-asserted in `src/syscall.rs`.
- `MAX_BUFFER` (4 KiB) and `MAX_MAP` (16 pages) bound the work a syscall does with IRQs masked; `user_buffer` checks
  every user range lies in `USER` (4 GiB..512 GiB).
- Errors are negated musl errno values; `KILLED` (256) sits outside `exit`'s 0..=255.
- `Elf::parse` accepts only page-aligned, address-ordered, in-region `PT_LOAD`s, never W+X, entry in an executable one.
- `BOOT_BUDGET`, `WAITER_BUDGET`, `PI_BUDGET` are sized to the user programs' frame needs: too small and `run`'s
  `expect("spawn")` panics. `PIPE_ROUND_TRIPS` must equal `ROUND_TRIPS` in `crates/user/src/bin/ping.rs`; a mismatch
  only prints a wrong `pipe:` number, nothing fails.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Host: `cargo test --target aarch64-apple-darwin -p kernel` runs `tests/sched.rs`, `tests/handle.rs`,
  `tests/pipe.rs`, `tests/exec.rs` (cpio and ELF).
- End to end: every scenario in `crates/e2e/tests/boot.rs`; `run`'s `test=*` arms are listed in
  `docs/DEVELOPMENT.md` (inner loop). Full gate: `cargo test-host`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Do **not** edit this
> file without explicit user approval.
