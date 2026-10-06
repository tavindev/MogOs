# Benchmarks

Speed is a primary goal, so performance is tested like behavior: measured, recorded, and guarded against regressions.

## Two kinds

| Kind | What | How | Use for |
| --- | --- | --- | --- |
| Host | Pure-logic crates (allocators, parsers, encodings) | `benches/*.rs` with `harness = false`, timed with `std::time::Instant`, run on macOS | Algorithmic cost of safe crates |
| Kernel | Hot paths in the running kernel (exception entry, context switch, syscall, IPC, page fault, allocation) | Boot QEMU in bench mode, time with the ARM generic timer (`CNTVCT_EL0`), print results over the UART | Real kernel paths end to end |

- Kernel numbers under QEMU's default emulator (TCG) are only meaningful as relative comparisons between commits, not as absolute speed.
- For realistic absolute numbers, run with Apple's hypervisor, which executes natively on the M-series CPU. The boot path works under `-accel hvf -cpu cortex-a72` (PSCI power-off, exceptions, MMU, fault report). `-cpu host` (and `max`) abort at startup on QEMU 9.2.1 with an M4 host (`Property 'host-arm-cpu.sme' not found`).
- Host: `cargo bench-host`. Kernel boot time: every boot prints `boot: <N> us` (kmain entry to end of init, from `CNTVCT_EL0`/`CNTFRQ_EL0`); take min and median of 11 boots.
- No benchmark framework dependency (criterion etc.): compile time is expensive on this machine. Report min and median of N runs.

## Workflow

- Any change to a hot path includes before/after numbers from the relevant benchmark, run on the same machine and mode.
- A slowdown above 5% on a tracked benchmark needs an explicit justification in the change; otherwise it is a failing result.
- New hot paths (each roadmap step that adds one) get a benchmark when they land, alongside their end-to-end test.
- Record the current numbers in the Baselines table below whenever they change.

## Baselines

| Benchmark | Mode | Min | Median | Commit |
| --- | --- | --- | --- | --- |
| `mm` frames: alloc+free of 1000 frames, 128 MiB allocator (ns/op) | Host, M4 Pro | 4.6 | 5.2 | uncommitted |
| Kernel boot, kmain to end of init (us) | QEMU TCG, dev build | 2820 | 3268 | uncommitted |
| Kernel boot, kmain to end of init (us) | QEMU hvf (`-cpu cortex-a72`), dev build | 235 | 251 | uncommitted |
