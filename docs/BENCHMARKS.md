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
- Record the current numbers in the Baselines table below whenever they change.

## Baselines

| Benchmark | Mode | Min | Median | Commit |
| --- | --- | --- | --- | --- |
| `mm` frames: alloc+free of 1000 frames, 128 MiB allocator (ns/op) | Host, M4 Pro | 4.6 | 5.2 | uncommitted |
| `mogfs` create + 100-byte write + commit, 400 files in one directory, in-memory disk (ns/op) | Host, M4 Pro | 1873 | 1956 | phase 4 MogFS unlink and rename |
| `mogfs` lookup in a 400-entry directory, in-memory disk (ns/op) | Host, M4 Pro | 758 | 784 | phase 4 MogFS unlink and rename |
| `net` UDP over the loss-free simulated link: `send_to` on A, `poll` + `recv_from` on B, batches of 16, 64-byte / 1472-byte datagrams (ns/datagram; 17.9 / 7.5 M datagrams/s at the median) | Host, M4 Pro | 53.6 / 130.3 | 55.8 / 133.7 | phase 8 step 46 |
| `net` UDP receive path: parse, checksum, demux, copy into the socket buffer and out with `recv_from`, 64-byte / 1472-byte datagrams (ns/frame) | Host, M4 Pro | 29.7 / 68.3 | 30.8 / 70.8 | phase 8 step 46 |
| `net` TCP goodput over the loss-free simulated link (no delay), A sends 64 MiB to B, which reads as it goes; 64 KiB / 1 MiB windows (MiB/s, higher is better; load about 8) | Host, M4 Pro | 6249 / 5617 | 6479 / 5740 | phase 8 step 47 |
| `net` TCP receive path per data segment (nearly all 1460 bytes): checksum, demux, sequence checks, copy into the ring, the ACK built, copy out with `recv`; recorded segments replayed into the same connection (ns/segment; load about 8) | Host, M4 Pro | 76.6 | 78.3 | phase 8 step 47 |
| `net` TCP connect + accept + close from each side, through TIME_WAIT, over the loss-free link, 10000 sequential connections (ns/connection; load about 8) | Host, M4 Pro | 420.7 | 430.6 | phase 8 step 47 |
| `net` TCP simulated goodput (virtual time, deterministic per seed), NewReno, 16 MiB A to B, 1 MiB window, RTO floor 200 ms, 11 seeds: 1% loss at 10 / 50 ms RTT, 5% loss at 10 / 50 ms RTT (MiB/s, higher is better; step 48's SACK baseline, same seeds, window and floor) | Host, simulated | 1.37 / 0.28 / 0.28 / 0.11 | 1.52 / 0.31 / 0.35 / 0.11 | phase 8 step 47 |
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
| Spawn round trip, `test=bench-spawn`: `spawn` of `nop` (one page of program) + `wait` + `close`, 1000 trips, timed in user space, without / with two arguments (ns) | QEMU hvf (`-cpu cortex-a72`), dev build, 42 boots, load about 9 | 3096 / 3254 | 3430 / 3604 | phase 4 shell review |
| Kernel boot with a MogFS disk mounted, mount in 3 requests (us; 2-block superblock read) | QEMU hvf (`-cpu cortex-a72`), dev build, 42 interleaved boots (busy machine) | 294 | 342 | phase 4 shell commands |
| `Board::disk` probe, timed in the kernel around the call (us; no disk: one device-ID read; disk: one read plus the setup) | QEMU hvf (`-cpu cortex-a72`), dev build, 21 boots each | 1 / 45 | 2 / 50 | phase 4 step 20 |
