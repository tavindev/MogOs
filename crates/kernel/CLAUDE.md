# `crates/kernel` - OS logic behind the `Board` port

## What this crate is

The hardware-free core: the boot sequence (`run`), the `Board` port each board implements, the scheduler and its
process table, handle tables, pipes, mutexes, syscall decoding, and the boot archive's cpio and ELF parsers.

It is **NOT** where registers, page tables, trap entry or MMIO live (`crates/arch`, `crates/board/*`), and it never
touches memory through raw addresses: the board reads user buffers, copies pages and frees frames.

## Responsibilities

- `Board` trait, `Clamp` port, `Violation` (`Board::violate`, `test=wx-*`) and `Program` enum (`src/lib.rs`); `run` drives boot and the `test=*` bootargs scenarios; the board turns on the MMU before calling it (its locks need the MMU), and `run` starts the other cores (`Board::start_cpus`) as the last step of boot, inside the `boot:` time; right after the `boot:` line it calls `Board::report_speculation`, which chooses core 0's vector table (no EL0 code may run on core 0 before it; until then an exception from EL0 panics) and prints the `spec:` line once every core has chosen, outside the boot time. The crate has no lock: its tables are plain data the board keeps under its locks, and a process's handle table is
safe atomics (`Table`, a seqlock per entry) the board reads without one.
- `Disk` and `BLOCK_SIZE` (4096) are `mogfs`'s, re-exported (`src/lib.rs`): synchronous `read`/`write` of
  consecutive blocks, `flush`, `blocks`; every failure is `mogfs::Error::Io`. `Board::disk` is called once in `run`,
  then `Board::mount` (except under `test=disk` and `test=bench-disk`, which keep the raw device), both before the
  `boot:` line, so probe and mount count toward boot time. A failed mount prints `fs: <error>` and never formats.
- `file` (`src/file.rs`): the file syscalls' work over a `&mut Fs<D>` the board passes in: the path walk (one
  `lookup` per `/`-separated component), `open` (`CREATE`, `TRUNC`), `mkdir`, `unlink`, `rename`, `readdir` (one pass
  writing whole `name\n` / `name/\n` entries, at most 64 per call), `list_archive`, and `errno` (mogfs error to musl
  errno; `Io` and `Corrupt` are `EIO`).
- `Scheduler<N, P>` (`src/sched.rs`): thread slots (frame, process, kernel stack, state, priorities) and the process
  table `Processes<P>` (address space, live threads (a slot bitmask), generation, and how many processes are
  releasing); states (`Ready`, `Blocked`, `Exited`, `Zombie`) for both; `end` (a thread; its process's last leaves the
  process to be released and then `exited`), `reap` (a process), `join` (a thread), `to_idle` (a core that releases a
  process before it picks a task). `Process` is what a process's own lock guards in the board (its map cursor), and
  the proof a sequenced handle-table write holds it.
- `Table` and `Handles` (`src/handle.rs`): a process's live handle table, its lookups (`entry`, `get`) lock-free (each
  entry's words behind a 64-bit sequence; the entries read go in a `Seen`, which `unchanged` rechecks), its writes
  (`insert`, `reserve` + `fill`, `close`, `commit`, `take`) by a `Writer`, a sealed typestate: `Process` (the lock
  held, each store sequenced) or `Alone` (made from an `OnlyThread`, a thread count read with Acquire at most 1, so
  its stores skip the sequence and its barriers). The types stop accidents (a stale flag, an `Alone` without a count
  read); that no lookup races an `Alone` store is the board's `unsafe` `ProcessEntry::unshared` contract;
  typed lookups (`mutex`, `io`) decode only the objects their call takes; `Handles` is a plain copy (`snapshot`) for
  `split` in `spawn` and building a child's. Rights; lookups take a `Handle`, a user value with its index clamped (by
  `dispatch`, or `Handle::new` / `split` with their own barrier).
- `Pipes<N>` (`src/pipe.rs`), `Mutexes<N>` (`src/mutex.rs`): fixed tables of kernel objects. `Pipe::read` and
  `Pipe::write` hand the caller each chunk of the ring through a closure, so the board copies straight between user
  memory and the pipe page; `read_waits` lets it skip probing the user buffer when the read would wait.
- `syscall::dispatch` (`src/syscall.rs`): returns `ENOSYS` above the last syscall, jumps on the masked number, and each call clamps the user values it indexes kernel memory with behind one `Clamp` barrier; decodes `x8`/`x0`-`x5`, checks handles and rights against the caller's
  `Table` without a lock (the entries read go in a `Seen`), returns a `Call` for the board to execute; `dup` and
  `close` are calls the board runs under the process lock. Syscall numbers and error constants are defined here.
- `cpio::find`, `cpio::entries`, `elf::Elf::parse` (`src/cpio.rs`, `src/elf.rs`).
- `network` (`src/network.rs`, design notes at its top): `Network`, up to three `net::Stack`s (`ETH` on the NIC,
  the loopback pair `LO` 127.0.0.1 and `PEER` 127.0.0.2 over a `Wire`, since a stack never sends to itself) and the
  socket table; `config` parses `net=<ip>/<prefix>[,gw=<ip>]`; `test=net` / `test=bench-net`. With that bootarg, or
  for `test=sockets` / `test=bench-sockets`, `run` calls `Board::start_net` (the address, and the DTB's `rng-seed`
  from the same `Dtb::chosen` walk as the bootargs as the TCP key) before `start_cpus`; the board's net task probes
  the NIC (`net: no nic` without one) and builds the `Network` off the boot path, and `run` waits for it
  (`with_net`) after the `boot:` line and prints `net: ready <N> us`, before any scenario counts frames. `test=httpd` runs `httpd` with the `httpd=` and `fetch=`
  bootargs as arguments, and alone implies `net=10.0.2.15/24,gw=10.0.2.2` (QEMU's user network). A socket is an entry reached by index and generation,
  counted by handles like a pipe. Every process holding a handle to it pays its `cost` (`SOCKET_FRAMES`, 8, plus 8 per
  backlog place of a listener; `Budget::charge`, accounting, as the memory is the fixed pool), tracked as a bitmask of
  process indices (`holders`, below `MAX_HOLDERS`, 64): `socket` and an accept charge the caller, a `spawn` charges
  the child before it starts (`ENOBUFS`, nothing moved) and refunds the parent if it kept no handle, a process's last
  handle closing (or its exit) refunds it once, and `listen` charges every holder for the backlog (1..=`BACKLOG`)
  up front, so connections peers queue are prepaid and no socket is ever charged to nobody. One past the backlog is
  reset. Open: a closed connection keeps its TCP slot, uncharged, until its FIN exchange ends (`crates/net` bounds it).
  The NIC's receive stops at a ring's worth of frames per poll (`VirtioNet::capped`), so a flood never holds `KERNEL`
  without end.
  Ops run in the submitter's context (its buffers are mapped only there): tried at submit (not an accept) and by every
  `complete` (`io_wait`); the board's net task only polls and wakes `Event::NetIo`. One receive-side op (receive,
  accept, connect) and one send per socket: the ops a process has in flight are bounded by its sockets, so by its
  budget (the step's "bounded queue charged to the budget"). A dead submitter's op slot is taken over (`alive`).

## Boundaries (hard)

- `#![no_std]` with `extern crate alloc`; workspace `unsafe_code = "forbid"` applies, no opt-out ever.
- Depends only on `mm`, `dtb`, `mogfs` and `net` (the stack, from phase 8 step 49, as `crates/net`'s contract planned). Never on `arch` or a board crate: dependencies point inward, boards depend on it.
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

- Spectre v1: `dispatch` indexes its jump table with the number masked to the table's 32 entries (`Clamp::mask`, which
  the compiler cannot drop); then each call clamps only the arguments it indexes kernel memory with, each to the
  capacity of what it indexes, together behind one barrier (`Clamp::clamp`: `csel`s then one `csdb` on the board, `min`
  on the host), before their first use, and passes on only the clamped values: a handle (x0, x3 for `rename`), a user
  buffer and its length, the file offset, the `readdir` start. A call that indexes nothing pays no barrier. An index
  derived from them is bounded by construction (MogFS's `% PTRS`). A value that only appears later keeps its own clamp:
  `spawn`'s handle list (`split`), `Handle::new`. Objects a handle reaches are kernel-written. A new syscall's indexing
  arguments get the same treatment (`docs/phases/phase-10-hardening.md`, 60b).

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
  const-asserted). `end` makes a thread a zombie while its count is above 0; its process's last thread leaves the
  process releasing: neither live (`process_live` is false, so a `kill` finds nothing to end) nor reapable, and counted
  as a task, until the board has released its handles and memory and calls `exited`, which ends it alike (its handle to
  itself was among those released). `reap` and the last `close` of a zombie free its index (the board takes the budget
  with it, `Budget::take`, so only the first reaper gets the limit); `close_thread` and `join` free a slot
  (`src/sched.rs`).
- One run queue for every core (`start_cores` sizes the per-core state at boot from `Board::cpus`); every call about
  "the current task" takes the core. `switch(cpu, frame)` runs the highest effective priority `Ready` slot that is no core's current one
  (`on_core`: a kernel task blocks and yields in two holds of the lock, so a wake can make it `Ready` while it still
  runs), round robin within a level; slot 0 (the boot context) only on core 0 (other
  cores wrap past it); with none, core 0 the boot context once no other core runs a task, any other case the core's
  idle context (process 0, its frame saved by `switch`). It returns the frame and the processes left and entered.
  `wake` and `add` count the tasks made ready; `take_woken`, at the end of a hook, turns that into the cores to signal:
  at most the tasks still ready and run nowhere (a waker that blocked took one itself), nothing while no core idles
  unsignalled (`sleepers`, so one core pays a single test), plus core 0 when every core went idle under a waiting boot context;
  `claim_idle` hands out an idle core to signal, once per idle period. A thread another core runs is never ended in place: the board `mark`s it and its core ends it.
  Priority inheritance is one level only (`unboost` doc). The board calls `unboost(slot, ..)` when an owner loses a
  waiter (an unlock that woke one, or the end of a thread blocked on `Lock`); after such an unlock it switches at once if
  `outranked()` (any ready task beats the caller).
- Pipes and mutexes: entry reached by `index` + `generation`, counted handles, freed when the count hits zero.
  `End::index` and `Mutex::index` are `u32` so copying an `Object` stays a plain move on the syscall path.
- Pipe writes are all-or-nothing (`Pipe::write`); `MAX_BUFFER <= pipe::SIZE` is const-asserted in `src/syscall.rs`.
- `MAX_BUFFER` (4 KiB) and `MAX_MAP` (16 pages) bound the work a syscall does under a board lock (IRQs masked); `user_buffer` checks
  every user range lies in `USER` (4 GiB..512 GiB).
- Errors are negated musl errno values; `KILLED` (256) sits outside `exit`'s 0..=255.
- Syscalls 20-25 (phase 8 step 50): socket, bind, listen, io_submit, io_wait (result in x0, tag in x1, an accept's
  peer in x2 as `ip << 16 | port`; a loopback peer, `PEER`'s 127.0.0.2, reads 127.0.0.1), shutdown. A
  `NetStack` handle (`CONNECT`, `LISTEN`) makes sockets, which remember which of the two it held; socket handles carry
  read and write; `bind` and `listen` need write, `io_submit` read (receive, accept) or write (send, connect), and an
  accepted connection's handle gets no right the accepting handle lacks.
- Syscalls 0-19 (`src/syscall.rs` docs): exit, io_submit_wait, dup, close, map, open, spawn, pipe, wait, mutex, lock,
  unlock, kill, mkdir, readdir, sync, unlink, rename, thread, thread_exit. `exit` ends the whole process; `thread_exit`
  ends the caller, and its process with its last thread; `wait` and `kill` take a process or a thread handle (`wait`
  on a thread is a join). Both exit codes keep the low 8 bits, so `KILLED` stays distinct. `MAX_BUFFER` is public: the
  board sizes its copy-in buffer with it. `io_submit_wait` takes a file offset in x4 (files need it, the console and pipes
  ignore it; offsets live in libc, not in handles, so `Object` stays `Copy`). `open` takes flags in x3; the opened
  object gets the directory handle's rights, so a child never has more. `spawn` takes arguments in x5/x6 (NUL-ended
  strings, at most `MAX_ARGS` (32) and `MAX_BUFFER` bytes, `E2BIG`; checked by `syscall::argc`); `dispatch` reads
  x0-x6. `Call` stays 56 bytes (const-asserted), so `Spawn`'s small fields are `u8`/`u16`. Changing the archive is `EROFS`.
- An inode a handle reaches is never freed: `unlink` is `EBUSY` while any handle reaches a `Dir` or `Node` (`file::Opens`,
  a count per inode the board keeps as such handles open and close, at most one entry per open handle), since `create`
  reuses freed inodes. `spawn` moves handles without changing a count; a process's release closes each of its own.
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
  with `INIT_ARCHIVE` (read, exec); only msh (`test=shell`, `test=bench-shell`) and `nettest` (`test=sockets`, which hands it to a C program that spawns) get `SHELL_ARCHIVE` (also duplicate, transfer), since it
  hands the archive to `sh`, which spawns from it. Every other init can neither copy nor pass it on.
- `BOOT_BUDGET`, `SHELL_BUDGET` (msh under `test=shell` and `test=bench-shell`: its 25 frames and the 2048 it gives `sh`), `WAITER_BUDGET`, `PI_BUDGET`, `FUZZ_BUDGET`, `SYSBENCH_BUDGET`, `THREADS_BUDGET` are sized to the user programs' frame needs: too small and `run`'s
  `expect("spawn")` panics. `PIPE_ROUND_TRIPS` must equal `ROUND_TRIPS` in `crates/user/src/bin/ping.rs`; a mismatch
  only prints a wrong `pipe:` number, nothing fails.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Host: `tests/network.rs` drives `Network` over loopback (data both ways, stale socket generations, budget charge,
  rights, ports, op slots).
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
