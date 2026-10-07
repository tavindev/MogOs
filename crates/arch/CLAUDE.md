# `crates/arch` - AArch64 primitives behind safe wrappers

## What this crate is

The only architecture-specific crate: boot and secondary-core entry (`src/aarch64/boot.s`), exception vectors and `TrapFrame`
(`trap.rs`), MMU and page tables (`mmu.rs`), GICv2 (`gic.rs`), the virtual timer (`timer.rs`), IRQ masking
(`irq.rs`), the only lock and per-CPU primitives (`lock.rs`), `uptime_us` (`mod.rs`).

It is **NOT** board-specific: no MMIO addresses, no memory map, no drivers, no scheduling policy (those are
`crates/board/qemu-virt` and `crates/kernel`).

## Responsibilities

- `install_vectors`, the vector asm and `aarch64_exception` routing: IRQ, EL0 `svc`, EL0 fault, EL1 `svc #0` (yield),
  EL1 `brk #0` (self-test); anything else panics with ESR/FAR/ELR.
- `new_task`, `new_user_task`, `switch_el0_regs`, `TrapFrame::restart`.
- Descriptor encoding (`l1_block`, `user_page`), `enable_mmu` (core 0 fills the boot table, then runs `aarch64_mmu_on`),
  `secondary_entry` (PSCI `CPU_ON`'s entry: `aarch64_mmu_on` on the same table, the stack top from the context id),
  `map_page` (`None` on a level-1 or level-2 block on the way, never writing a table into kernel memory), `unmap_page`, `free_space`, `set_ttbr0`, `flush_asid` (`tlbi aside1is`), `user_readable` /
  `user_writable` (`at` probes), `clean_dcache` / `invalidate_icache` (`ic ialluis`; clean each code page, invalidate once).
- `irq::disable` / `restore` / `wait`, `gic::enable` / `enable_cpu` / `route` / `unmask` / `send_sgi` / `ack` / `eoi`,
  `timer::arm` / `stop`, `timer::allow_user_counter`.
- `Lock<T>`, a ticket spinlock: `lock()` masks IRQs, then acquires, and its `Guard` releases, then restores DAIF;
  `lock_masked()` skips DAIF, for code entered masked; `Guard::leak` keeps it held until the `unsafe` `Lock::unlock`
  (how trap hooks return holding the board's kernel lock). `cpu()` (TPIDR_EL1: 0 from `_start`, MPIDR Aff0 from `aarch64_secondary`), `MAX_CPUS` (4),
  `PerCpu<T>` (one `RefCell<T>` per core, reached through `with` with IRQs masked; reentry panics).

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
- **Boot table**: the static level-1 `L1` loaded by `enable_mmu`, ASID 0 (`boot_table`).

## Invariants & rules

- `TrapFrame` layout is fixed by the vector asm (`size_of == 288`, const-asserted); `sp_el0` and `tpidr_el0` are
  touched only by `switch_el0_regs`, needed on every switch with a user thread on either side (each thread has its
  own; `new_user_task` sets both).
- No FP/SIMD state is saved (softfloat target, `docs/DEVELOPMENT.md`).
- Descriptor bits are const-asserted in `mmu.rs` because TCG ignores cacheability attributes, so a wrong bit would
  still boot. User pages are always `PXN` and not global (ASID-tagged); a page is RX or RW, never W+X.
- `enable_mmu` runs once, on core 0, with the MMU off and before any atomic RMW (exclusives need Normal memory), so
  before any `Lock`. `aarch64_mmu_on` and the secondary entry up to its SCTLR write touch no memory (no load, store or
  atomic: constants by `movz`/`movk`, the table by `adrp`), and its `tlbi vmalle1` is local.
- A core's index is its MPIDR Aff0 (one cluster); the board starts only cores below `MAX_CPUS`, which `PerCpu` indexes.
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
- End to end, `crates/e2e/tests/boot.rs`: `boots_and_powers_off` (vectors, MMU), `unmapped_access_reports_data_abort`,
  `tasks_alternate_on_yield`, `timer_preempts_spinning_task` (GIC, timer), `faulting_process_is_killed_and_others_keep_running`
  (EL0 faults, ASIDs), `syscall_bench_reports_round_trip`, `lock_bench_reports_round_trips_and_an_exact_count` (`Lock`),
  `every_core_comes_online_runs_a_task_and_takes_a_timer_tick` and every scenario that boots `-smp 4`.
- Hot paths by hand: `cargo run -- -append test=bench` (yield), `test=bench-syscall` and `test=bench-lock`
  (`docs/BENCHMARKS.md`).

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
