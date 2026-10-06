# Phase 2: Time and Concurrency

Status: done. All done-when criteria pass in `cargo test-host` (step 7's `ticks: 3` is superseded by step 9).

Goal: the kernel runs several tasks and preempts them on a timer.

## Steps

Each done-when is an e2e assertion on the serial output. Clean-boot tests also assert no `panic:` line.

| # | Step | Done when |
| --- | --- | --- |
| 7a | Softfloat target + IRQ-masked heap | Kernel builds for `aarch64-unknown-none-softfloat` (no FP/SIMD in the kernel, so traps never save v-registers). The board's heap masks IRQs around each call (`arch` saves DAIF, masks, restores); no spinlock. Existing e2e tests pass. |
| 7 | GICv2 + EL1 virtual timer | GIC found in the DTB by `arm,cortex-a15-gic` (QEMU's default). The virtual timer (PPI 27, period from `CNTFRQ_EL0`) ticks while the kernel idles in `wfi`; after the `boot:` line it prints `ticks: 3` and powers off. |
| 8 | Tasks + `svc` yield | The trap frame is the task context; each task has its own kernel stack; the trap path returns the next task's frame. With `test=yield` (timer off), two tasks print `task a: 0`, `task b: 0`, `task a: 1`, `task b: 1`, `task a: 2`, `task b: 2` in strict alternation. |
| 9 | Preemptive round-robin | Task a starts first and spins forever without yielding; task b prints `task b: 0`..`task b: 2` and powers off, so each b line proves a timer preemption of a. Replaces the `ticks: 3` demo. |

## Benchmarks (hvf medians gate, TCG informational)

- Yield round trip with `test=bench`: two tasks ping-pong via `svc` N times; elapsed / N. Covers exception entry and context switch; runs only under the bootarg so normal boots don't pay for it.
- IRQ latency is not benchmarked yet: under hvf QEMU emulates the GIC in the host process, so the number would measure the host. Revisit with an in-kernel GIC or real hardware (needed for the deterministic-scheduling goal).

## Notes

- Scheduler state needs no lock on one core: it is touched only in trap context (IRQs masked by exception entry) or before IRQs are first unmasked. It lives in one board `static`; `arch` calls the board through one `#[unsafe(no_mangle)]` symbol, like `kmain`.
- Multicore spinning is out of scope.

## What was done

Filled in as each step lands.

- Step 7a: target is `aarch64-unknown-none-softfloat`; `llvm-objdump -d` of the dev kernel shows no FP/SIMD register operand in its 6169 instructions. Phase 1's `CPACR_EL1.FPEN` setup in `_start` is removed, so any FP/SIMD use now traps. The board's `#[global_allocator]` is a bare `linked_list_allocator::Heap` in an `UnsafeCell`, each alloc/dealloc wrapped in `arch::irq::disable()`/`restore()` (save DAIF, mask, restore); no spinlock. Clean-boot e2e test asserts no `panic:` line. hvf boot median 190 us before, 192 us after (within noise; [BENCHMARKS.md](../BENCHMARKS.md)).
- Step 7: `dtb::Dtb::gic` returns the `arm,cortex-a15-gic` distributor and CPU interface from `reg`; `arch::gic` (enable, `GICC_IAR` ack, EOI, addresses passed in) and `arch::timer::arm` (TVAL from `CNTFRQ_EL0`). The trap handler sends "current EL with SPx, IRQ" to the board's `#[unsafe(no_mangle)] board_irq`, which acks, re-arms the 10 ms virtual timer (PPI 27), counts the tick and EOIs. IRQs stay masked except inside `arch::irq::wait` (`wfi`, unmask, `isb`, mask), so the kernel's tick check never races the handler. After `boot:` the kernel waits for 3 ticks, prints `ticks: 3`, powers off; same under hvf (74 ms wall per boot). hvf boot medians before/after over three 21-run interleaved sets: 181/193, 206/214, 203/186 us (noise).
- Step 8: the trap frame is the task context. The vector asm does `mov sp, x0` on the frame address `aarch64_exception` returns, so `svc #0` (EC 0x15) switches by returning the next task's frame from the board's `#[unsafe(no_mangle)] task_switch`; no separate switch routine, frame layout unchanged. `kernel::Scheduler` (fixed array of frame addresses, slot 0 the boot context, round robin, `Full` when out of slots) sits in one board `static`. `Board::spawn` heap-allocates a 16 KiB stack, puts the task's board copy, entry and arg at its top and an `arch::new_task` frame below (ELR = trampoline, SPSR EL1h DAIF masked, x0 = arg). `test=yield`: the boot task yields 3 times, each running a then b, e2e asserts the exact six lines. `test=bench`: boot ping-pongs with one yielding task, hvf median 67 ns/round-trip (TCG 1218). hvf boot median within noise (198/192 us before, 177/204 after).
- Step 9: the IRQ path returns a frame like the `svc` path: the board's `board_irq(frame)` acks, and on a timer tick re-arms, EOIs and returns `task_switch(frame)` (the same scheduler call `svc` uses); other IRQs resume `frame`. New task frames start at EL1h with IRQs unmasked (SPSR `0x345`); the boot context stays masked except in `idle`'s `wfi` window, so a tick there switches away from boot mid-`wait`, which resumes at its re-mask when scheduled again. The scenario runs under `test=preempt`, not normal boot: like step 8's tests it needs tasks and a running timer that a plain boot does not, and it costs about 8 ticks (80 ms), so normal boot is now init only and powers off after `boot:` (the `ticks: 3` demo and `Board::ticks` are gone). Boot spawns a (spins on `spin_loop`) and b (prints, yields; powers off after `task b: 2`), starts the timer, idles; e2e asserts exactly the three b lines, also under hvf. `test=yield`/`test=bench` stay deterministic with IRQs unmasked in tasks: the GIC distributor and `CNTV_CTL` are off from reset and only `start_timer` (called only by `test=preempt`) enables them. Heap and `spawn` still mask around their critical sections; the IRQ handler neither allocates nor prints; exception entry masks IRQs, so a tick never nests in an `svc` switch. hvf yield round trip, 21 interleaved boots: median 61 ns before, 61 ns after (min 57/58; the 67 ns step-8 baseline re-measured at 61 today); boot median 196/194 us.
