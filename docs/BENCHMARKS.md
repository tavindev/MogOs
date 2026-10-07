# Benchmarks

Speed is a primary goal, so performance is tested like behavior: measured, recorded, and guarded against regressions.

## Two kinds

| Kind | What | How | Use for |
| --- | --- | --- | --- |
| Host | Pure-logic crates (allocators, parsers, encodings) | `benches/*.rs` (`harness = false`) on criterion, timed in the benchmark thread's CPU time, run on macOS | Algorithmic cost of safe crates |
| Kernel | Hot paths in the running kernel (exception entry, context switch, syscall, pipe, page fault, allocation) | Boot QEMU in bench mode, time with the ARM generic timer (`CNTVCT_EL0`), print results over the UART | Real kernel paths end to end |

- Kernel numbers under QEMU's default emulator (TCG) are only meaningful as relative comparisons between commits, not as absolute speed.
- For realistic absolute numbers, run with Apple's hypervisor, which executes natively on the M-series CPU. The boot path works under `-accel hvf -cpu cortex-a72` (PSCI power-off, exceptions, MMU, fault report). `-cpu host` (and `max`) abort at startup on QEMU 9.2.1 with an M4 host (`Property 'host-arm-cpu.sme' not found`).
- Host: `cargo bench-host`. Kernel boot time: every boot prints `boot: <N> us` (kmain entry to end of init, from `CNTVCT_EL0`/`CNTFRQ_EL0`); take min and median of 11 boots.
- Host benchmarks use criterion (dev-dependency only) with `benches/thread_time.rs`, which each bench includes by
  `#[path]`: the thread's CPU time, not wall time (on this loaded host a wall-clock sample counted the time other
  processes ran, 3-4x the work), and flat sampling. Rows are `<group>/<name>`; one iteration is the whole workload
  (1000 frames, 400 files), so divide criterion's time by that count for ns/op. Set up outside the timed part with
  `iter_batched_ref` (`PerIteration` when the input is large), or plain `iter` when the workload leaves its state as
  it found it; when the set-up state borrows its memory (`net`'s stacks, `mogfs2`'s `Fs`), `iter_custom` sums the
  timed part of each run read with `cpu_time::ThreadTime`. Keep results alive with `std::hint::black_box`; add
  `Throughput::Bytes` to a row that reports MiB/s. `net`'s simulated goodput is virtual time, deterministic per seed,
  so its bench prints it before criterion runs. Host rows recorded before the move to criterion are wall-clock min
  and median of 5-51 runs; later ones record criterion's estimate.
- In-guest benchmarks (`test=bench-*`, `scripts/bench.sh <test>`, `scripts/oscompare.sh`) stay on the kernel's timer
  (`CNTVCT_EL0`): no framework runs in `no_std` under QEMU.
- Exact instruction counts: under TCG with `-icount shift=0,sleep=off` the virtual counter advances 1 ns per instruction, so a kernel benchmark's `ns/round-trip` reads as instructions per round trip, the same on every run. It finds where a few ns come from; it never gates (hvf does), since a probe or TTBR0 write costs far more under hvf than its one instruction.

## Workflow

- Kernel comparisons use hvf (`-accel hvf -cpu cortex-a72`): TCG run-to-run noise is about 10%, so TCG numbers are informational only and never gate a change.
- Kernel: compare medians of at least 21 runs, before and after interleaved, on an otherwise idle machine. Host: 11
  rounds of `scripts/bench.sh host`.
- Any change to a hot path includes before/after numbers from the relevant benchmark, run on the same machine and mode.
- Any slowdown beyond run-to-run noise (hvf median for kernel benchmarks, host median for host benchmarks) is a failing result; a justification does not excuse it. Remove it, or show with numbers that no safe faster form exists. If the before/after spread is wider than the difference, rerun before concluding.
- Host A/B: `scripts/bench.sh host <rounds> <base commit> <package> [<criterion args>]` checks the base out in a
  temporary worktree, runs both trees' benches each round, the order alternating, and prints criterion's change estimate and
  confidence interval per row, then each row's median, min and max change over the rounds. One round's interval
  covers only that run's noise, not the drift between runs, so it is not a verdict: a row is slower when its median
  change over 11 or more rounds lies above the A/A spread measured the same way (the host noise floor in Baselines). The base must
  already have criterion benches.
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
| unknown syscall 64 (18 before phase 8 step 50), `ENOSYS` (`enosys`) | 32.9 | 30.0 |

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
| `mm` `frames/alloc+free`: alloc+free of 1000 frames, 128 MiB allocator (ns/op) | Host, M4 Pro | 4.6 | 5.2 | uncommitted |
| `mm` `frames/contiguous(4)+free, empty` / `, 725 reserved` / `, fragmented`: `alloc_contiguous(4)` + free, 128 MiB allocator: empty / behind 725 reserved frames (boot's prefix) / behind 4096 frames with every fourth used (ns/op; bit-by-bit base 7.5 / 287 / 1464 min, 8.7 / 290 / 1513 median) | Host, M4 Pro | 7.4 / 12.3 / 46.5 | 7.6 / 12.6 / 47.1 | `mm` word-wise `alloc_contiguous` |
| `mogfs` `mogfs/create+write+commit`: create + 100-byte write + commit, 400 files in one directory, in-memory disk (ns/op) | Host, M4 Pro | 1873 | 1956 | phase 4 MogFS unlink and rename |
| `mogfs` `mogfs/lookup`: lookup in a 400-entry directory, in-memory disk (ns/op) | Host, M4 Pro | 758 | 784 | phase 4 MogFS unlink and rename |
| Host noise floor, `scripts/bench.sh host 11 HEAD <crate>` against itself (A/A), criterion on thread CPU time: median change over 11 rounds per row, `mm` four rows (two sessions) / `mogfs` create, lookup / `net` 16 rows in bench order (load 8-195) / `mogfs2` 10 rows in bench order (load 6-155) (%; single rounds spread -27% to +21%, `net` -41% to +37%, `mogfs2` -27% to +38%, often flagged significant by criterion, so a verdict takes the median). A host row is slower when its median change exceeds about 0.5% (`mm`), 3% (`mogfs`, `net`) or 1% (`mogfs2`); rerun one within twice that | Host, M4 Pro, load 6-195 | - | +0.5 / -0.4 / +0.0 / +0.0 and +0.2 / -0.4 / -0.5 / +0.1; +2.6 / +0.8; +1.4 / -2.8 / -0.5 / +0.9 / +0.7 / +1.6 / -0.4 / -0.0 / -3.0 / +0.3 / +1.5 / +0.6 / -0.8 / -2.1 / -0.2 / -0.1; +0.7 / +0.5 / +0.5 / +0.1 / -0.0 / +0.0 / -0.3 / +0.5 / +0.1 / -0.6 | criterion host benches |
| `mm` frames rows / `mogfs` create, lookup on criterion (thread CPU time, flat sampling), estimate converted to ns/op; the old harness run just before and after gave medians 5.2-5.7 / 8.5 / 12.7 / 47.0-48.5; 1959-1966 / 785 (lookup now repeats on one settled directory, not once right after the creates) | Host, M4 Pro, load 6-11 | - | 5.46 / 7.59 / 12.51 / 48.2; 1991 / 839 | criterion host benches |
| `net` on criterion (thread CPU time, flat sampling), estimate converted to ns/op: `udp/simulated link` 64 / 1472 bytes, `udp/receive path` 64 / 1472 (ns/datagram); `tcp/receive path`, `, 63 idle connections in earlier slots` (ns/segment, 2880 per iteration); `tcp/connect+accept+close`, `through a SYN cookie` (ns/connection); `tcp/idle poll` 4 / 64 / 4096 entries (ns/poll); `tcp/SYN answered` 64 / 4096 entries, with a cookie (ns/SYN; the 64-entry and cookie rows read the thread clock around each of 64 batches, about 100 ns a read, so about +1.6 ns/SYN against the old `Instant` reads); `tcp/goodput` 64 KiB / 1 MiB window (MiB/s). The old harness run just before and after gave medians 60.4-64.0 / 135.4-150.4, 30.7-34.0 / 75.4-79.6; 84.6-91.9, 95.1-104.9; 422.5-471.3, 429.4-477.3; 7.4-8.4 each; 56.6-58.7 / 45.0-51.5 / 32.2-37.0; 5507-6244 / 4898-5361 (the second run slower at lower load: criterion's sustained run reads like the later one, or above it) | Host, M4 Pro, load 13-20 | - | 70.0 / 154.1, 36.1 / 81.1; 86.2, 99.6; 516, 548; 8.53 / 8.72 / 9.07; 61.6 / 54.0 / 38.8; 5284 / 4835 | criterion host benches |
| `mogfs2` on criterion, as above: `create+write+commit`, 1024 / 16384 blocks; `lookup (400 entries)`, 1024 / 16384 blocks; `lookup (100k entries, 64-slot cache)` (ns/op); `mount, 1 GiB file` (ns); `1 GiB sequential write + commit` / `read`, `64 MiB sequential read` fresh / after the overwrites (MiB/s). Lookups and reads now repeat on one settled file system, not once right after the writes, and the fragmented file is one seed's layout, not the median of 11. The old harness just before and after: 2113-2176 / 2208-2243; 138.6-142.5 / 139.5-142.3; 1570-1624; 17958-18666; 6353-6673 / 9757-10192, 10729-11060 / 4191-4303 | Host, M4 Pro, load 12-13 | - | 2108 / 2339; 129.2 / 127.0; 1494; 16207; 6281 / 10548, 11510 / 4506 | criterion host benches |
| `net` UDP over the loss-free simulated link: `send_to` on A, `poll` + `recv_from` on B, batches of 16, 64-byte / 1472-byte datagrams (ns/datagram; 17.9 / 7.5 M datagrams/s at the median) | Host, M4 Pro | 53.6 / 130.3 | 55.8 / 133.7 | phase 8 step 46 |
| `net` UDP receive path: parse, checksum, demux, copy into the socket buffer and out with `recv_from`, 64-byte / 1472-byte datagrams (ns/frame) | Host, M4 Pro | 29.7 / 68.3 | 30.8 / 70.8 | phase 8 step 46 |
| `net` UDP receive path as above, 1472-byte datagrams, after step 47's second review (ns/frame; a code-placement artifact, not added work: the UDP path did not change, 6 interleaved sessions read 66.3-67.8 against 64.3-65.2 ns for 9c4ca51, and both built with `-C llvm-args=-align-loops=64` read 65.5-67.7 against 65.2-66.1; a workspace-wide alignment flag is queued as its own experiment) | Host, M4 Pro | 66.3 | 68.6 | phase 8 step 47 second review |
| `net` TCP goodput over the loss-free simulated link (no delay), A sends 64 MiB to B, which reads as it goes; 64 KiB / 1 MiB windows (MiB/s, higher is better; load 11-12, best minimum and median of 6 interleaved sessions' medians) | Host, M4 Pro | 6366 / 5621 | 6448 / 5683 | phase 8 step 47 review |
| `net` TCP receive path per data segment (nearly all 1460 bytes): checksum, demux, sequence checks, copy into the ring, the ACK built, copy out with `recv`; recorded segments replayed into the same connection (ns/segment; load 11-12, 6 sessions) | Host, M4 Pro | 76.7 | 80.0 | phase 8 step 47 review |
| `net` TCP receive path per data segment as above, with 63 idle connections in the slots before it (the last-matched-slot hint; the rest is `poll` walking 65 slots per batch of 16) (ns/segment; load 11-12, 6 sessions) | Host, M4 Pro | 87.4 | 92.1 | phase 8 step 47 review |
| `net` TCP connect + accept + close from each side, through TIME_WAIT, over the loss-free link, 10000 sequential connections (ns/connection; load 19-25, 4 interleaved sessions) | Host, M4 Pro | 398.5 | 407.7 | phase 8 step 47 second review |
| `net` TCP connect + accept + close as above, every connection through a SYN cookie (no half-open table) (ns/connection; load 19-25, 4 sessions) | Host, M4 Pro | 407.5 | 420.0 | phase 8 step 47 second review |
| `net` TCP connect + accept + close as above, after step 47's verification fixes and newtypes (ns/connection; load about 10, 6 interleaved sessions; 10f4b2f read 405.1-412.7 in the same sessions: a code-placement artifact, not added work: the handshake takes the same 5 polls and 7 frames per connection, the default build retires 17 more instructions but 38 more cycles per connection, and built with `-C llvm-args=-align-loops=64` the new code retires fewer instructions (2.514 G against 2.534 G for 210k connections) in the same or fewer cycles (360.0-362.2 M against 362.3-364.5 M)) | Host, M4 Pro | 413.3 | 418.0 | phase 8 step 47 verification |
| `net` `poll` with nothing to do: a listener, 2 slots, a 4 / 64 / 4096-entry half-open table (ns/poll; the table walk ran every poll before: 8.3 / 24.6 / 1196 ns) | Host, M4 Pro | 6.8 / 6.8 / 6.9 | 7.0 / 7.1 / 7.0 | phase 8 step 47 second review |
| `net` TCP SYN answered with a SYN-ACK while filling a fresh half-open table of 64 / 4096 entries (O(1): a keyed mix picks a run of 8 slots; 3% / 2% of these SYNs overflow to cookies; the linear scan it replaced: 68.4-82.8 / 2004-2300 ns minimums), and with a cookie (ns/SYN; load about 8, 3 interleaved sessions) | Host, M4 Pro | 47.6 / 44.7 / 29.2 | 49.8 / 45.9 / 30.4 | phase 8 step 47 review |
| `net` TCP simulated goodput (virtual time, deterministic per seed), NewReno, 16 MiB A to B, 1 MiB window, RTO floor 200 ms, 11 seeds: 1% loss at 10 / 50 ms RTT, 5% loss at 10 / 50 ms RTT (MiB/s, higher is better; step 48's SACK baseline, same seeds, window and floor) | Host, simulated | 1.37 / 0.28 / 0.30 / 0.11 | 1.52 / 0.31 / 0.34 / 0.11 | phase 8 step 47 review |
| `mogfs` (v1) create + 100-byte write + commit, 400 files, 16384-block (64 MiB) in-memory disk, from a scratch copy of its bench (ns/op) | Host, M4 Pro, 3 runs interleaved with `mogfs2` | 2007 | 2087 | phase 7 step 39 |
| `mogfs2` create + 100-byte write + commit, 400 files in one directory, 1024-block in-memory disk, 64-slot cache (ns/op; v1 1951-1976 interleaved) | Host, M4 Pro, 6 runs interleaved with v1 | 1807 | 1853-1900 | phase 7 step 39 |
| `mogfs2` the same on a 16384-block (64 MiB) disk (ns/op; v1 2076-2098 interleaved) | Host, M4 Pro, 3 runs | 1869 | 1934-1951 | phase 7 step 39 |
| `mogfs2` lookup in a 400-entry directory (ns/op; checks the entry against the inode it names) | Host, M4 Pro | 115 | 121 | phase 7 step 39 |
| `mogfs2` lookup in a 100k-entry directory, 64-slot cache (ns/op; two leaf reads and checks per lookup) | Host, M4 Pro, 11 runs | 1359 | 1401 | phase 7 step 39 |
| `mogfs2` 1 GiB file: sequential 1 MiB writes + commit / sequential read, in-memory disk (MiB/s, higher is better) | Host, M4 Pro, 5 runs (busy machine) | 3832 / 5829 | 7413 / 11364 | phase 7 step 39 |
| `mogfs2` mount after the 1 GiB file (ns; 16 requests: the superblocks, the live bitmap index and pages, the rightmost path, the older slot's index and the pages it does not share) | Host, M4 Pro, 5 runs | 16375 | 17125 | phase 7 step 39 |
| `mogfs2` 64 MiB file sequential read, fresh / after 16384 random 4 KiB overwrites + commit (fragmentation; MiB/s) | Host, M4 Pro, 11 runs | 8757 / 4335 | 12683 / 4889 | phase 7 step 39 |
| Kernel boot, kmain to end of init (us) | QEMU TCG, dev build | 2928 | 3140 | uncommitted |
| Kernel boot, kmain to end of init (us) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots | 157 | 178 | phase 3 step 11 |
| Kernel boot, kmain to end of init, `-smp 1` (us; base 179 / 199 and its A/A copy 178 / 199 in the same run) | QEMU hvf (`-cpu cortex-a72`), dev build, 63 interleaved boots, load about 11 | 181 | 199 | phase 5 step 25a |
| Kernel boot, kmain to end of init, `-smp 4` (us; base, whose secondaries stay off, 181 / 207; core 0 pays one `CPU_ON`, core 1 starts the rest) | QEMU hvf (`-cpu cortex-a72`), dev build, 63 interleaved boots, load about 11 | 187 | 228 | phase 5 step 25a |
| SGI round trip between core 0 and core 1, `test=bench-ipi`, 1000 trips (ns; the target woken from `wfi` each time; GICv2 base 28877 median in the same run) | QEMU hvf (`-cpu cortex-a72`), dev build, `-smp 4`, 11 interleaved boots, load 15-27 | 11837 | 16464 | phase 5 step 25c |
| `arch::cpu()` / `PerCpu::with` round trip, `test=bench-lock` (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 11 boots | 1.0 / 4.6 | 1.0 / 4.6 | phase 5 step 25c |
| `bench-smp` aggregate throughput at k = 1, 2, 4, 8, 12 workers (ops/s): 0-byte `write`; pipe round trips with own `pong`; `spawn` + `wait` of `nop` | QEMU hvf (`-cpu cortex-a72`), dev build, `-smp 12`, 5 boots, load 16-49 (medians) | | syscall 14.4 M, 10.9 M, 7.1 M, 2.1 M, 0.02 M; pipe 13.4 k, 8.6 k, 4.3 k, 5.4 k, 2.5 k; spawn 53.9 k, 41.7 k, 13.6 k, 9.2 k, 1.5 k | phase 5 step 25c |
| Cores online, `start_cpus` to the last core's GIC up (us; tree bring-up) | QEMU hvf (`-cpu cortex-a72`), one boot each, load 100+ | | 327 at 4, 824 at 12, 5063 at 64 | phase 5 step 25c |
| Yield round trip via `svc`, `test=bench`, 100000 trips (ns) | QEMU TCG, dev build, 11 boots | 1178 | 1218 | uncommitted |
| Yield round trip via `svc`, `test=bench`, 100000 trips (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots | 68 | 70 | phase 3 step 16 |
| Syscall round trip from EL0, `test=bench-syscall`, 100000 `print(sp, 0)` timed in user space (ns; before `print` became `write`) | QEMU TCG, dev build, 11 boots | 621 | 637 | phase 3 step 11 |
| Syscall round trip from EL0, `test=bench-syscall`, 100000 `io_submit_wait(console, write, sp, 0)` timed in user space (ns; one `KERNEL` lock round trip per trap since step 24, base 28 / 30 in the same run) | QEMU hvf (`-cpu cortex-a72`), dev build, 63 interleaved boots, load about 10 | 29 | 32 | phase 5 step 24 |
| Pipe round trip, `test=bench-pipe`: one byte to `pong` and back over two pipes, 100000 trips, timed by the kernel from spawn to exit (ns) | QEMU TCG, dev build, 11 boots | 14180 | 14556 | phase 3 step 15 |
| Pipe round trip, `test=bench-pipe` (ns; six traps, so six `KERNEL` lock round trips, since step 24; since step 26 a read that waits skips the user-buffer probe; base 383 / 389 in the same run) | QEMU hvf (`-cpu cortex-a72`), dev build, 63 interleaved boots, load about 20 | 344 | 349 | phase 5 step 26 |
| Thread round trip, `test=bench-threads`: `thread` + `wait` (join) + `close` of a thread that exits at once, 1000 trips, timed in user space (ns; word-level `alloc_contiguous` since `efac888`) | QEMU hvf (`-cpu cortex-a72`), dev build, 63 boots, load about 20 | 343 | 358 | phase 5 step 26 |
| Same-process switch, `test=bench-threads`: one byte to a thread of the same process and back over two pipes, 100000 trips, timed in user space (ns; `bench-pipe` across two processes, with a TTBR0 write per switch, is 349 in the same run; no user yield exists, so this stands for "yield between two threads") | QEMU hvf (`-cpu cortex-a72`), dev build, 63 boots, load about 20 | 313 | 321 | phase 5 step 26 |
| Uncontended lock round trip, `test=bench-lock`: 10^7 acquire + release on one core; ticket (`lock_masked` + drop: `ldxrh`/`stxrh`, `ldarh`, `stlrh`) / test-and-set (`swap(true, Acquire)` + `store(false, Release)`) (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 42 boots | 2.8 / 6.0 | 2.9 / 6.1 | phase 5 step 24 |
| Disk throughput, `test=bench-disk`: 2048 sequential virtio-blk blocks (8 MiB raw image) written then flushed, then read, polled, one request in flight; 4 KiB per request, then 256 KiB (MiB/s, higher is better; 4 KiB write+flush / read / 256 KiB write+flush / read) | QEMU TCG (`-global virtio-mmio.ioeventfd=off`), dev build, 1 boot | - | 159 / 206 / 1020 / 4032 | phase 4 step 20 |
| Disk throughput, `test=bench-disk` (MiB/s, as above; machine busy with parallel builds) | QEMU hvf (`-cpu cortex-a72`, `-global virtio-mmio.ioeventfd=off`), dev build, 21 boots | 101 / 126 / 1389 / 3470 | 148 / 188 / 3415 / 8629 | phase 4 step 20 |
| Kernel boot with a MogFS disk mounted (us; `mog_os` before step 22 did not mount: 224 / 245) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 interleaved boots | 333 | 386 | phase 4 step 22 |
| File round trips, `test=bench-fs`: `open(CREATE \| TRUNC)` + 100-byte write + `sync` + `close` on one file in the root, 1000 trips, timed in user space (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots | 122665 | 136717 | phase 4 step 22 |
| File round trips, `test=bench-fs`: `open` (one `lookup`) + `close`, 100000 trips (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots | 109 | 111 | phase 4 step 22 |
| Syscall round trip through musl, `sh -c cbench`: 100000 `write(1, "", 0)` (the `test=bench-syscall` call through the dispatcher) timed in user space (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 11 boots, load about 25 | 36 | 37 | phase 4 step 23 |
| busybox spawn, `sh -c cbench`: `posix_spawn` + `waitpid` of busybox `true`, 100 trips, timed in user space (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 11 boots, load about 25 | 26367 | 27407 | phase 4 step 23 |
| Spawn round trip, `test=bench-spawn`: `spawn` of `nop` (one page of program) + `wait` + `close`, 1000 trips, timed in user space, without / with two arguments (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 42 boots, load about 9 | 3096 / 3254 | 3430 / 3604 | phase 4 shell review |
| Spawn round trip, `test=bench-spawn`, as above (ns; bit-by-bit `alloc_contiguous` base 3348 / 3372 min, 3516 / 3719 median in the same run) | QEMU hvf (`-cpu cortex-a72`), dev build, 31 interleaved boots, load about 11 | 2898 / 3005 | 3122 / 3329 | `mm` word-wise `alloc_contiguous` |
| `spawn` / `spawn-args` per call, `scripts/bench.sh bench-syscalls` (ns; base 2393 / 2576 min, 2479 / 2698 median in the same run; every other call within noise) | QEMU hvf (`-cpu cortex-a72`), dev build, 31 interleaved rounds, load about 13 | 1623 / 1857 | 1706 / 1911 | `mm` word-wise `alloc_contiguous` |
| Kernel boot, kmain to end of init, `-smp 1`, no disk (us; base 188 / 243 in the same run, held: boot's one `alloc_contiguous` is under the noise) | QEMU hvf (`-cpu cortex-a72`), dev build, 41 interleaved boots, load about 11 | 194 | 243 | `mm` word-wise `alloc_contiguous` |
| Kernel boot with a MogFS disk mounted, mount in 3 requests (us; 2-block superblock read) | QEMU hvf (`-cpu cortex-a72`), dev build, 42 interleaved boots (busy machine) | 294 | 342 | phase 4 shell commands |
| `Board::disk` probe, timed in the kernel around the call (us; no disk: one device-ID read; disk: one read plus the setup) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots each | 1 / 45 | 2 / 50 | phase 4 step 20 |
| Network: `test=bench-net` with `net=10.0.2.15/24,gw=10.0.2.2 udp=<port>`, 64-byte UDP datagrams to a host echo (`python3`, on 127.0.0.1) through QEMU's user network: one round trip / one send of a 10000 burst / one datagram each way with 16 in flight (ns; QEMU's user network and the host echo dominate: each send is one queue notify, which QEMU serves in the vCPU thread with a host `sendto`) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots, load about 39 | 42820 / 13522 / 16049 | 56683 / 16135 / 20201 | phase 8 step 49 |
| Kernel boot with a NIC and `net=` (us; the NIC's setup and its 66 frames; without `net=` the NIC is never probed; base without a NIC 198 / 236 in the same run) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 interleaved boots, load about 39 | 260 | 296 | phase 8 step 49 |
| Loopback TCP, `test=bench-sockets` (`nettest bench` against `nettest benchserve`, each its own process): 64-byte send + receive round trip / connect + close / one 4 KiB send of a 16 MiB stream (ns; the stream is 1134 MiB/s at the median; the pipe's round trip is about 390 ns in the same conditions: each TCP round trip also carries two segments, four syscalls a side and the net task's polls) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots, load about 9 | 2824 / 2376 / 3371 | 2889 / 2516 / 3446 | phase 8 step 50 |
| HTTP through QEMU's `hostfwd` (`test=httpd`): host to guest, a GET round trip on a new connection timed by a Python client (us) / guest to host, `fetch` of a 20-byte page from a Python server, 200 GETs, mean per GET (us; 3 boots) / 64 MiB to `httpd`'s echo, both ways at once (MiB/s each way; 3 runs) / 64 MiB fetched from the host (MiB/s; 3 boots). QEMU's user network ends TCP in QEMU, so these measure it more than our stack; recorded as found (min / median columns: best and median of the runs) | QEMU hvf (`-cpu cortex-a72`), dev build, load about 31 | 104 / 195 / 67.5 / 148 | 156 / 212 / 66.3 / 148 | phase 8 step 51 |
| Phase 8 (steps 49-51) against main `a597dcf`, exact TCG instruction counts (`-icount shift=0`): yield / syscall / pipe round trip, boot without bootargs (instructions; boot in us of 1000) | QEMU TCG, `-icount shift=0`, dev build | 345 / 218 / 2480 / 156 | same; main 345 / 218 / 2480 / 163 | phase 8 step 51 |
| Kernel boot with a NIC and `net=`, against main `22c07bc` (us; the `rng-seed` read joined the bootargs walk, about 45k instructions removed; the NIC probe and setup, ring memory and stacks, about 60 us under hvf, moved to the net task, which a scenario waits for after `boot:`, so that part is moved, not removed): exact TCG instruction counts (`-icount shift=0`, thousands) with / without a NIC, then hvf median of 31 interleaved boots with a NIC | QEMU TCG `-icount` / hvf, dev build, load 25 to 60 | 164 / 157 (main 264 / 156) | hvf 278 (main 378) | phase 8 follow-ups |
| Boot to network ready with a NIC and `net=` (`net: ready <N> us`, printed once the net task's setup is done, before any scenario), against main `22c07bc`, which set up inside `boot:` (thousands of TCG instructions, `-icount shift=0`): `boot:` / `net: ready` | QEMU TCG `-icount`, dev build | 164 / 226 | main 264 / (264) | phase 8 follow-ups |

## Cross-OS comparison

`scripts/oscompare.sh [runs]` (default 21) runs the same C benchmarks, `c/oscb.c`, on MogOs, Linux in the same QEMU
setup, and the macOS host as a bare-metal reference, plus MogOs's own Rust benchmarks. It fetches and builds
everything into the gitignored `third_party/oscompare/`, boots each OS once per run, interleaved (each run: MogOs
`oscb` from msh, each native MogOs test, Linux, Linux with `mitigations=off`, macOS), and prints the table below with
the configuration and versions. A 21-run pass takes about 20 minutes on a busy machine.

### Fairness rules

- Same host, same QEMU binary (9.2.1), `-M virt -cpu cortex-a72 -accel hvf -m 128M -smp 1` for every guest. TCG
  numbers are informational only and not part of this table. `-smp 1` changes when MogOs gets SMP (phase 5); then both
  sides move to the same core count.
- Same disk: a fully allocated 64 MiB raw image, fresh for every boot, one `virtio-blk-device` with
  `virtio-mmio.force-legacy=false` and `virtio-mmio.ioeventfd=off`, QEMU's default cache mode.
- Same source and libc: `oscb.c` (and its spawn target `oscnop.c`) is built against musl 1.2.5 on both guests, with the
  release and SHA-256 pinned in `c/Makefile`: MogOs's build through `c/mog-cc` (its syscall layer, soft-float, `-Os`),
  Linux's from the unpatched release with the same clang (`/opt/homebrew/opt/llvm/bin/clang`), `-Os` and the
  toolchain's `rust-lld`, static. macOS builds it with `xcrun clang -Os` against its own libc.
- Same timer: `CNTVCT_EL0`/`CNTFRQ_EL0` read from user space on MogOs and Linux. macOS returns garbage for an EL0
  `mrs cntvct_el0`, so there `mach_absolute_time` reads the same 24 MHz counter.
- Each benchmark is its own process (`oscb <bench> <dir> <nop>`), one batch per boot; the table gives the median of
  the runs and the best run (min for ns, max for MiB/s). A benchmark that fails prints `oscb: error <name>` and the
  rest still run.
- Nothing is tuned on either side: Alpine's stock `virt` kernel and default ext4 options, MogOs's release build.
  Linux runs twice, mitigations default and `mitigations=off`; the guest prints `/sys/devices/system/cpu/vulnerabilities`
  so the mode is recorded. On `cortex-a72` under hvf Meltdown is "Not affected" (no KPTI); of what `mitigations=`
  controls, only Spectre v2's BHB mitigation changes. Alpine's other hardening stays on in both Linux columns (below).

### What each row measures

| Row | `oscb` (every OS) | MogOs native column (Rust, for reference) |
| --- | --- | --- |
| `getppid` | `getppid()`, 100000 times: Linux's null syscall. MogOs's libc answers it without trapping, so it has no ratio | n/a |
| `write0` | `write(1, p, 0)`, 100000: the console on the guests (Linux: through the tty layer), the log file on macOS; its ratio is against Linux's `getppid` | `test=bench-syscall`, the same call without libc |
| `yield` | `sched_yield` between two processes, 100000 round trips | n/a: MogOs has no user-space yield call (`test=bench` times kernel tasks) |
| `pipe` | 1 byte to a `posix_spawn`ed partner and back over two pipes, 100000 | `test=bench-pipe`, timed by the kernel from spawn to exit |
| `spawn` | `posix_spawn` of `oscnop` by path + `waitpid`, 1000 | `test=bench-spawn`: native `spawn` of the one-page `nop` from a held handle |
| `open+close` | `open(O_RDONLY)` + `close` of a file, 100000 | `test=bench-fs` |
| `create+write+fsync` | `open(O_CREAT \| O_TRUNC)` + 100 bytes + `fsync` + `close`, 1000 | `test=bench-fs` (`sync` instead of `fsync`) |
| `readdir1000` | `opendir` + `readdir` of 1000 entries + `closedir`, 100 times | n/a |
| `file-*-256k` | 8 MiB in 256 KiB writes + `fsync`, then read back warm (whatever the OS caches) | n/a |
| `raw-*` | Linux: `O_DIRECT` on the whole disk, 8 MiB written + `fsync`, then read, 4 KiB then 256 KiB per call; not run on macOS | `test=bench-disk`: the same in the kernel, polled (MogOs has no raw-device path from C, so these ratios are native) |

Linux setup: the kernel, initramfs and modloop of `alpine-virt-3.24.2-aarch64.iso` (Linux 6.18.52, pinned by
SHA-256), plus a small overlay initramfs with `oscb`, `scripts/oscompare-init.sh` as `rdinit`, and `mke2fs` from the
ISO's packages. The modloop (ext4's module) is a second, read-only virtio disk, so it never sits in the guest's RAM;
the init loads the modules, deletes the initramfs's module tree, runs the raw test, then `mke2fs -t ext4 -b 4096
-E lazy_itable_init=0,lazy_journal_init=0` (no background init thread) and mounts it with defaults (`rw,relatime`,
`data=ordered`). MogOs runs `sh -c 'oscb syscalls / oscnop; ...'` from msh on a fresh MogFS image.

Known differences, not corrected for:

- Memory: Linux reports `MemTotal` 89 MiB and `MemAvailable` 52 MiB at benchmark time; MogOs has about 126 MiB of
  free frames. No benchmark here comes close to either.
- `write0` on Linux crosses the tty layer, which MogOs's console write does not have; `getppid` is Linux's
  null-syscall floor (105 ns), so the table compares MogOs's `write0` with it.
- Timer tick and hardening (from Alpine's `config-6.18.52-0-virt`, sizes unmeasured): Linux runs a 1000 Hz tick
  (`CONFIG_HZ=1000`; `NO_HZ_FULL` is built but not enabled), each tick a VM exit through QEMU's GIC, while MogOs's
  `test=shell` never starts its timer, so it runs without ticks or preemption. Linux also keeps
  `INIT_ON_ALLOC_DEFAULT_ON`, `HARDENED_USERCOPY`, `RANDOMIZE_KSTACK_OFFSET` (on every syscall) and
  `STACKPROTECTOR_STRONG`, which `mitigations=off` does not turn off.
- `fsync`: ext4 commits the journal for one file; MogOs's libc maps `fsync` to `sync`, a whole-tree MogFS commit (the
  dirty inode-table block and the superblock, two flushes). Both end in QEMU flushing the image file on the host,
  which dominates and is noisy (see the min/median spread).
- macOS runs on all 12 cores, another kernel and CPU mode (bare metal, no hypervisor): `yield` and `pipe` cross cores
  there, `fsync` does not flush the drive (`F_FULLFSYNC` would), `oscnop` is dynamic. A reference, not a rival.

### Skipped

- FreeBSD: an official arm64 VM image is about 594 MiB compressed, plus 156 MiB of `base.txz` for a sysroot to build
  `oscb`; it boots through UEFI, macOS cannot write files into its UFS image (so running the benchmarks needs
  cloud-init or serial-console automation), and 128 MiB is below its supported minimum. Past a reasonable download and
  setup for one more column; revisit if a third data point is needed.
- TCG: informational only by the rules above.

### Results

2026-10-07, Apple M4 Pro (12 cores), macOS 26.6.2, QEMU 9.2.1 hvf, MogOs `09053b6` plus `oscb` in the boot archive,
release build, Alpine 3.24.2 (Linux 6.18.52-0-virt), musl 1.2.5 on both guests, 21 interleaved runs; the table is
the script's output (columns renamed). Busy machine:
load average 15 before, 11 after (other agents building), so the disk and `fsync` rows are noisy. Cells: median
(best). Ratio: Linux (mitigations default) median over MogOs `oscb` median for ns, the inverse for MiB/s, so above 1
means MogOs is faster; "native" marks a row only MogOs's Rust benchmark covers. Every MogOs `yield`, `readdir1000`
and `file-*` run failed (`oscb: error`, reasons below).

| Benchmark | Unit | MogOs `oscb` | MogOs native | Linux | Linux `mitigations=off` | macOS host (reference) | MogOs vs Linux |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `getppid` | ns | 2.0 (1.9) | n/a | 105.3 (100.7) | 82.9 (80.5) | 74.7 (71.8) | n/a |
| `write0` | ns | 39.8 (34.9) | 30.0 (28.0) | 1245.4 (1219.1) | 1216.0 (1196.2) | 450.6 (394.8) | 2.65x vs Linux `getppid` |
| `yield` | ns | n/a | n/a | 554.9 (537.5) | 501.4 (496.8) | 155.8 (94.4) | n/a |
| `pipe` | ns | 441.7 (409.0) | 367.0 (355.0) | 1055.9 (1036.4) | 963.0 (947.1) | 5091.4 (4635.7) | 2.39x |
| `spawn` | ns | 18027.1 (16495.0) | 3289.0 (3034.0) | 18456.9 (16628.8) | 18139.4 (16946.8) | 1354800.4 (1222897.6) | 1.02x |
| `open+close` | ns | 156.0 (152.5) | 93.0 (89.0) | 453.9 (445.7) | 404.3 (399.1) | 7562.7 (7356.1) | 2.91x |
| `create+write+fsync` | ns | 163829.0 (119608.4) | 146639.0 (119856.0) | 227311.2 (198542.0) | 231912.0 (201045.7) | 52674.0 (45583.5) | 1.39x |
| `readdir1000` | ns | n/a | n/a | 79399.2 (75798.3) | 79562.1 (72553.8) | 291324.6 (262707.9) | n/a |
| `file-write+fsync-256k` | MiB/s | n/a | n/a | 2632.5 (2867.4) | 2632.4 (3097.9) | 3765.9 (4451.5) | n/a |
| `file-read-256k` | MiB/s | n/a | n/a | 35430.9 (36781.6) | 34169.8 (36831.0) | 18790.4 (22349.0) | n/a |
| `raw-write+flush-4k` | MiB/s | n/a | 176.0 (198.0) | 97.7 (114.4) | 106.6 (129.2) | n/a | 1.80x (native) |
| `raw-read-4k` | MiB/s | n/a | 226.0 (254.0) | 122.4 (146.7) | 127.3 (137.7) | n/a | 1.85x (native) |
| `raw-write+flush-256k` | MiB/s | n/a | 3449.0 (4134.0) | 2394.3 (2749.2) | 2579.7 (3164.2) | n/a | 1.44x (native) |
| `raw-read-256k` | MiB/s | n/a | 8032.0 (11347.0) | 4686.4 (6660.9) | 5241.3 (6570.4) | n/a | 1.71x (native) |

### Analysis

Where MogOs wins (mechanisms to keep, not margins to bank on):

- Syscall entry: 40 ns through musl (30 ns native) against Linux's 105 ns `getppid` (83 ns with `mitigations=off`, so
  about 22 ns of Linux's is the Spectre-v2 BHB mitigation, which MogOs does not have yet; kernel-stack randomization
  and the tick are further unmeasured shares). Linux's `write0` adds the tty layer's locks (1245 ns).
- Pipe round trip, 2.4x: a direct switch on one core without wait queues, fd-table locking or RCU.
- `open+close`, 2.9x: a lookup in a one-block directory whose inode table is in memory, against a path walk with
  permission checks, a `struct file` and fd-table updates.
- `create+write+fsync`, 1.4x, but inside host-flush noise (best runs: 120 µs vs 199 µs): MogFS commits with two
  flushes and no journal.
- Raw disk, 1.4-1.9x (native), the weakest claim: an in-kernel loop with no syscalls polling one request, against
  user-space `O_DIRECT` through blk-mq with an interrupt per completion (through QEMU's GIC). Polling will not survive
  concurrent I/O, so this margin is not one to keep; a C raw-device path would make the row same-source.

Where MogOs loses or cannot run yet (work items, most likely cause first):

1. `spawn` through musl is a tie with Linux (18.0 vs 18.5 µs) although native `spawn` is 5.5x faster (3.3 µs): the
   libc path costs about 15 µs. Likely causes, unmeasured: `__mog_start` maps and zeroes a 128 KiB stack (32 frames)
   at every start, `oscnop` copies about 10 pages of program and zeroes about 5 of BSS eagerly against `nop`'s one page, and
   `vfork` + `execve` + `wait4` add the fd-table save, argument-string building and a pid-table lookup. Fixes: profile
   it first; then a lazily grown or smaller initial stack, demand paging (phase 6) so untouched pages cost nothing,
   and a smaller C runtime image.
2. libc overhead on the fast paths: `open+close` 156 ns via musl vs 93 native, `write0` 40 vs 30, `pipe` 442 vs 367.
   The fd-table indirection in `c/musl/src/mogos/mogos.c` sits on every call, and lexical path resolution on every
   path-based one (`open`). Fix: a
   per-call profile, then trim the dispatcher (for example resolve relative paths against a cached directory handle
   instead of rebuilding from the root).
3. `readdir1000` and `file-*`: fail, checked in a boot: `readdir` creates 500 of the 1000 files before `open` fails,
   and `fileio` creates `seq` but its first 256 KiB write fails. MogFS v1 holds 504 inodes and files up to 57232 bytes (14 direct pointers), and
   there is no page cache, so warm reads would hit the disk. Fix: MogFS v2 with extents and larger directories
   (phase 7) and the kernel page cache (phase 6, D2 in `docs/research/linux-survey.md`). Expect `file-read` to lose
   until the page cache lands (Linux reads at 35 GiB/s from memory).
4. `yield`: fails (`sched_yield` is `ENOSYS` in the libc, no native call). Fix: a yield syscall, so the scheduler's
   switch cost is measured against Linux's 555 ns.
5. `fsync` is a whole-tree commit without group commit: one writer ties or wins, many concurrent writers will lose to
   ext4's journal batching. Fix: per-file `fsync` and group commit (survey M8, phase 7); measure with several writers
   once SMP exists.
6. Missing hardening makes part of the syscall margin unpaid: Linux pays about 22 ns for BHB here, plus unmeasured
   kernel-stack randomization and a running tick. Fix: phase 10 step 60a (QEMU answers no SMCCC call, so the
   loop, not firmware) and the timer on during benchmarks; then compare MogOs-with-mitigations against the `Linux`
   column. Every such cost is a row of the debt ledger below.
7. Single core only: Linux's numbers include SMP-safe locking MogOs does not need yet. Rerun at the same core count
   when phase 5 lands; the locks added then must not eat these margins.

### Debt ledger

Linux costs MogOs does not pay yet, each tied to the rows it inflates. Rule: every cross-OS rerun states which rows
are paid, and a paid row records its measured cost (hvf A/B against its base, as in Workflow above).
Status is unpaid, paid (with the measured cost), or declined (argued in the linked doc; the rerun still isolates
Linux's share, so the margin is labelled "by design", not banked). "Linux here" is Linux on the setup above: its
`vulnerabilities` files read the same in both columns except `spectre_v2` ("CSV2, BHB" against "CSV2, but not
BHB"), which is what lets the `getppid` delta be charged to BHB alone.

| Debt | Affects | Linux here | MogOs plan, and why it should cost less | Lands | Status |
| --- | --- | --- | --- | --- | --- |
| Spectre-BHB and branch-predictor hardening on entry from EL0 | `getppid`, `write0`, `pipe`, every trap | BHB: 22.4 ns per syscall (`getppid` 105.3 vs 82.9 with `mitigations=off`), Linux's k = 8 loop and `dsb nsh; isb` for A72. v2 predictor invalidation: 0, since under hvf the guest sees the host's CSV2 | Linux v6.18's decision order (CSV2_3 or the safe list, ECBHB, CLRBHB, loop, firmware) per core, with static vector tables built by `.irp`, one per k and barrier, the count an immediate. Only the EL0 entries run it, and `VBAR_EL1` is written once, so there is no trampoline hop or per-entry load. An unaffected core runs the plain table. Expect at or a little under Linux's 22 ns. v2's firmware call on real A72 before r1p0 (and Spectre-BSE with it) waits for hardware | phase 10 step 60a; v2 firmware call phase 11 | paid in 60a: 13 ns per EL0 trap under hvf (syscall 32 -> 45 ns, every `bench-syscalls` call about +13, pipe 357 -> 437 ns over six traps; yield and boot hold), the guest being an A72 r0p3 MIDR with the M4's CSV2 (`loop8-dsb`: 27 instructions); 75 instructions on TCG `cortex-a76` (k = 24). v2 firmware call unpaid |
| Spectre v1 user-pointer and index masking | `write0`, `pipe`, `open+close`, file rows | unmeasured (`__user pointer sanitization` in both columns; `mitigations=off` does not turn it off): a `bic` of bit 55 per user copy and `cmp`/`sbc`/`csdb` per `array_index_nospec` | A clamp ending in `csdb` on every user-derived index: one `mask_user` per user buffer, the handle index, the syscall number, MogFS's block index from `offset` and `start`; the list is exhaustive and reviewed. About 1 ns per buffer or handle, and `mask_user` becomes Linux's single `bic` once phase 6 puts the kernel in TTBR1 | 60b | paid in 60b: a `csdb` costs 8.8 ns on the M4 (host loop; hvf runs it natively). Each call clamps only what it indexes with behind at most one `csdb` (the number is masked, no barrier): about 9-10 ns per call that indexes (4-27 TCG instructions), 0 for one that does not. Linux's own entry pays an `array_index_nospec` (`csdb`) on the number too |
| Kernel stack offset randomization | `getppid`, `write0`, every syscall | unmeasured (`RANDOMIZE_KSTACK_OFFSET` in both columns: a `get_random_u16()` per syscall on 6.18 arm64; isolate it with `randomize_kstack_offset=off`) | Not built: it blurs the stack layout for kernel-stack corruption and uninitialized-stack leaks, which safe Rust excludes and the `arch`/board `unsafe` surface (fixed 288-byte trap frame, no user-sized stack buffers) does not offer | phase 10 (decision) | declined ([phase-10-hardening.md](phases/phase-10-hardening.md)) |
| Kernel W^X mappings | `getppid`, `yield`, `pipe`, `spawn`, boot | in every row: strict kernel RWX, plus `rodata=full`, the 6.18 default (`rodata_full = true`), which maps the whole linear map with 4 KiB pages (no BBML2 on A72, inferred) so read-only kernel data has no writable alias | 4 KiB pages only for the 2 MiB holding the image, 2 MiB blocks for the rest of RAM, `SCTLR_EL1.WXN` as a free hardware check. The identity map holds the image once, so there is no alias to break up and no `rodata=full` cost. Expected about 0; the TLB cost is measured, with contiguous-bit 64 KiB runs as the fallback | 60c | paid in 60c: text, rodata and the rest are 2 MiB blocks (the 4 KiB image pages had cost the pipe round trip 14 ns), so syscall, yield, pipe and `spawn` hold; boot pays 4-5 us for the table fill with the MMU off (median boot holds against main); padding wastes 3.06 MiB of the text and rodata blocks. The `rodata=full` share declined by design (no alias exists) |
| SSBD (Spectre v4) | syscall and trap rows | 0: `spec_store_bypass: Vulnerable` in both columns (the model shows no SSBS, and QEMU answers no SMCCC call, so there is no firmware mitigation) | Where FEAT_SSBS exists, `SCTLR_EL1.DSSBS = 0` keeps the kernel mitigated on every entry for free, and `msr ssbs, #0` at boot covers code that runs before the first entry. On A72 (no SSBS) firmware `ARCH_WORKAROUND_2`, which Linux calls on every kernel entry and exit, is priced on hardware. Under QEMU the report matches Linux's | 60a (detect, report, SSBS); phase 11 (A72 firmware) | 60a done: detected and reported per core, `msr ssbs, #0` where FEAT_SSBS exists (TCG `cortex-a76`, `max`); unpaid on hardware; nothing owed under QEMU |
| Kernel stack guards (`VMAP_STACK`) | `spawn`, `pipe`, `yield` (stack setup and TLB) | in those rows: virtually mapped kernel stacks with guard pages | Safe Rust does not stop a stack overflow, so this is a real gap. 60c gives boot's stack an unmapped guard page, free at run time (25c moved the secondaries' stacks into per-CPU blocks in RAM's 2 MiB blocks, unguarded until phase 6, as `MAX_CPUS` and its linker stacks went). Per-task kernel stacks get guarded virtual stacks with phase 6's higher-half kernel | 60c; phase 6 | boot stacks paid in 60c, free at run time; per-task stacks unpaid |
| Pointer authentication of kernel return addresses (`ARM64_PTR_AUTH_KERNEL`) | every row | 0 here: the hvf `cortex-a72` guest is shown no PAuth (the model's ID_AA64ISAR1 is 0; inferred from QEMU 9.2.1's source, so the next rerun captures `/proc/cpuinfo` Features) | A72 has no PAuth (it arrived in Armv8.3). On hardware that has it, it guards the `unsafe` surface against return-address corruption, which safe Rust does not cover, so it is planned rather than declined. It needs stable rustc to emit `pac-ret`, and stable rustc 1.99 has no `-C branch-protection` (it is `-Z` only) | phase 11 (PAuth hardware) | unpaid; nothing owed here |
| Call-used register zeroing (`ZERO_CALL_USED_REGS`) | every row | unmeasured, in both columns | Declined: on Linux it limits leaks of stale register values and shortens ROP gadgets. No kernel value reaches EL0, since the trap exit reloads every user register from the trap frame. A gadget chain needs control-flow hijack first, which needs memory corruption that safe Rust excludes; for the `unsafe` surface that is PAuth's job | phase 10 (decision) | declined, by design |
| Stack zero-init (`INIT_STACK_ALL_ZERO`) | every row | unmeasured, in both columns | Declined: safe Rust cannot read an uninitialized stack slot | phase 10 (decision) | declined, by design |
| Stack canaries (`STACKPROTECTOR_STRONG`) | every row | unmeasured, in both columns | Declined: safe Rust bounds-checks every buffer, and the `unsafe` surface has no stack buffer sized by user input | phase 10 (decision) | declined, by design |
| Usercopy bounds checks (`HARDENED_USERCOPY`) | `write0`, `pipe`, file rows | unmeasured, in both columns | Declined: a user copy is a Rust slice copy, length-checked against both slices, and the user side is built by `user_bytes` | phase 10 (decision) | declined, by design |
| Zeroing on allocation (`INIT_ON_ALLOC_DEFAULT_ON`) | `spawn`, `map`, `pipe` | unmeasured, in both columns | Declined: safe Rust cannot read uninitialized heap memory, and frames given to user space are already zeroed (`map`), a cost the baselines already pay | phase 10 (decision) | declined, by design |
| List pointer checks (`LIST_HARDENED`) | `yield`, `pipe`, `spawn` | unmeasured, in both columns | Declined: the kernel has no pointer-linked lists; phase 5's run queues link by bounds-checked indexes | phase 10 (decision) | declined, by design |
| Multi-core wake and IPI paths | `pipe`, `yield`, `spawn` | in every number (Alpine is an SMP kernel even at `-smp 1`: real atomics, wait queues, RCU) | One big ticket lock first (one uncontended round trip per trap), the reschedule SGI only to wake idle cores (and only for a task still waiting at the end of the trap), deterministic placement with no wake-affine heuristics (step 28a's direct-switch `call` for ping-pong, two traps per round trip: about 220 ns at `-smp 1` and `-smp 4`, estimate), TLB shootdown by broadcast `tlbi ... is` with no IPI | phase 5 steps 24, 25b, 25c, 28, 28a | lock share paid in step 24: 2.9 ns per round trip, syscall 30 -> 32 ns, pipe 375 -> 389 ns; per-core scheduler paid in 25b/25c at `-smp 1` (against main `7565e10`, 63 boots: syscall 68 -> 69, yield 91 -> 93, pipe 629 -> 646 ns; TCG +152 instructions per yield and +274 per pipe round trip, not yet shown unavoidable); cross-core hand-off at `-smp 4` unpaid by design (pipe 412 -> 2289 ns, spawn 3.7 -> 18.6 us: an SGI and a vCPU wake per hand-off; `bench-ipi` 9-17 us under GICv3) |
| Per-core switch on one shared table, `-smp 1` | `yield`, `pipe`, `spawn`, every hook | (Linux's per-CPU run queues: no shared-table core lookup) | Step 28's per-core run queues, touched only by their own core, remove the core lookup and its bounds check per hook, the `on_core` set/clear and test, and the kick test | phase 5 step 28 | owed (25c follow-up, TCG instructions per round trip against main `e997c36`): yield 373 -> 453 (+80, two switches), pipe 2642 -> 2826 (+184), thread pipe 2626 -> 2808 (+182), syscall 245 -> 250 (+5), spawn 39534 -> 39829 (+295); hvf `-smp 1` yield 77 -> 78, pipe 528 -> 539 ns. Step 28's done-when brings yield and pipe at `-smp 1` back to `e997c36`'s counts |
| Big kernel lock under several cores | `bench-smp`, every syscall on several cores | per-object locks, RCU, per-CPU run queues | Split `KERNEL`: per-process locks first (map cursor, handle-table writes, futex waiters born there in 27; handle lookups by seqlock, with no lock; budgets atomic) under a compile-time lock order (step 26a), then per-core run queues with a wake inbox and per-pipe locks (step 28), a queued lock for what stays shared (step 32) | phase 5 steps 26a, 28, 32 | unpaid: `bench-smp` at `-smp 12` (25c) falls with workers: syscall 14.4 M ops/s at k = 1, 10.9 M at 2, 7.1 M at 4, 2.1 M at 8, 0.02 M at 12; pipe 13.4 k, 8.6 k, 4.3 k, 5.4 k, 2.5 k round trips/s; spawn 53.9 k, 41.7 k, 13.6 k, 9.2 k, 1.5 k/s |
| Fair-scheduler pick | `yield`, `pipe` (wake) | in `yield` (555 ns) and `pipe` (EEVDF pick, rbtree, `update_curr`) | EEVDF over a linear scan of the core's fair tasks with vruntimes relative to the queue minimum, no tunables, one cached timer deadline; RT tasks (the syscall and pipe benchmarks) never reach it | phase 5 step 29 | unpaid |
| Group accounting | `yield`, `pipe` | in the same rows (hierarchical `sched_entity` charging; the benchmarks run in the root group) | Groups do not nest: one entity per group in each core's fair queue, a token bucket only on groups with a quota, so an ungrouped task pays one charge | phase 5 step 30 | unpaid |
| Inode timestamps | `create+write+fsync`, `file-write+fsync-256k`, `readdir1000` | in those rows (`relatime`: mtime and ctime on every write, atime at most daily) | MogFS v2 inode items carry mtime, ctime and btime as 64-bit ns, written inside the inode item the commit already copies, the clock from `CNTVCT_EL0`; no atime, so a read never dirties an inode | phase 7 steps 39, 39b | unpaid |
| Directory and node cache for large file systems | `open+close`, `readdir1000` | in those rows (dcache and RCU path walk) | MogFS v2's fixed node cache over a hashed B+tree: a lookup is a hash and a descent through cached nodes, no dentry objects, refcounts or negative entries; v1 only wins today because its 504-inode table fits in memory | phase 7 steps 39, 39b | unpaid |
| Interrupt-driven, multi-request block I/O | `raw-*`, `file-*`, `create+write+fsync` | in those rows (blk-mq, an IRQ per completion through QEMU's GIC) | A batch of requests in flight with one notify, completion by IRQ on the submitting CPU, no I/O scheduler; today's poll of one request will not survive concurrent I/O, so the `raw-*` margins are the least earned | phase 7 steps 42, 44 | unpaid |
| Per-file `fsync` and group commit | `create+write+fsync` | in that row (one jbd2 commit, batched across writers) | `sync`s arriving during a commit share the next one; a file handle's `sync` commits the CoW tree in two flushes with no journal double write; a per-file log only if step 43's latency-under-streaming number asks | phase 7 steps 42, 43 | unpaid; measure with several writers once SMP lands |
| Demand paging (owed to MogOs: it makes us faster) | `spawn`, `map` | Linux already pays only for touched pages | Budget charged at `map`, frames on first touch, ELF pages lazily: `oscnop`'s eager copy and the 128 KiB stack zeroing go away; the price moves to first-touch faults, so `map` of one page and the fault path get measured | phase 6 | unpaid |
| Tracing hooks | every syscall and switch | near zero when off (static keys patch tracepoints to `nop`), plus the entry work-flag test | Per-CPU rings of fixed records; without code patching the hook is one load and branch on a flag the compiler can see, measured against a build without it | phase 10 step 60 (the survey's step-24 trace skeleton was not built in phase 5) | unpaid |
| Signals | syscall exit, `pipe` | in every row (a pending-work flag test on each return to EL0; signal checks in every wait) | No signal state in the kernel: libc builds signals from the exception channel and a notify bit that completes a wait; a remote stop reuses the switch path's kill mark and the reschedule SGI, so the syscall exit gains nothing | phase 9 step 53 | unpaid |
| Timer tick during benchmarks | every row | in every row (1000 Hz, each tick a VM exit) | MogOs's `test=shell` runs without its timer; once the shell runs in the fair class its slice timer is on, and the rerun keeps it on | phase 5 step 29 | unpaid |

At the 2026-10-07 run only the step-24 lock share was paid; 60a has since paid the BHB row. Linux's hardening defaults above come from Alpine's
`config-6.18.52-0-virt`. KPTI is in neither Linux column: A72 is on Linux's KPTI safe list, and `RANDOMIZE_BASE` is
not set, so `kaslr_requires_kpti` never forces it. BTI is off too (`ARM64_BTI_KERNEL` needs a non-GCC compiler in
6.18). The next rerun also captures the guest's `/proc/cpuinfo` Features, to confirm what hvf shows.
