# `crates/kernel` - OS logic behind the `Board` port

## What this crate is

The hardware-free core: the boot sequence (`run`), the `Board` port each board implements, the scheduler and its
process table, handle tables, pipes, mutexes, syscall decoding, and the boot archive's cpio and ELF parsers.

It is **NOT** where registers, page tables, trap entry or MMIO live (`crates/arch`, `crates/board/*`), and it never
touches memory through raw addresses: the board reads user buffers, copies pages and frees frames.

## Responsibilities

- `Board` trait and `Program` enum (`src/lib.rs`); `run` drives boot and the `test=*` bootargs scenarios; the board turns on the MMU before calling it (its locks need the MMU), and `run` starts the other cores (`Board::start_cpus`) as the last step of boot, inside the `boot:` time. The crate has no lock: its tables are plain data the board keeps under its big lock.
- `Disk` and `BLOCK_SIZE` (4096) are `mogfs`'s, re-exported (`src/lib.rs`): synchronous `read`/`write` of
  consecutive blocks, `flush`, `blocks`; every failure is `mogfs::Error::Io`. `Board::disk` is called once in `run`,
  then `Board::mount` (except under `test=disk` and `test=bench-disk`, which keep the raw device), both before the
  `boot:` line, so probe and mount count toward boot time. A failed mount prints `fs: <error>` and never formats.
- `file` (`src/file.rs`): the file syscalls' work over a `&mut Fs<D>` the board passes in: the path walk (one
  `lookup` per `/`-separated component), `open` (`CREATE`, `TRUNC`), `mkdir`, `unlink`, `rename`, `readdir` (one pass
  writing whole `name\n` / `name/\n` entries, at most 64 per call), `list_archive`, and `errno` (mogfs error to musl
  errno; `Io` and `Corrupt` are `EIO`).
- `Scheduler<N, P>` (`src/sched.rs`): thread slots (frame, process, kernel stack, state, priorities) and the process
  table `Processes<P>` (address space, `Handles`, `Memory` with the budget and map cursor, live threads (a slot bitmask),
  generation); states (`Ready`, `Blocked`, `Exited`, `Zombie`) for both; `end` (a thread, and its process with its last
  thread), `reap` (a process), `join` (a thread).
- `Handles` (`src/handle.rs`): per-process handle tables, rights, `dup`, `split` for `spawn`.
- `Pipes<N>` (`src/pipe.rs`), `Mutexes<N>` (`src/mutex.rs`): fixed tables of kernel objects. `Pipe::read` and
  `Pipe::write` hand the caller each chunk of the ring through a closure, so the board copies straight between user
  memory and the pipe page; `read_waits` lets it skip probing the user buffer when the read would wait.
- `syscall::dispatch` (`src/syscall.rs`): decodes `x8`/`x0`-`x5`, checks handles and rights, returns a `Call` for the
  board to execute. Syscall numbers and error constants are defined here.
- `cpio::find`, `cpio::entries`, `elf::Elf::parse` (`src/cpio.rs`, `src/elf.rs`).

## Boundaries (hard)

- `#![no_std]` with `extern crate alloc`; workspace `unsafe_code = "forbid"` applies, no opt-out ever.
- Depends only on `mm`, `dtb` and `mogfs`. Never on `arch` or a board crate: dependencies point inward, boards depend on it.
- Hardware reaches it only through `Board` (generic `B: Board`); AGENTS.md Architecture rules apply.
- Callers: `crates/board/qemu-virt` (implements `Board`, calls `run`, `dispatch` and the table types) and its host
  tests in `tests/`. The user ABI it decodes is mirrored by hand in `crates/user/src/lib.rs`.

## Vocabulary

- **Slot**: a scheduler index, one thread; slot 0 is the **boot context**. A thread handle (`Object::Thread`) names a
  slot and its generation.
- **Process index**: an entry of the process table, also its ASID (board side); index 0 is the kernel (boot context
  and kernel tasks, boot table, ASID 0). A process handle (`Object::Process`) names an index and its generation.
- **Generation**: per-slot, per-process (and per pipe/mutex entry) counter that tells a live object from a later one
  in the same place.
- **Zombie**: an ended thread or process whose slot or index is kept because a handle to it is still open.
- **Budget**: frames a process may hold (`mm::Budget`), its threads' kernel stacks included; `spawn` moves part of
  the parent's to the child.
- **Boot archive**: the cpio of `crates/user` programs; `Object::Archive` / `Object::File` reach it, read-only.
- **File system**: the mounted MogFS; `Object::Dir(Inode)` / `Object::Node(Inode)` (a file) reach it. Inode numbers
  stay fixed while a file lives.

## Invariants & rules

- Handle value is `generation << 32 | index`; `close` bumps the entry's generation; an entry retires at `1 << 31`, so
  values stay positive and never wrap (`src/handle.rs` header, `RETIRED`).
- `dup` needs `DUPLICATE` and only narrows rights; `split` moves only `TRANSFER` handles and works on a copy, so a
  failed `spawn` changes nothing (`Handles::split`).
- `Scheduler::add` and `add_process` take the generation from `free_slot` / `free_process` (old + 1); `reap`,
  `join`, `process_live`, `thread_live` return `EBADF` for a stale generation, `budget` `None`. Slot 0 never ends
  (`assert!` in `Scheduler::end`), nor does process 0.
- Thread slots and process indices share one lifecycle (`Entries`): state, generation, a count of open handles
  (`held` on each new `Process` or `Thread` handle, `close` / `close_thread` on each closed one, ignored for an older
  generation) and a bitmask of free entries, so `end` and `free_slot` / `free_process` are O(1) (at most 64 entries,
  const-asserted). `end` makes a thread, and with its last thread the process, a zombie while its count is above 0;
  the caller first takes and releases the process's own handles (`take_handles`), so its handle to itself does not
  keep it. `reap` hands out the budget limit once (later calls get 0); the last `close` / `close_thread` of a zombie
  frees its index or slot like `reap` / `join` (`src/sched.rs`).
- One run queue for every core (`start_cores` sizes the per-core state at boot from `Board::cpus`); every call about
  "the current task" takes the core. `advance(cpu)` runs the highest effective priority no other core runs (a bit per
  running slot), round robin within a level; slot 0 (the boot context) only on core 0; with none, core 0 the boot
  context once no core runs a task, any other case the core's idle context (process 0, its frame saved by `switch`).
  `wake` and `add` count the tasks made ready (`take_woken`) and `claim_idle` hands out an idle core to signal, once
  per idle period. A thread another core runs is never ended in place: the board `mark`s it and its core ends it.
  Priority inheritance is one level only (`unboost` doc). The board calls `unboost(slot, ..)` when an owner loses a
  waiter (an unlock that woke one, or the end of a thread blocked on `Lock`); after such an unlock it switches at once if
  `outranked()` (any ready task beats the caller).
- Pipes and mutexes: entry reached by `index` + `generation`, counted handles, freed when the count hits zero.
  `End::index` and `Mutex::index` are `u32` so copying an `Object` stays a plain move on the syscall path.
- Pipe writes are all-or-nothing (`Pipe::write`); `MAX_BUFFER <= pipe::SIZE` is const-asserted in `src/syscall.rs`.
- `MAX_BUFFER` (4 KiB) and `MAX_MAP` (16 pages) bound the work a syscall does under the board's big lock (IRQs masked); `user_buffer` checks
  every user range lies in `USER` (4 GiB..512 GiB).
- Errors are negated musl errno values; `KILLED` (256) sits outside `exit`'s 0..=255.
- Syscalls 0-19 (`src/syscall.rs` docs): exit, io_submit_wait, dup, close, map, open, spawn, pipe, wait, mutex, lock,
  unlock, kill, mkdir, readdir, sync, unlink, rename, thread, thread_exit. `exit` ends the whole process; `thread_exit`
  ends the caller, and its process with its last thread; `wait` and `kill` take a process or a thread handle (`wait`
  on a thread is a join). Both exit codes keep the low 8 bits, so `KILLED` stays distinct. `MAX_BUFFER` is public: the
  board sizes its copy-in buffer with it. `io_submit_wait` takes a file offset in x4 (files need it, the console and pipes
  ignore it; offsets live in libc, not in handles, so `Object` stays `Copy`). `open` takes flags in x3; the opened
  object gets the directory handle's rights, so a child never has more. `spawn` takes arguments in x5/x6 (NUL-ended
  strings, at most `MAX_ARGS` (32) and `MAX_BUFFER` bytes, `E2BIG`; checked by `syscall::argc`); `dispatch` reads
  x0-x6. `Call` stays 56 bytes (const-asserted), so `Spawn`'s small fields are `u8`/`u16`. Changing the archive is `EROFS`.
- An inode a handle reaches is never freed: `unlink` is `EBUSY` while any table holds a `Dir` or `Node` handle to it
  (`Scheduler::holds`, at most `MAX_PROCESSES * MAX_HANDLES` entries), since `create` reuses freed inodes. The scan sees
  every handle: `spawn` moves handles within one syscall, and an exiting process's table is emptied as it releases.
- Paths resolve only below a directory handle: each component goes through `mogfs::lookup`, which rejects `.`, `..`
  and empty names, so `../x` and `/x` are `EINVAL`. Trust note: a crafted image can point an entry at `ROOT` or an
  ancestor, so a subdirectory handle may reach the root and the tree may cycle; nothing in the kernel recurses over
  the tree. File syscalls do their disk work under the big lock with IRQs masked, so each is bounded: a path has at most
  `file::MAX_DEPTH` (16) components (`ENAMETOOLONG`), a lookup or a `readdir` scan reads at most a directory's 14
  blocks, a `readdir` call lists at most 64 entries, and file I/O moves at most `MAX_BUFFER`. Worst case: `open(CREATE)`
  on a 16-component path of full directories is about 240 block requests, about 5 ms with IRQs masked. A
  directory `rename` across directories also reads every directory below the one moved (mogfs's cycle check), up to
  the whole tree: about 500 requests, about 10.5 ms with IRQs masked, on a well-formed image (each directory read
  once), and about 7000, about 150 ms, on a crafted one (504 directories of 14 blocks each).
- `Elf::parse` accepts only page-aligned, address-ordered, in-region `PT_LOAD`s, never W+X, entry in an executable one.
- init's handles (`Handles::init`): 0 console (read, write, duplicate, transfer), 1 itself (kill), 2 the boot archive
  with `INIT_ARCHIVE` (read, exec); only msh (`test=shell`, `test=bench-shell`) gets `SHELL_ARCHIVE` (also duplicate, transfer), since it
  hands the archive to `sh`, which spawns from it. Every other init can neither copy nor pass it on.
- `BOOT_BUDGET`, `SHELL_BUDGET` (msh under `test=shell` and `test=bench-shell`: its 25 frames and the 2048 it gives `sh`), `WAITER_BUDGET`, `PI_BUDGET`, `FUZZ_BUDGET`, `SYSBENCH_BUDGET`, `THREADS_BUDGET` are sized to the user programs' frame needs: too small and `run`'s
  `expect("spawn")` panics. `PIPE_ROUND_TRIPS` must equal `ROUND_TRIPS` in `crates/user/src/bin/ping.rs`; a mismatch
  only prints a wrong `pipe:` number, nothing fails.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Host: `cargo test --target aarch64-apple-darwin -p kernel` runs `tests/sched.rs` (slot and process generations,
  zombies, last-thread exit, joins, priorities), `tests/handle.rs`,
  `tests/pipe.rs`, `tests/exec.rs` (cpio, ELF, archive listing), `tests/args.rs` (`spawn`'s argument checks), `tests/dispatch.rs` (`unlink`, `rename`, `sync` handle checks), `tests/file.rs` (path walk limits, `readdir` at
  tight buffer sizes, over an in-memory disk); `file` also end to end (`test=shell`, `test=bench-fs`).
- End to end: every scenario in `crates/e2e/tests/boot.rs`; `run`'s `test=*` arms are listed in
  `docs/DEVELOPMENT.md` (inner loop). Full gate: `cargo test-host`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
