# Benchmarks

Speed is a primary goal, so performance is tested like behavior: measured, recorded, and guarded against regressions.

## Two kinds

| Kind | What | How | Use for |
| --- | --- | --- | --- |
| Host | Pure-logic crates (allocators, parsers, encodings) | `benches/*.rs` with `harness = false`, timed with `std::time::Instant`, run on macOS | Algorithmic cost of safe crates |
| Kernel | Hot paths in the running kernel (exception entry, context switch, syscall, pipe, page fault, allocation) | Boot QEMU in bench mode, time with the ARM generic timer (`CNTVCT_EL0`), print results over the UART | Real kernel paths end to end |

- Kernel numbers under QEMU's default emulator (TCG) are only meaningful as relative comparisons between commits, not as absolute speed.
- For realistic absolute numbers, run with Apple's hypervisor, which executes natively on the M-series CPU. The boot path works under `-accel hvf -cpu cortex-a72` (PSCI power-off, exceptions, MMU, fault report). `-cpu host` (and `max`) abort at startup on QEMU 9.2.1 with an M4 host (`Property 'host-arm-cpu.sme' not found`).
- Host: `cargo bench-host`. Kernel boot time: every boot prints `boot: <N> us` (kmain entry to end of init, from `CNTVCT_EL0`/`CNTFRQ_EL0`); take min and median of 11 boots.
- No benchmark framework dependency (criterion etc.): compile time is expensive on this machine. Report min and median of N runs.

## Workflow

- Kernel comparisons use hvf (`-accel hvf -cpu cortex-a72`): TCG run-to-run noise is about 10%, so TCG numbers are informational only and never gate a change.
- Compare medians of at least 21 runs, before and after interleaved, on an otherwise idle machine.
- Any change to a hot path includes before/after numbers from the relevant benchmark, run on the same machine and mode.
- Any slowdown beyond run-to-run noise (hvf median for kernel benchmarks, host median for host benchmarks) is a failing result; a justification does not excuse it. Remove it, or show with numbers that no safe faster form exists. If the before/after spread is wider than the difference, rerun before concluding.
- New hot paths (each roadmap step that adds one) get a benchmark when they land, alongside their end-to-end test.
- Per call, A/B: `scripts/bench.sh <test> <rounds> <new mog_os> [<base mog_os>]` boots each kernel `<rounds>` times
  under hvf, alternating which goes first, each boot on a fresh 1024-block MogFS image, and prints the median and min
  of every `bench <name>: <ns> ns` line, base vs new with deltas. `SLOWER` marks a call whose median and min both rose:
  rerun it with more rounds; if it holds, it is a failure under the rule above. A/A noise at 21 rounds (one kernel
  against itself, load about 25): medians within 1% for calls under 1 us and within 7% for disk-bound calls, and
  `SLOWER` showed on 3 of 27 calls, so one flag alone is not a verdict.
- Build the base kernel from the base commit (a worktree, `cargo build`, copy
  `target/aarch64-unknown-none-softfloat/debug/mog_os`), then the new one; the script takes the two files.
- `test=bench-syscalls` (`crates/user/src/bin/sysbench.rs`): one table entry per call, each its fast path. A batch
  makes the call many times in groups of up to 8 between two `CNTVCT_EL0` reads (raw ticks, converted once per batch,
  so the counter read costs under 1 ns per call), setup and undo outside the timed part; the line is the median of
  11 batches. A new syscall gets a table entry. The older single benches (`test=bench-syscall`, `bench-fs`,
  `bench-spawn`, `bench-pipe`, `bench`) stay as they are: folding them in would change their loops and their numbers.
- `test=bench-shell`: `shellsetup` makes the fixtures, then msh, given command lines as arguments, runs them 5 times,
  timing each from just before its `spawn` to after `wait` reaps the child (`bench <command>: <ns> ns`).
- Cycle and instruction counts: not available under QEMU. Probed at EL1 (PMCR_EL0 enable, PMCNTENSET_EL0, PMEVTYPER0
  INST_RETIRED 0x08, PMUSERENR_EL0): under hvf, writing PMEVTYPER0_EL0 or PMXEVTYPER_EL0 traps as undefined (EC 0);
  the cycle counter alone works but is QEMU's virtual clock (15.0 M "cycles" for 15.0 ms of CNTVCT, in steps of
  1000), not CPU cycles. Under TCG the cycle counter is the same clock and the event counter reads 0. Cycle and
  instruction counts wait for real hardware (phase 11).
- Record the current numbers in the Baselines table below whenever they change.

## Per-call baselines

`scripts/bench.sh bench-syscalls 21`, QEMU hvf (`-cpu cortex-a72`), dev build, M4 Pro, load about 30, commit
"e2e: build the kernel once per run" (ns per call; the `bench` line names in brackets).

| Call | Median | Min |
| --- | --- | --- |
| `io_submit_wait` console write, 0 bytes (`console-write`) | 36.6 | 32.5 |
| `io_submit_wait` console read, 0 bytes (`console-read`) | 37.1 | 34.3 |
| `io_submit_wait` pipe write, 64 bytes (`pipe-write`) | 59.8 | 55.2 |
| `io_submit_wait` pipe read, 64 bytes (`pipe-read`) | 62.6 | 58.3 |
| `io_submit_wait` file write, 64 bytes at offset 0 (`file-write`; a block write) | 15366 | 13691 |
| `io_submit_wait` file read, 64 bytes at offset 0 (`file-read`) | 73.6 | 69.4 |
| `dup` | 38.1 | 35.5 |
| `close` | 38.4 | 34.7 |
| `open`, existing file in the root (`open`) | 81.4 | 76.9 |
| `open(CREATE)`, new file (`open-create`) | 16018 | 11159 |
| `open(TRUNC)`, existing empty file (`open-trunc`) | 86.8 | 83.1 |
| `mkdir` | 16038 | 14007 |
| `readdir`, root of 3 entries into 512 bytes | 89.4 | 84.7 |
| `unlink` of an empty file | 8278 | 7495 |
| `rename` within the root | 16025 | 14398 |
| `sync`, nothing changed (`sync`) | 36.8 | 32.4 |
| `sync` after a 64-byte file write (`sync-change`) | 98268 | 88908 |
| `map`, one page | 552 | 522 |
| `pipe` | 99.0 | 86.2 |
| `spawn` of `nop`, no arguments (`spawn`) | 2441 | 2347 |
| `spawn` of `nop`, two arguments (`spawn-args`) | 2661 | 2531 |
| `wait` on a killed child (`wait`) | 54.0 | 48.8 |
| `kill` of a ready child that never ran (`kill`) | 1074 | 909 |
| `mutex` | 38.2 | 36.4 |
| `lock`, uncontended | 36.9 | 33.1 |
| `unlock`, no waiter | 37.9 | 35.9 |
| unknown syscall 18, `ENOSYS` (`enosys`) | 32.9 | 30.0 |

## Shell command baselines

`scripts/bench.sh bench-shell 21`, as above: 105 samples per command (21 boots of 5 rounds), each from msh's
`spawn` of the program to reaping it, output to the PL011 included (us).

| Command | Median | Min |
| --- | --- | --- |
| `ls d1` (1 entry) | 37.6 | 26.1 |
| `ls d100` (100 entries) | 914 | 843 |
| `ls d390` (390 entries; 1000 does not fit: MogFS v1 has 504 inodes, the root and fixtures included) | 3552 | 3298 |
| `cat small` (4 KiB) | 6161 | 5818 |
| `cat big` (57232 bytes, the largest MogFS v1 file) | 86283 | 82076 |
| `write w hello` | 115 | 75.7 |
| `mkdir m` | 41.4 | 29.3 |
| `rm m` | 5.6 | 5.1 |
| `mv a b` / `mv b a` | 24.2 / 22.7 | 18.8 / 16.5 |
| `echo hi` | 11.9 | 10.5 |

`ls` and `cat` are bound by the console: each byte is a PL011 write, a VM exit under hvf (about 1.5 us per byte), so
`cat big` is about 57232 of them.

## Baselines

| Benchmark | Mode | Min | Median | Commit |
| --- | --- | --- | --- | --- |
| `mm` frames: alloc+free of 1000 frames, 128 MiB allocator (ns/op) | Host, M4 Pro | 4.6 | 5.2 | uncommitted |
| `mogfs` create + 100-byte write + commit, 400 files in one directory, in-memory disk (ns/op) | Host, M4 Pro | 1873 | 1956 | phase 4 MogFS unlink and rename |
| `mogfs` lookup in a 400-entry directory, in-memory disk (ns/op) | Host, M4 Pro | 758 | 784 | phase 4 MogFS unlink and rename |
| `net` UDP over the loss-free simulated link: `send_to` on A, `poll` + `recv_from` on B, batches of 16, 64-byte / 1472-byte datagrams (ns/datagram; 17.9 / 7.5 M datagrams/s at the median) | Host, M4 Pro | 53.6 / 130.3 | 55.8 / 133.7 | phase 8 step 46 |
| `net` UDP receive path: parse, checksum, demux, copy into the socket buffer and out with `recv_from`, 64-byte / 1472-byte datagrams (ns/frame) | Host, M4 Pro | 29.7 / 68.3 | 30.8 / 70.8 | phase 8 step 46 |
| Kernel boot, kmain to end of init (us) | QEMU TCG, dev build | 2928 | 3140 | uncommitted |
| Kernel boot, kmain to end of init (us) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots | 157 | 178 | phase 3 step 11 |
| Yield round trip via `svc`, `test=bench`, 100000 trips (ns) | QEMU TCG, dev build, 11 boots | 1178 | 1218 | uncommitted |
| Yield round trip via `svc`, `test=bench`, 100000 trips (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots | 68 | 70 | phase 3 step 16 |
| Syscall round trip from EL0, `test=bench-syscall`, 100000 `print(sp, 0)` timed in user space (ns; before `print` became `write`) | QEMU TCG, dev build, 11 boots | 621 | 637 | phase 3 step 11 |
| Syscall round trip from EL0, `test=bench-syscall`, 100000 `io_submit_wait(console, write, sp, 0)` timed in user space (ns; one `KERNEL` lock round trip per trap since step 24, base 28 / 30 in the same run) | QEMU hvf (`-cpu cortex-a72`), dev build, 63 interleaved boots, load about 10 | 29 | 32 | phase 5 step 24 |
| Pipe round trip, `test=bench-pipe`: one byte to `pong` and back over two pipes, 100000 trips, timed by the kernel from spawn to exit (ns) | QEMU TCG, dev build, 11 boots | 14180 | 14556 | phase 3 step 15 |
| Pipe round trip, `test=bench-pipe` (ns; six traps, so six `KERNEL` lock round trips, since step 24; base 365 / 375 in the same run) | QEMU hvf (`-cpu cortex-a72`), dev build, 63 interleaved boots, load about 10 | 378 | 389 | phase 5 step 24 |
| Uncontended lock round trip, `test=bench-lock`: 10^7 acquire + release on one core; ticket (`lock_masked` + drop: `ldxrh`/`stxrh`, `ldarh`, `stlrh`) / test-and-set (`swap(true, Acquire)` + `store(false, Release)`) (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 42 boots | 2.8 / 6.0 | 2.9 / 6.1 | phase 5 step 24 |
| Disk throughput, `test=bench-disk`: 2048 sequential virtio-blk blocks (8 MiB raw image) written then flushed, then read, polled, one request in flight; 4 KiB per request, then 256 KiB (MiB/s, higher is better; 4 KiB write+flush / read / 256 KiB write+flush / read) | QEMU TCG (`-global virtio-mmio.ioeventfd=off`), dev build, 1 boot | - | 159 / 206 / 1020 / 4032 | phase 4 step 20 |
| Disk throughput, `test=bench-disk` (MiB/s, as above; machine busy with parallel builds) | QEMU hvf (`-cpu cortex-a72`, `-global virtio-mmio.ioeventfd=off`), dev build, 21 boots | 101 / 126 / 1389 / 3470 | 148 / 188 / 3415 / 8629 | phase 4 step 20 |
| Kernel boot with a MogFS disk mounted (us; `mog_os` before step 22 did not mount: 224 / 245) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 interleaved boots | 333 | 386 | phase 4 step 22 |
| File round trips, `test=bench-fs`: `open(CREATE \| TRUNC)` + 100-byte write + `sync` + `close` on one file in the root, 1000 trips, timed in user space (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots | 122665 | 136717 | phase 4 step 22 |
| File round trips, `test=bench-fs`: `open` (one `lookup`) + `close`, 100000 trips (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots | 109 | 111 | phase 4 step 22 |
| Syscall round trip through musl, `sh -c cbench`: 100000 `write(1, "", 0)` (the `test=bench-syscall` call through the dispatcher) timed in user space (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 11 boots, load about 25 | 36 | 37 | phase 4 step 23 |
| busybox spawn, `sh -c cbench`: `posix_spawn` + `waitpid` of busybox `true`, 100 trips, timed in user space (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 11 boots, load about 25 | 26367 | 27407 | phase 4 step 23 |
| Spawn round trip, `test=bench-spawn`: `spawn` of `nop` (one page of program) + `wait` + `close`, 1000 trips, timed in user space, without / with two arguments (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 42 boots, load about 9 | 3096 / 3254 | 3430 / 3604 | phase 4 shell review |
| Kernel boot with a MogFS disk mounted, mount in 3 requests (us; 2-block superblock read) | QEMU hvf (`-cpu cortex-a72`), dev build, 42 interleaved boots (busy machine) | 294 | 342 | phase 4 shell commands |
| `Board::disk` probe, timed in the kernel around the call (us; no disk: one device-ID read; disk: one read plus the setup) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots each | 1 / 45 | 2 / 50 | phase 4 step 20 |
