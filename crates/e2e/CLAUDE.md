# `crates/e2e` - QEMU boot tests

## What this crate is

Host-only integration tests: `tests/boot.rs` builds `qemu-virt`, boots `mog_os` in `qemu-system-aarch64`
(`virt`, `cortex-a72` unless the scenario passes its own `-cpu`, 128 MiB, `-global virtio-mmio.force-legacy=false`, `-global virtio-mmio.ioeventfd=off`;
`-append test=<name>` for every scenario but plain boot; one core, except `-smp 4` for `test=smp`, the second echo
scenario and `spec_line_matches_the_cpu`, which boots TCG `cortex-a72`, `cortex-a76` and `max`), and asserts on the serial
lines and the exit status. `tests/user.rs` runs `crates/user`'s host tests (outside the workspace) through a nested
`cargo test`, so `cargo test-host` covers them.
`src/lib.rs` is an empty placeholder (`[lib] test = false`). It is the main test of the project, not a library.

## Boundaries (hard)

- Runs on the host (std). It drives the kernel only through the binary and its serial output; its one workspace
  dependency, `mogfs` (dev), only formats the shell tests' images (`mogfs_image`).
- Workspace `forbid(unsafe_code)`.
- Callers: `cargo test-host`. Nothing depends on it.

## Invariants & rules

- Assertions are on exact serial lines the kernel or user programs print; when boot output changes, extend `tests/boot.rs`
  in the same change (`docs/DEVELOPMENT.md`, rules for agents).
- Each boot has a 30 s deadline, then QEMU is killed and the test fails. Every boot prints QEMU's exit status,
  stderr and stdout (captured: shown when the test fails).
- The kernel is built once per test run (`Once` in `boot_with_input`): even a fresh `cargo build` replaces `mog_os`
  (a new inode), so a build beside a booting test made QEMU fail with `Couldn't load elf` and no output.
- Disk scenarios make a zeroed raw image in the temp dir per test (`disk_image`), or a formatted MogFS one
  (`mogfs_image`), and remove it; the flush checks (`test=disk`, and msh's `sync` in `sync_reports_a_failed_flush`) boot through a `blkdebug` blockdev that fails every host flush with EIO (`flush_fails`).
- Console input (`boot_with_input`) writes chunk `i` once the output holds the ready marker `i + 1` times, so `shell`
  types one command per `msh> ` prompt and no echo interleaves with msh's output.
- `assert_no_leak` checks that a scenario's `<test>: free frames <n> before, <n> after` counts match; every scenario
  of `budget`, `spawn`, `pipe`, `wait`, `pi`, `echo`, `shell`, `bench-fs`, `bench-spawn`, `fuzz`, `bench-syscalls`, `bench-shell`, `threads`, `bench-threads`, `sockets` and `httpd` uses it; a new scenario that frees frames should too.
- Network scenarios boot with QEMU's user network and a `virtio-net-device` (`boot_with_nic`, devices after `extra`);
  the host side (a UDP echo, `udp_echo`) binds `127.0.0.1:0` so parallel tests never share a port, and the guest
  reaches it as 10.0.2.2. The httpd test forwards a free host port (bound to `127.0.0.1:0`, then released) to the
  guest's port 80 with `hostfwd` and retries its requests until the server listens.
- A new kernel behavior gets its failing scenario here first (`docs/WORKFLOW.md`, step 2).
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- All: `cargo test-host`. One scenario: `cargo test --target aarch64-apple-darwin -p e2e -- <test name filter>`. Flake
  hunting: the loop in `docs/DEVELOPMENT.md` (inner loop).

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
