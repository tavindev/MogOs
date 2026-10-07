# `crates/arch` - AArch64 primitives behind safe wrappers

## What this crate is

The only architecture-specific crate: boot entry (`src/aarch64/boot.s`), exception vectors and `TrapFrame`
(`trap.rs`), MMU and page tables (`mmu.rs`), GICv2 (`gic.rs`), the virtual timer (`timer.rs`), IRQ masking
(`irq.rs`), `uptime_us` (`mod.rs`).

It is **NOT** board-specific: no MMIO addresses, no memory map, no drivers, no scheduling policy (those are
`crates/board/qemu-virt` and `crates/kernel`).

## Responsibilities

- `install_vectors`, the vector asm and `aarch64_exception` routing: IRQ, EL0 `svc`, EL0 fault, EL1 `svc #0` (yield),
  EL1 `brk #0` (self-test); anything else panics with ESR/FAR/ELR.
- `new_task`, `new_user_task`, `switch_el0_regs`, `TrapFrame::restart`.
- Descriptor encoding (`l1_block`, `user_page`), `enable_mmu`, `map_page`, `unmap_page`, `free_space`, `set_ttbr0`,
  `flush_asid`, `user_readable` / `user_writable` (`at` probes), `clean_dcache` / `invalidate_icache` (clean each code page, invalidate once).
- `irq::disable` / `restore` / `wait`, `gic::enable` / `ack` / `eoi`, `timer::arm`, `timer::allow_user_counter`.

## Boundaries (hard)

- `#![no_std]`, depends only on `mm`. Opts out of `forbid(unsafe_code)` (lints: `docs/DEVELOPMENT.md` settings
  table); no board addresses.
- Every `unsafe` block carries a one-line `// SAFETY:`; every `unsafe fn` has a `# Safety` section stating the
  caller's obligation. Expose safe wrappers where the obligation can be met inside the crate.
- `boot.s` needs `kmain` and the linker symbols `__stack_top`, `__bss_start`, `__bss_end` from the board. Traps call out
  through four `extern "C"` symbols the board must define: `task_switch`, `board_irq`,
  `board_syscall`, `board_user_fault`, each entered with IRQs masked.
- Built only for `aarch64-unknown-none-softfloat`; excluded from `cargo test-host`. Code lives under
  `#[cfg(target_arch = "aarch64")]`.

## Vocabulary

- **Trap frame**: the 288-byte `TrapFrame` the vector asm pushes on the kernel stack; its address *is* the task's
  saved context, which the scheduler stores and returns.
- **Boot table**: the static level-1 `L1` loaded by `enable_mmu`, ASID 0 (`boot_table`).

## Invariants & rules

- `TrapFrame` layout is fixed by the vector asm (`size_of == 288`, const-asserted); `sp_el0` and `tpidr_el0` are
  touched only by `switch_el0_regs`, needed only when the address space changes.
- No FP/SIMD state is saved (softfloat target, `docs/DEVELOPMENT.md`).
- Descriptor bits are const-asserted in `mmu.rs` because TCG ignores cacheability attributes, so a wrong bit would
  still boot. User pages are always `PXN` and not global (ASID-tagged); a page is RX or RW, never W+X.
- `enable_mmu` runs with the MMU off and before any atomic RMW (exclusives need Normal memory).
- `map_page` / `free_space` / `set_ttbr0` require tables owned by one address space and an ASID used by one table;
  flush the ASID after `unmap_page` or before reusing it.
- `irq::disable` / `restore` order memory like lock/unlock (no `nomem`); `State` is `#[must_use]`.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- No host tests. `cargo build` and `cargo clippy` must be clean (they build it for the bare-metal target).
- End to end, `crates/e2e/tests/boot.rs`: `boots_and_powers_off` (vectors, MMU), `unmapped_access_reports_data_abort`,
  `tasks_alternate_on_yield`, `timer_preempts_spinning_task` (GIC, timer), `faulting_process_is_killed_and_others_keep_running`
  (EL0 faults, ASIDs), `syscall_bench_reports_round_trip`.
- Hot paths by hand: `cargo run -- -append test=bench` (yield) and `test=bench-syscall` (`docs/BENCHMARKS.md`).

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
