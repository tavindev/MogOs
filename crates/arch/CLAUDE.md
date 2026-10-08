# `crates/arch` - AArch64 primitives behind safe wrappers

## What this crate is

The only architecture-specific crate: boot and secondary-core entry (`src/aarch64/boot.s`), exception vectors and `TrapFrame`
(`trap.rs`), the per-core vector table choice and the `spec:` report (`spec.rs`), MMU and page tables (`mmu.rs`), GICv3 (`gic.rs`: distributor and redistributor MMIO, CPU interface by system registers), the virtual timer (`timer.rs`), IRQ masking
(`irq.rs`), the only lock and per-CPU primitives (`lock.rs`), `uptime_us` (`mod.rs`), and the kernel's `memcpy` and
`memmove` (`mem.s`, overriding `compiler_builtins`' weak ones: 16 bytes per unaligned `ldp`/`stp`, no FP/SIMD; with the
MMU off a copy is legal only between 8-aligned buffers).

It is **NOT** board-specific: no MMIO addresses, no memory map, no drivers, no scheduling policy (those are
`crates/board/qemu-virt` and `crates/kernel`).

## Responsibilities

- `install_boot_vectors` (core 0 at boot: the plain EL1 entries, EL0 entries that panic), `install_vectors(conduit)`
  (each core, once, before it runs EL0 code: picks its table from its own MIDR and ID registers and the SMCCC
  workarounds behind the DT's PSCI conduit, in Linux v6.18 `proton-pack.c` order, writes `VBAR_EL1`, runs
  `msr ssbs, #0` where FEAT_SSBS exists; it reads an ID register, a trap under hvf, or asks the firmware only when the
  decision reaches it), `record_speculation(conduit)` (each core, once, off the boot path: returns its record, the table
  read back from `VBAR_EL1` with the v2, BHB, SSB, Meltdown and BSE states, which the board stores in its cores'
  table), `speculation(records)` (the worst record and how many cores share it, once all have recorded), the vector asm (17 static tables, 2 KiB
  apart in `spec::TABLES` order: plain, `clrbhb`, firmware workaround 3 by `hvc` and by `smc`, and the branch loop
  for each Linux k (8, 11, 24, 32, 38, 132) with `dsb nsh; isb` or `sb`, then the boot table; only entries 8-15, from EL0, run the
  mitigation, before their first branch) and `aarch64_exception` routing: IRQ, EL0 `svc`, EL0 fault, EL1 `svc #0` (yield),
  EL1 `brk #0` (self-test); anything else panics with ESR/FAR/ELR.
- `new_task`, `new_user_task`, `switch_el0_regs`, `TrapFrame::restart`.
- Descriptor encoding (the private `kernel`, `user_page`), `enable_mmu(&KernelMap)` (core 0 builds the boot tables: the
  device GiB PXN; the RAM GiB's level-2 table of 2 MiB blocks: the image's text blocks RX, its rodata blocks RO and PXN,
  the rest RW and PXN; RAM's first 2 MiB by a level-3 table: the DTB's pages RO and PXN, core 0's boot-stack guard page
  unmapped, the rest RW and PXN; all UXN and global; SCTLR's WXN set; then `aarch64_mmu_on`; the fill is plain stores
  of precomputed attributes, since every access is uncached with the MMU off),
  `secondary_entry` (PSCI `CPU_ON`'s entry: `aarch64_mmu_on` on the same table, then its per-CPU area, also its stack
  top, and its index from the context id), `map_device_gib`,
  `map_page` (`None` on a level-1 or level-2 block on the way, never writing a table into kernel memory), `unmap_page`,
  `free_space`, `set_ttbr0`, `flush_asid` (`tlbi aside1is`), `clamp` (each of N values bounded by its max by
  `cmp`/`csel`, then one `csdb`), `mask` (an `and` the compiler cannot see through), `user_readable` /
  `user_writable` (`at` probes), `clean_dcache` / `invalidate_icache` (`ic ialluis`; clean each code page, invalidate once).
- `irq::disable` / `restore` / `wait` / `window`, `gic::enable` / `affinity` / `enable_cpu` / `route` / `unmask` / `unmask_local` / `send_sgi` / `ack` / `eoi`, `mpidr`,
  `timer::arm` / `stop`, `timer::allow_user_counter`.
- `Lock<T>`, a ticket spinlock (it counts the acquisitions that had to wait, `contended`, on the slow path only): `lock()` masks IRQs, then acquires, and its `Guard` releases, then restores DAIF;
  `lock_masked()` skips DAIF, for code entered masked; `Guard::leak` keeps it held until the `unsafe` `Lock::unlock`
  (how trap hooks return holding the board's kernel lock). TPIDR_EL1 holds the core's dense index in bits 48-63 and its per-CPU area's
  signed offset from the `.percpu` template in bits 0-47 (0 on core 0 until `enter_percpu`): `cpu()` is `mrs` + `lsr`,
  `PerCpu<T>::with` (a `RefCell<T>` template static in `.percpu`, its `new` `unsafe`) is `mrs` + `sbfx` + add, IRQs
  masked, reentry panics. `enter_percpu` (core 0; copies the template, sets TPIDR_EL1), `percpu_size`, `mpidr`.

## Boundaries (hard)

- `#![no_std]`, depends only on `mm`. Opts out of `forbid(unsafe_code)` (lints: `docs/DEVELOPMENT.md` settings
  table); no board addresses.
- Every `unsafe` block carries a one-line `// SAFETY:`; every `unsafe fn` has a `# Safety` section stating the
  caller's obligation. Expose safe wrappers where the obligation can be met inside the crate.
- `boot.s` needs `kmain`, `kmain_secondary` (a secondary core's first Rust code: MMU on, own stack, IRQs still masked from PSCI's entry state) and the linker symbols `__stack_top`, `__bss_start`, `__bss_end` from the board. Traps call out
  through four `extern "C"` hooks the board must define: `task_switch`, `board_irq`,
  `board_syscall`, `board_user_fault`, each entered with IRQs masked and returning holding the board's kernel lock;
  the trap exit releases it through a fifth, `board_unlock`, right after `mov sp, x0`, so no other core can run the
  task whose stack this core just left. The `brk #0` self-test runs no hook, so `breakpoint_self_test` is `unsafe`:
  its caller takes the lock first.
- Built only for `aarch64-unknown-none-softfloat`; excluded from `cargo test-host`. Code lives under
  `#[cfg(target_arch = "aarch64")]`.

## Vocabulary

- **Trap frame**: the 288-byte `TrapFrame` the vector asm pushes on the kernel stack; its address *is* the task's
  saved context, which the scheduler stores and returns.
- **Boot table**: the static level-1 `L1` (with `L2` and `L3`, shared by every address space) loaded by `enable_mmu`,
  ASID 0 (`boot_table`).

## Invariants & rules

- A core's vector table is chosen from that core's own registers, never patched, before the core runs EL0 code:
  core 0 runs on the boot table (`install_boot_vectors`, EL0 entries panic) until `report_speculation`, and a
  secondary chooses in `kmain_secondary` before its interrupt controller is up. An unlisted MIDR
  without CSV2_3, ECBHB, CLRBHB or firmware workaround 3 gets the largest k (132), never "not affected"; with v2
  vulnerable no BHB mitigation runs (Linux: "no point mitigating Spectre-BHB alone"). A firmware workaround the
  kernel discovers but does not call yet (1 and 2, phase 11) never reads as mitigated.
- `TrapFrame` layout is fixed by the vector asm (`size_of == 288`, const-asserted); `sp_el0` and `tpidr_el0` are
  touched only by `switch_el0_regs`, needed on every switch with a user thread on either side (each thread has its
  own; `new_user_task` sets both).
- No FP/SIMD state is saved (softfloat target, `docs/DEVELOPMENT.md`).
- Descriptor bits are const-asserted in `mmu.rs` because TCG ignores cacheability attributes, so a wrong bit would
  still boot. User pages are always `PXN` and not global (ASID-tagged); a page is RX or RW, never W+X. No kernel
  mapping is writable and executable (WXN backs it), and each stack the board's `KernelMap` names has an unmapped
  guard page below it. `free_space` skips every level-1 index the boot table fills: those tables are shared.
- `enable_mmu` runs once, on core 0, with the MMU off and before any atomic RMW (exclusives need Normal memory), so
  before any `Lock`. `aarch64_mmu_on` and the secondary entry up to its SCTLR write touch no memory (no load, store or
  atomic: constants by `movz`/`movk`, the table by `adrp`), and its `tlbi vmalle1` is local.
- A core's index is dense, from the DTB (the board's table), not its MPIDR; there is no compile-time core count. A
  secondary's entry copies the template into its area and sets TPIDR_EL1 from `CPU_ON`'s context id, with no MPIDR
  lookup; the template is never written.
- TLB and I-cache maintenance use the inner-shareable forms the hardware broadcasts to every core, so a shootdown
  needs no IPI.
- `Lock` is the only lock: a waiter spins on `ldarh` of the owner ticket, with no `wfe` until measured (hvf may trap
  it). `lock()` masks before acquiring and releases before restoring, so an IRQ never finds its own core holding it.
- `map_page` / `free_space` / `set_ttbr0` require tables owned by one address space and an ASID used by one table;
  flush the ASID after `unmap_page` or before reusing it.
- `irq::disable` / `restore` order memory like lock/unlock (no `nomem`); `State` is `#[must_use]`.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- No host tests. `cargo build` and `cargo clippy` must be clean (they build it for the bare-metal target).
- End to end, `crates/e2e/tests/boot.rs`: `boots_and_powers_off` (vectors, MMU, the `spec:` line),
  `spec_line_matches_the_cpu` (TCG `cortex-a72`, `cortex-a76` and `max` at `-smp 4`: each model's table on every core),
  `el0_before_the_vector_table_is_chosen_panics` (`test=el0-before-spec`: the boot table stops EL0), `unmapped_access_reports_data_abort`,
  `kernel_text_is_read_only_data_never_executes_and_the_boot_stack_has_a_guard` (W^X, the guard page),
  `tasks_alternate_on_yield`, `timer_preempts_spinning_task` (GIC, timer), `faulting_process_is_killed_and_others_keep_running`
  (EL0 faults, ASIDs), `syscall_bench_reports_round_trip`, `lock_bench_reports_round_trips_and_an_exact_count` (`Lock`),
  `every_core_comes_online_runs_a_task_and_takes_a_timer_tick` and every scenario that boots `-smp 4`.
- Hot paths by hand: `cargo run -- -append test=bench` (yield), `test=bench-syscall` and `test=bench-lock`
  (`docs/BENCHMARKS.md`).

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
