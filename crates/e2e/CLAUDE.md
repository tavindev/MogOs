# `crates/e2e` - QEMU boot tests

## What this crate is

Host-only integration tests: `tests/boot.rs` builds `qemu-virt`, boots `mog_os` in `qemu-system-aarch64`
(`virt`, `cortex-a72`, 128 MiB; `-append test=<name>` for every scenario but plain boot), and asserts on the serial
lines and the exit status.
`src/lib.rs` is an empty placeholder (`[lib] test = false`). It is the main test of the project, not a library.

## Boundaries (hard)

- Runs on the host (std); no dependencies on other workspace crates. It drives the kernel only through the binary
  and its serial output.
- Workspace `forbid(unsafe_code)`.
- Callers: `cargo test-host`. Nothing depends on it.

## Invariants & rules

- Assertions are on exact serial lines the kernel or user programs print; when boot output changes, extend `tests/boot.rs`
  in the same change (`docs/DEVELOPMENT.md`, rules for agents).
- Each boot has a 30 s deadline, then QEMU is killed and the test fails.
- `assert_no_leak` checks that a scenario's `<test>: free frames <n> before, <n> after` counts match; every scenario
  of `budget`, `spawn`, `pipe`, `wait` and `pi` uses it; a new scenario that frees frames should too.
- A new kernel behavior gets its failing scenario here first (`docs/WORKFLOW.md`, step 2).
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- All: `cargo test-host`. One scenario: `cargo test --target aarch64-apple-darwin -p e2e -- <test name filter>`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Do **not** edit this
> file without explicit user approval.
