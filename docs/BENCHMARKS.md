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
- Exact instruction counts: under TCG with `-icount shift=0,sleep=off` the virtual counter advances 1 ns per instruction, so a kernel benchmark's `ns/round-trip` reads as instructions per round trip, the same on every run. It finds where a few ns come from; it never gates (hvf does), since a probe or TTBR0 write costs far more under hvf than its one instruction.

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
| `mm` frames: `alloc_contiguous(4)` + free, 128 MiB allocator: empty / behind 725 reserved frames (boot's prefix) / behind 4096 frames with every fourth used (ns/op; bit-by-bit base 7.5 / 287 / 1464 min, 8.7 / 290 / 1513 median) | Host, M4 Pro | 7.4 / 12.3 / 46.5 | 7.6 / 12.6 / 47.1 | `mm` word-wise `alloc_contiguous` |
| `mogfs` create + 100-byte write + commit, 400 files in one directory, in-memory disk (ns/op) | Host, M4 Pro | 1873 | 1956 | phase 4 MogFS unlink and rename |
| `mogfs` lookup in a 400-entry directory, in-memory disk (ns/op) | Host, M4 Pro | 758 | 784 | phase 4 MogFS unlink and rename |
| `net` UDP over the loss-free simulated link: `send_to` on A, `poll` + `recv_from` on B, batches of 16, 64-byte / 1472-byte datagrams (ns/datagram; 17.9 / 7.5 M datagrams/s at the median) | Host, M4 Pro | 53.6 / 130.3 | 55.8 / 133.7 | phase 8 step 46 |
| `net` UDP receive path: parse, checksum, demux, copy into the socket buffer and out with `recv_from`, 64-byte / 1472-byte datagrams (ns/frame) | Host, M4 Pro | 29.7 / 68.3 | 30.8 / 70.8 | phase 8 step 46 |
| Kernel boot, kmain to end of init (us) | QEMU TCG, dev build | 2928 | 3140 | uncommitted |
| Kernel boot, kmain to end of init (us) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots | 157 | 178 | phase 3 step 11 |
| Kernel boot, kmain to end of init, `-smp 1` (us; base 179 / 199 and its A/A copy 178 / 199 in the same run) | QEMU hvf (`-cpu cortex-a72`), dev build, 63 interleaved boots, load about 11 | 181 | 199 | phase 5 step 25a |
| Kernel boot, kmain to end of init, `-smp 4` (us; base, whose secondaries stay off, 181 / 207; core 0 pays one `CPU_ON`, core 1 starts the rest) | QEMU hvf (`-cpu cortex-a72`), dev build, 63 interleaved boots, load about 11 | 187 | 228 | phase 5 step 25a |
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
| Spectre-BHB and branch-predictor hardening on entry from EL0 | `getppid`, `write0`, `pipe`, every trap | BHB: 22.4 ns per syscall (`getppid` 105.3 vs 82.9 with `mitigations=off`), Linux's k = 8 loop for A72. v2 predictor invalidation: 0, since under hvf the guest sees the host's CSV2 | The same k = 8 loop plus `dsb nsh; isb`, on the EL0 vector entries only, in a second vector table that boot selects through `VBAR_EL1`, so an unaffected CPU runs the plain table: no per-entry branch, no per-CPU vector lookup. Per entry it cannot cost less than the loop, so expect about Linux's 22 ns. v2's firmware call on real A72 before r1p0 waits for hardware | phase 10 step 60a; v2 firmware call phase 11 | unpaid |
| Spectre v1 user-pointer and index masking | `write0`, `pipe`, `open+close` | unmeasured (`__user pointer sanitization` in both columns; `mitigations=off` does not turn it off) | Mask the user pointer after its `at s1e0*` probe and the handle index after its bounds check (`csel` + `csdb`) at the one probe helper and the one handle lookup every call already goes through: two instructions per user buffer or handle | 60b | unpaid |
| Kernel stack offset randomization | `getppid`, `write0`, every syscall | unmeasured (`RANDOMIZE_KSTACK_OFFSET` in both columns: a `get_random_u16()` per syscall on 6.18 arm64; isolate it with `randomize_kstack_offset=off`) | Not built: it blurs the stack layout for kernel-stack corruption and uninitialized-stack leaks, which safe Rust excludes and the `arch`/board `unsafe` surface (fixed 288-byte trap frame, no user-sized stack buffers) does not offer | phase 10 (decision) | declined ([phase-10-hardening.md](phases/phase-10-hardening.md)) |
| Kernel W^X mappings | `getppid`, `yield`, `pipe`, `spawn`, boot | in every row (strict kernel RWX: text, rodata and data mapped separately, so more TLB entries than one block) | 4 KiB pages only for the 2 MiB holding the image, 2 MiB blocks for the rest of RAM, and `SCTLR_EL1.WXN` as a free hardware check; expected cost about 0, measured as TLB pressure | 60c | unpaid |
| SSBD (Spectre v4) | syscall and trap rows | 0: `spec_store_bypass: Vulnerable` in both columns (no SSBS; QEMU answers no SMCCC call, so no firmware mitigation exists) | Where FEAT_SSBS exists, `SCTLR_EL1.DSSBS = 0` keeps the kernel mitigated on every entry for free; on A72 (no SSBS) firmware `ARCH_WORKAROUND_2`, which Linux calls on every kernel entry and exit, priced on hardware. Same report as Linux under QEMU | 60a (detect, report, SSBS); phase 11 (A72 firmware) | unpaid on hardware; nothing owed under QEMU |
| Multi-core wake and IPI paths | `pipe`, `yield`, `spawn` | in every number (Alpine is an SMP kernel even at `-smp 1`: real atomics, wait queues, RCU) | One big ticket lock first (one uncontended round trip per trap), the reschedule SGI only to wake idle or preempted cores, deterministic placement with no wake-affine heuristics, TLB shootdown by broadcast `tlbi ... is` with no IPI | phase 5 steps 24, 25b, 28 | lock share paid in step 24: 2.9 ns per round trip, syscall 30 -> 32 ns, pipe 375 -> 389 ns (in the results above); wake and IPI unpaid |
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

At the 2026-10-07 run only the step-24 lock share is paid. Not a row: KPTI. A72 is on Linux's KPTI safe list, but Linux
forces KPTI when KASLR is on and the CPU lacks E0PD (`kaslr_requires_kpti`), and the guest boots `quiet`, so whether
the `Linux` column pays it is unrecorded; the next rerun captures the `dmesg` line. MogOs decides KASLR and its KPTI
question in phase 6 ([phase-10-hardening.md](phases/phase-10-hardening.md), Notes).
