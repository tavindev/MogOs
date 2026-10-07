# `crates/user` - user programs and their syscall stubs

## What this crate is

EL0 programs (`src/bin/*.rs`), each the init or a child of one `test=*` scenario (msh's commands are its children), and the native syscall stubs they
share (`src/lib.rs`). `crates/board/qemu-virt/build.rs` builds them and bundles every bin into the boot archive.

It is **NOT** libc or a Rust `std` target, and **NOT** where the hand-written asm programs live (`src/user.s` in the
board crate).

## Boundaries (hard)

- Outside the workspace (own `Cargo.lock`): lint and format with the `--manifest-path` commands in
  `docs/DEVELOPMENT.md`. `cargo test-host` builds it only for its lib's host test (the command
  table), through `crates/e2e/tests/user.rs`, which runs the `cargo test --manifest-path` command there.
- `#![no_std]` (the lib `cfg_attr(not(test))`), `#![no_main]`, no dependencies. `unsafe` only in `src/lib.rs` for
  `svc`, the `mrs` of `now_ns` and `tls`, `map`'s slice and `start`'s argument slice, and in bins for `#[unsafe(no_mangle)]`, the one call to the
  `unsafe fn start`, and `fuzz`'s and `sysbench`'s calls to `unsafe fn raw` (any syscall, all seven arguments; the caller keeps what the
  kernel may write unreferenced); each block with a `// SAFETY:`.
- A program that takes arguments defines `_start(argc, _, len)` and calls `unsafe { start(argc, len, main) }` with
  its x0 and x2, which hands `main` the arguments as `&[&[u8]]` (the kernel puts them at the end of the top stack page,
  `STACK_TOP`) and exits with its result; boot-spawned programs get none, except `fuzz` and msh under
  `test=bench-shell`, which get the kernel's.
- Talks to the kernel only through `svc #0`; handles arrive at values 0, 1, ... as the spawner passed them
  (init: 0 console, 1 itself, 2 boot archive, 3 the MogFS root directory when a disk is mounted).
- `read`/`write` are `read_at`/`write_at` at offset 0: `io_submit_wait` takes the file offset in x4, which the console
  and pipes ignore (so the asm programs in `user.s` leave x4 as it is). `open` takes flags (`CREATE`, `TRUNC`), not
  rights: the opened file gets the directory's.

## Invariants & rules

- The ABI is mirrored by hand from `crates/kernel/src/syscall.rs` (numbers, rights bits, error values, `KILLED`);
  change both together.
- `link.ld` and `build.rs`: static ELFs at 4 GiB, one RX and one RW `PT_LOAD`, `-zmax-page-size=4096`. Everything
  must fit in the board's `IMAGE` (below a guard page and the top two stack pages, which end at 4 GiB + 2 MiB) or `spawn` returns `ENOEXEC`.
- Threads: `thread(entry, stack, tls, arg)` starts `entry(arg)` on a stack the caller mapped, with TPIDR_EL0 = `tls`
  (`tls()` reads it back); `wait` on its handle joins it, `thread_exit` ends one thread, `exit` the whole process. Its
  kernel stack (4 frames) comes out of the process's budget.
- Child budgets (`CHILD_BUDGET`, `A_BUDGET`, `PONG_BUDGET`, `VICTIM_BUDGET`, ...) are sized deliberately, some exact, some with slack,
  as their comments say; they must fit in the kernel's `BOOT_BUDGET`, `WAITER_BUDGET`, `PI_BUDGET`, `THREADS_BUDGET`. `ROUND_TRIPS` in
  `ping.rs` must equal the kernel's `PIPE_ROUND_TRIPS`.
- Least privilege (security rule): msh runs only the programs in `COMMANDS` (`src/lib.rs`; anything else, even in
  the archive, is `command not found`), resolves every path argument itself, against its root handle and current
  directory, and passes a program only the console (write, never read) and the handles its `Grant` names, narrowed
  by `dup` to those rights plus transfer: `cat` a read-only file, `ls` a read-only directory, `mkdir` and `rm` the
  parent directory with write, `touch` and `write` the parent with read and write, `mv` both parents with write,
  `sync` the root with no right, `echo` nothing. The one exception is `sh` (busybox, `Grant::Posix`, the C layout of
  `c/CLAUDE.md`): a shell reads commands, changes files anywhere and runs programs, so it gets the console as stdin (read),
  stdout and stderr (write), the root with read and write, and the boot archive with read and exec, each
  also with duplicate and transfer to hand on, plus 2048 frames (`POSIX_BUDGET`) and a first argument
  `<argc> /<cwd>`. msh's words are split on spaces, except a word in single quotes. The leaf name goes as an argument. A program never gets the root
  unless its job needs it (`sync`), nor a right it does not use; the lib's host test pins the table. A shell program
  exits with the errno of its failure (`status`), which msh prints by name.
- A failed check exits instead of printing, so a wrong result shows as a missing line in the e2e test; panic is
  `exit(255)`, outside the errno codes shell programs exit with.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Only end to end: `spawner` / `child` (`spawn_moves_handles_and_budget_to_the_child`), `reader` / `writer`
  (`parent_blocks_on_an_empty_pipe_until_the_child_writes`), `waiter` (`an_exited_child_keeps_its_slot_until_waited_for`),
  `pi` / `low` / `mid` / `high` (`priority_inheritance_lets_the_mutex_owner_outrun_a_middle_priority_spinner`),
  `ping` / `pong` (`pipe_bench_reports_round_trip`), `readlines` (`console_reads_edited_lines_typed_ahead`), `msh`
  and its programs `ls`, `mkdir`, `touch`, `write`, `cat`, `rm`, `mv`, `echo`, `sync`
  (`shell_files_survive_a_reboot_only_once_synced`, `sync_reports_a_failed_flush`), and `sh` (the musl tests in `c/CLAUDE.md`),
  `fsbench` (`fs_bench_reports_round_trips`), `spawnbench` / `nop` (`spawn_bench_reports_round_trip`), `fuzz` / `nop`
  (`fuzzer_never_crashes_the_kernel_or_leaks_frames`), `sysbench` / `nop` (`syscall_benches_report_every_call`),
  `shellsetup` / `msh` with arguments (`shell_bench_times_each_command_from_spawn_to_reap`), `threads` / `victim`
  (`threads_share_a_counter_keep_their_tls_and_end_with_their_process`), `threadbench`
  (`thread_bench_reports_round_trips`), all in `crates/e2e/tests/boot.rs`.
- Clippy and fmt via the `crates/user` commands in `docs/DEVELOPMENT.md` must be clean.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
