# `crates/user` - user programs and their syscall stubs

## What this crate is

EL0 programs (`src/bin/*.rs`), each the init or a child of one `test=*` scenario, and the native syscall stubs they
share (`src/lib.rs`). `crates/board/qemu-virt/build.rs` builds them and bundles every bin into the boot archive.

It is **NOT** libc or a Rust `std` target, and **NOT** where the hand-written asm programs live (`src/user.s` in the
board crate).

## Boundaries (hard)

- Outside the workspace (own `Cargo.lock`): lint and format with the `--manifest-path` commands in
  `docs/DEVELOPMENT.md`. `cargo test-host` does not build it.
- `#![no_std]`, `#![no_main]`, no dependencies. `unsafe` blocks only in `src/lib.rs` for `svc` and `map`'s slice
  (bins only use `#[unsafe(no_mangle)]`), each with a `// SAFETY:`.
- Talks to the kernel only through `svc #0`; handles arrive at values 0, 1, ... as the spawner passed them
  (init: 0 console, 1 itself, 2 boot archive).

## Invariants & rules

- The ABI is mirrored by hand from `crates/kernel/src/syscall.rs` (numbers, rights bits, error values, `KILLED`);
  change both together.
- `link.ld` and `build.rs`: static ELFs at 4 GiB, one RX and one RW `PT_LOAD`, `-zmax-page-size=4096`. Everything
  must fit in the board's `IMAGE` (below the stack page at 4 GiB + 2 MiB) or `spawn` returns `ENOEXEC`.
- Child budgets (`CHILD_BUDGET`, `A_BUDGET`, `PONG_BUDGET`, ...) are sized deliberately, some exact, some with slack,
  as their comments say; they must fit in the kernel's `BOOT_BUDGET`, `WAITER_BUDGET`, `PI_BUDGET`. `ROUND_TRIPS` in
  `ping.rs` must equal the kernel's `PIPE_ROUND_TRIPS`.
- A failed check exits instead of printing, so a wrong result shows as a missing line in the e2e test; panic is
  `exit(1)`.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Only end to end: `spawner` / `child` (`spawn_moves_handles_and_budget_to_the_child`), `reader` / `writer`
  (`parent_blocks_on_an_empty_pipe_until_the_child_writes`), `waiter` (`an_exited_child_keeps_its_slot_until_waited_for`),
  `pi` / `low` / `mid` / `high` (`priority_inheritance_lets_the_mutex_owner_outrun_a_middle_priority_spinner`),
  `ping` / `pong` (`pipe_bench_reports_round_trip`), all in `crates/e2e/tests/boot.rs`.
- Clippy and fmt via the `crates/user` commands in `docs/DEVELOPMENT.md` must be clean.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Do **not** edit this
> file without explicit user approval.
