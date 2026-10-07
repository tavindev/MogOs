# Phase 10: Observability, debugging, security hardening

Goal: production-grade visibility and a hardened, fuzzed trust boundary. Only the early hardening block below is
written; steps 60-66 (trace tooling, debug handle, crash dumps, audit and revocation, fuzzing CI, signed images,
mitigation completion) follow the survey ([linux-survey.md](../research/linux-survey.md) section 12) and are planned
when the phase starts.

## Phase 10 early: hardening baseline

Security hardening has no step before phase 10, yet it is the largest unpaid syscall cost in the cross-OS comparison:
Linux pays 22.4 ns per syscall for Spectre-BHB here (`docs/BENCHMARKS.md`, debt ledger). These steps are cheap now and
need neither SMP nor the phase 6 memory work, so they run beside phase 5. They touch `crates/arch` (vectors, MMU), the
board's `linker.ld` and user-memory helpers, `kernel`'s handle table and `mogfs`'s block index, so each rebases onto
the phase 5 step in flight. Speed with complete safety is the moat: no mitigation a CPU needs is skipped, and each one
takes its cheapest correct form, measured. Each step's measured cost goes into the ledger row it pays.

Benchmarks are hvf medians of 21 interleaved boots against the step's base commit: syscall (`test=bench-syscall` and
`test=bench-syscalls`), yield (`test=bench`), pipe (`test=bench-pipe`), boot; the cost is pre-declared, then measured,
and "holds" means within noise. The e2e harness runs under TCG, where each `-cpu` model shows its own ID registers;
under hvf the guest gets the model's MIDR and PFR1/ISAR1/MMFR1/MMFR2 but the host's PFR0 (so the M4's CSV2 and CSV3;
QEMU 9.2.1 `target/arm/hvf/hvf.c`). So e2e proves the decision per model, and hvf gives the cost. Boot tests take the
`-cpu` model as an argument (today `crates/e2e/tests/boot.rs` hard-codes `cortex-a72`).

| # | Step | Done when |
| --- | --- | --- |
| 60a | Speculation report and Spectre-BHB | Each core, at boot, reads its own MIDR_EL1, ID_AA64PFR0/PFR1/ISAR1/ISAR2/MMFR1_EL1 and, only if `SMCCC_VERSION` (asked through PSCI_FEATURES) is at least 1.1, `ARCH_FEATURES` for workarounds 1-3 over the DT's PSCI conduit; below 1.1 there are no workarounds (Linux's `arm_smccc_1_1_get_conduit`). It decides as Linux v6.18's `proton-pack.c` does. v2: CSV2, or a MIDR on Linux's v2 safe list, is not affected; else firmware workaround 1; else vulnerable. BHB: CSV2_3 or a MIDR on Linux's BHB safe list (A35, A53, A55, A510, A520, B53, Kryo 2XX-4XX silver) is not affected; with v2 vulnerable nothing is done ("no point mitigating Spectre-BHB alone"); else ECBHB (nothing to run), else CLRBHB (`clearbhb; isb`), else the loop with Linux's k for the MIDR (A72 and A57: 8; A76, A77, N1: 24; up to 132), else firmware workaround 3, else vulnerable. SSB: FEAT_SSBS keeps `SCTLR_EL1.DSSBS` at 0, so every exception entry sets PSTATE.SSBS to 0 for free, and boot runs `msr ssbs, #0` because DSSBS only acts on entry; without SSBS or workaround 2 it is vulnerable. The vectors are static tables built with `.irp`: plain, CLRBHB, firmware, and one loop table per k and barrier (`sb` where ID_AA64ISAR1_EL1.SB, else `dsb nsh; isb`), the count an immediate, so entry does no load. Only the eight lower-EL entries run the mitigation, after `stp x0, x1` and before the first branch: `mov x0, #k`, then the three-instruction loop (`b . + 4; subs x0, x0, #1; b.ne`) runs k times, then the barrier. Each core writes `VBAR_EL1` once. The `spec:` line (v1, v2, BHB, SSB, Meltdown, BSE, and the table's name) is derived from the table read back from `VBAR_EL1`, and reports the worst core. e2e `spec_line_matches_the_cpu`, lines pinned from QEMU 9.2.1 `target/arm/tcg/cpu64.c`: `-cpu cortex-a72` (r0p3, CSV2 0, no firmware) prints v2 vulnerable, BHB not mitigated (v2 vulnerable), SSB vulnerable, plain table; `-cpu cortex-a76` (CSV2 1, SSBS 1, no SB) prints the loop of 24 with `dsb nsh; isb` and SSB mitigated; `-cpu max` (CSV2_3, SSBS2, SB, CSV3) prints BHB not affected, SSB mitigated, plain table. Each boot then runs the syscall and pipe scenarios, and at `-smp 4` every core's read-back table is the same. Benchmarks: syscall and pipe, pre-declared at or a little under Linux's 22 ns per trap (no trampoline hop); yield and boot hold (yield traps from EL1). Recorded in the ledger's BHB row. |
| 60b | Spectre v1: every user-derived index and pointer | Each user-derived value that indexes memory passes a clamp, `cmp` + `sbc` + `and` (Linux's `array_index_mask_nospec`) or `csel`, then `csdb`, from `arch`. Arm's "Cache Speculation Side-channels" v2.5 says only the pair is sufficient "on ALL Arm implementations", so no `csdb`-less mask is used. The list is exhaustive and the reviewer checks it. (1) User buffers: `arch::mask_user(ptr, len)` (`cmp`/`ccmp`/`csel`, then `csdb`) runs once per buffer as `user_bytes` and `user_bytes_mut` build the slice, returning null when the range leaves user space. The `at s1e0r/w` probe stays as the permission check. (2) Handles: `Handles::entry` clamps the index before its load, and `close` reuses that clamped index instead of re-indexing; through it the kernel loads every handle-derived index (pipe, mutex, process slot, inode), and those objects are kernel-written. (3) Syscall dispatch: after the `ENOSYS` check, `nr` is clamped to 0. (4) MogFS: `read` and `write` index `Record::ptrs` from the user's `offset`, and `scan` (`readdir`'s `start`) from the entry number. A mispredicted loop bound runs one more iteration with `pos` at `end` (at most `MAX_FILE_SIZE`), so `ptrs[PTRS]` is reachable speculatively, and clamping `offset` first does not bound it. So `mogfs` stays safe and indexes `ptrs` through a clamp on its `Disk` port (static dispatch, inlined): the board's implementation uses `arch`'s, and host tests use `min`. The archive's `readdir` only counts `start` entries (`skip`), never indexing by it. A new syscall that indexes with a user value joins the list. e2e: every existing scenario passes, and `test=fuzz` stays clean with kernel-address buffers (`EFAULT`) and out-of-range handles (`EBADF`); new: `io_submit_wait` on a file at `offset` at and past its size returns 0 (read), and `readdir` with `start` past the last entry returns 0. Benchmarks: syscall, pipe, and `bench-syscalls`' `open`, `readdir`, `file-read`; pre-declared about 1 ns per buffer or handle. Recorded in the ledger's v1 row. |
| 60c | Kernel W^X | Today the RAM GiB is one level-1 block, EL1 read-write and executable (`l1_block`: no PXN, no read-only bit). Kernel text is writable, and data, heap and every user frame's identity alias are executable at EL1; device memory is already PXN and UXN. In the new map the RAM GiB points at a level-2 table. The 2 MiB at the image base (0x40200000, 2 MiB aligned) points at a level-3 table, with `.text` read-only and executable, `.rodata` read-only and PXN, and data, bss and the stacks RW and PXN. Every stack gets an unmapped 4 KiB guard page below it, boot's and the three secondaries'. The DTB's 2 MiB at RAM base is read-only and PXN, and the rest of RAM is 2 MiB blocks, RW, PXN and UXN. All kernel entries stay global (nG = 0). `linker.ld` aligns each section and guard to 4 KiB, exports their bounds as symbols, and `ASSERT`s that `__kernel_end` stays within the 2 MiB, so a larger image fails to link instead of booting unmapped. `SCTLR_EL1.WXN` is set as defence in depth; no test isolates it, since the descriptor bits already fault. Core 0 builds the tables from the linker symbols before the MMU is on. Every process's level 1 copies its kernel entries from `arch::boot_table()` instead of the `KERNEL_L1` constant. The RAM entry is now a table descriptor (`0b11`), so `free_table` skips the kernel's indexes explicitly; skipping by block type would free the shared kernel tables at every exit. e2e: `test=wx-text` (a store to `kmain`) and `test=wx-exec` (a branch to a `.data` word) each end in the fault dump naming a permission fault at that address; `test=wx-guard` (core 0 recurses past its stack) faults on the guard page. The `spawn`, `kill` and `assert_no_leak` scenarios pass, so teardown leaves the kernel tables alone, and ELF loading still writes through the RW alias, now PXN. Benchmarks: syscall, yield, pipe, boot, spawn; pre-declared 0, with the TLB cost of 4 KiB image pages over one 1 GiB entry measured. Under hvf each guest entry also passes through the host's stage-2 tables, so the measured cost may not match hardware. If the image pages cost more than noise, text and rodata switch to contiguous-bit 64 KiB runs, and that is measured too. Recorded in the ledger's W^X row. |

### Step details

- **60a.** Invariants:
  - A table is chosen per core from that core's own registers and written once, never patched at run time.
  - An unknown MIDR without CSV2_3, ECBHB or CLRBHB gets the largest k (132) and a warning, never "not affected".
  - On a v2-vulnerable CPU the line says BHB is not mitigated, as Linux's does, instead of a loop that does not fix
    v2.

  Avoids Linux's `alternative_cb` code patching and its one system-wide `max_bhb_k`: with no modules, a few static
  tables (about 2 KiB each) cover every case.

  Under hvf the result is correct for the CPU the guest is shown: an A72 r0p3 MIDR with the M4's CSV2, hence v2 not
  affected and the k = 8 loop. A real A72 is phase 11.
- **60b.** Invariants:
  - Every user-derived array index goes through the clamp. The list in the step is exhaustive, and the reviewer checks
    it on every step that adds a syscall or a user-indexed table.
  - Clamped at one choke point behind one `csdb`, bounded by array capacity (changed during implementation, see What
    was done: a `csdb` costs about 9 ns on the M4). `dispatch` clamps every argument that may index kernel memory,
    each with a `csel` to the capacity of what it indexes (never a run-time size such as a file's length), then runs
    one `csdb`, and passes on only the clamped values. An index derived from them later is bounded by that capacity
    by construction, with no `csel`. A value that only appears after `dispatch` and indexes memory keeps its own
    clamp and barrier.

  Avoids Linux's scattered `array_index_nospec` call sites; here the choke points already exist.

  Linux masks user pointers with one `bic` of bit 55, possible because its kernel lives in TTBR1. After phase 6's
  higher-half move, the pointer clamp can become that one instruction and its cost is re-measured.
- **60c.** Invariants:
  - No kernel mapping is both writable and executable.
  - Each kernel stack built at boot has a guard page.
  - User pages are unchanged: RX, or RW and UXN, always PXN.

  Not covered yet: per-task kernel stacks are allocator frames in the RW blocks with no guard, so a deep kernel
  recursion overruns the next frame. Phase 6's higher-half kernel gives them guarded virtual stacks (the ledger's
  VMAP_STACK row).

  Phase 6 rebuilds this map under TTBR1 and keeps the invariants.

## Which variants apply (cortex-a72)

Sources, all read 2026-10-07:
- Arm's Speculative Processor Vulnerability table (developer.arm.com 110280; the December 2023 snapshot, since the live
  page needs JavaScript).
- Arm's Spectre-BHB whitepaper v1.6.
- Arm's Spectre-BSE bulletin (110360, published 2025-07-22; read through a text proxy).
- Linux v6.18: `arch/arm64/kernel/proton-pack.c`, `cpu_errata.c`, `entry.S`, `include/asm/assembler.h`, `uaccess.h`
  and `barrier.h`; `arch/arm64/mm/context.c`.
- QEMU 9.2.1: `target/arm/tcg/cpu64.c`, `target/arm/hvf/hvf.c` and `target/arm/tcg/psci.c`.

| Variant | A72 before r1p0 (QEMU's model is r0p3) | A72 r1p0 and later | Under QEMU here | MogOs |
| --- | --- | --- | --- | --- |
| v1 bounds bypass (CVE-2017-5753) | affected | affected | affected | 60b |
| v2 branch target injection (CVE-2017-5715) | affected | not affected | hvf: not affected (host CSV2); TCG `cortex-a72`: vulnerable (no CSV2, no firmware) | phase 11, on real firmware: `ARCH_WORKAROUND_1` at Linux v6.18's sites: every context switch (`check_and_switch_context`), an EL0 instruction abort or PC fault on a kernel address, an EL0 breakpoint or single-step, an EL0 IRQ with a kernel PC |
| v3 Meltdown (CVE-2017-5754) | not affected | not affected | not affected | no KPTI |
| v3a system register read (CVE-2018-3640) | affected | not affected | as the model | in the sources, the only target is the kernel's location (VBAR_EL1): Linux builds its v3a capability only with KASLR, and its code fix is for KVM's EL2 vectors. So phase 6, with KASLR |
| v4 speculative store bypass (CVE-2018-3639) | affected | affected | hvf `cortex-a72`: no SSBS (the model's PFR1 is 0), no firmware: vulnerable, as Linux reports | 60a: SSBS where present; A72's `ARCH_WORKAROUND_2` (Linux calls it on every kernel entry and exit) in phase 11 |
| Spectre-BHB (CVE-2022-23960) | affected, k = 8 | affected, k = 8 | hvf: the k = 8 loop; TCG `cortex-a72`: not mitigated, since v2 is vulnerable | 60a |
| Spectre-BSE (CVE-2024-10929) | affected | not affected | no firmware: vulnerable (Linux has no BSE handling) | Arm's only mitigation for A72 before r1p0 is firmware `ARCH_WORKAROUND_1` or `_3` (an EL3 MMU off/on); no Arm statement says the BHB loop covers it. So 60a reports it vulnerable without firmware, and phase 11's workaround-1 path covers it. Arm rates practical exploitation "very low" |

QEMU 9.2.1 answers no SMCCC call: under hvf and TCG a non-PSCI HVC returns -1, and PSCI_FEATURES for `SMCCC_VERSION`
is not supported. So SMCCC stays at 1.0 and no firmware workaround exists in either guest. Linux under hvf therefore
reports `spectre_v2: Mitigation: CSV2, BHB` and `spec_store_bypass: Vulnerable`, the strings recorded in the cross-OS
run.

## Decided

- **The minimal mitigation set**, for the CPU each core is shown:
  - BHB per Linux's order (60a)
  - v1 clamps (60b)
  - SSBS when present (60a)
  - W^X with stack guards (60c)

  Firmware workarounds 1-3 are called only when SMCCC 1.1 discovery says they exist and are needed, that is in
  phase 11 on real A72 firmware, where they are priced against Linux. No mitigation is left out because it is slow;
  a costly one is measured and paid.
- **No per-syscall kernel stack offset randomization.** The argument rests on memory safety alone, not on cost.
  - Linux's `RANDOMIZE_KSTACK_OFFSET` makes the kernel stack layout unpredictable across syscalls. That defeats
    attacks built on memory-safety bugs in kernel code: stack buffer overflows, uninitialized-stack reads, and stack
    spraying for use-after-return.
  - In MogOs the logic crates are `forbid(unsafe_code)`, where safe Rust cannot express those bugs.
  - The `unsafe` in `arch` and the board puts no user-sized or uninitialized data on a kernel stack: the trap frame is
    a fixed 288 bytes, and user data moves through frames and checked addresses.
  - With the attack's precondition ruled out, declining gives away no security. The ledger marks it declined, and
    the cross-OS rerun isolates Linux's share with `randomize_kstack_offset=off`.
  - Reopen it if `unsafe` code ever puts user-sized or uninitialized data on a kernel stack.
- **KASLR stays in phase 6**, with the higher-half kernel (survey steps 32 and 38). It carries this question: A72
  before r1p0 leaks VBAR_EL1 through v3a, and Linux forces KPTI with KASLR on CPUs without E0PD
  (`kaslr_requires_kpti`). Phase 6 measures KPTI-style fixed vectors against a KASLR that randomizes only what v3a
  cannot reveal. The mapping seal stays with KASLR there.

## Notes

- Survey numbering ([linux-survey.md](../research/linux-survey.md) section 12) against this doc:
  - 60a and 60b take the "first Spectre baseline" (SMCCC workarounds, index clamping) from survey step 38. That step
    keeps KASLR and the seal in phase 6.
  - 60c takes the W^X kernel map from survey step 32. That step keeps the higher-half move.
  - Survey steps 60-66 keep their numbers. Step 66 (mitigation completion: predictor scrub for untrusted processes,
    side-channel regression tests) builds on 60a.
- What these steps cost and pay is tracked in `docs/BENCHMARKS.md`'s debt ledger, which also judges Linux's other
  hardening defaults. A cross-OS rerun after 60a compares MogOs-with-mitigations against the `Linux` column, not the
  `mitigations=off` one.

## What was done

Filled in as each step lands.

- Step 60a, speculation report and Spectre-BHB: `arch`'s vectors (`trap.rs`) are 16 static tables built by `.irp`
  macros, 2 KiB apart in `spec::TABLES` order: plain, `clrbhb` (`hint #22; isb`), firmware workaround 3 by `hvc` and
  by `smc` (a static table cannot be patched for the conduit, so one per conduit; x2 and x3 go back from their frame
  slots), and the branch loop for each of Linux's k (8, 11, 24, 32, 38, 132) with `dsb nsh; isb` and with `sb`. Only
  entries 8-15 run the mitigation, after `stp x0, x1` and before the first branch, the count an immediate (checked
  in the disassembly: 16 tables, entries 0-7 identical to plain, one shared `.Ltrap`). `arch::install_vectors(conduit)`
  (`spec.rs`) runs on each core: it decides in Linux v6.18's order (`proton-pack.c`, read 2026-10-07) from that
  core's MIDR and ID registers, reading each register and asking the firmware only when the decision reaches it, writes
  `VBAR_EL1` once and runs `msr ssbs, #0` where FEAT_SSBS exists (`SCTLR_EL1.DSSBS` stays 0). Firmware discovery is
  Linux's `arm_smccc_1_1_get_conduit`: PSCI_FEATURES(SMCCC_VERSION), then SMCCC_VERSION at least 1.1, then
  ARCH_FEATURES, over the DT's `/psci` `method` (`Dtb::psci_method`, kept in the board's `CONDUIT` for the
  secondaries). One reconciliation of the step text with its invariant: Linux v6.18 gives an unlisted MIDR k = 0, then
  tries workaround 3, then reports vulnerable; here the order is listed k, then workaround 3, then the largest k (132,
  printed as "unlisted cpu, largest k"), so A73 and A75 (on no k list, and Arm's BSE bulletin names workaround 3 as
  their fix) take the firmware. Workarounds 1 and 2 are discovered but not called until phase 11, so they print as
  "vulnerable (firmware workaround 1 not called)" and "vulnerable"; workaround 2 answering not required prints "not
  affected". Meltdown uses Linux's `kpti_safe_list` and CSV3 (no KPTI exists, so anything else is vulnerable); BSE
  follows Arm bulletin 110360 (A57, A72 before r1p0, A73, A75; mitigated only by the firmware table).
  `arch::record_speculation` stores each core's record (the table read back from `VBAR_EL1`, the v2, BHB, SSB,
  Meltdown and BSE states, packed so the worst core's record is the largest); secondaries record right after they
  install, core 0 in the new `Board::report_speculation`, which `kernel::run` calls after the `boot:` line: it waits
  until every started core has recorded and prints `spec: ... table <name> on <same>/<cores> cores` for the worst
  core. `kmain` now reads the DTB before installing core 0's vectors. e2e `spec_line_matches_the_cpu` (TCG, `-smp 4`,
  then `test=bench-syscall test=pipe`; the harness passes `-cpu cortex-a72` only when a scenario names no `-cpu`):
  `cortex-a72` prints `v2 vulnerable, bhb not mitigated (v2 vulnerable), ssb vulnerable, meltdown not affected, bse
  vulnerable, table plain on 4/4 cores`, `cortex-a76` `v2 not affected, bhb mitigated, ssb mitigated, meltdown not
  affected, bse not affected, table loop24-dsb on 4/4 cores`, `max` `v2 not affected, bhb not affected, ssb
  mitigated, meltdown not affected, bse not affected, table plain on 4/4 cores` (each after `v1 vulnerable`, which
  60b turns); `boots_and_powers_off` pins the `cortex-a72` line at one core. The `clrbhb` and firmware tables are
  chosen by no QEMU model, so the disassembly is their only check. Under hvf the guest shows MIDR `0x410fd083` (A72
  r0p3) and the M4's `ID_AA64PFR0_EL1` `0x1101000010110011` (CSV2 1, CSV3 1), with PFR1, ISAR1, ISAR2 and MMFR1 0,
  so it prints `v2 not affected, bhb mitigated, ssb vulnerable, meltdown not affected, bse vulnerable, table
  loop8-dsb`. Benchmarks (hvf, base `b08f7fd`, 63 interleaved boots per core count, load 6 to 12; median/min
  before -> after; the hvf numbers are the A72 k = 8 loop with `dsb nsh; isb` run on the M4, not an A72's cost):
  `-smp 1` syscall 32/31 -> 45/44 ns, every `bench-syscalls` call about +13 ns (`enosys` 35.1/33.8 -> 48.4/46.5,
  `open` 84.3/81.0 -> 102.5/94.9, `readdir` 90.8/87.3 -> 106.1/101.6, `file-read` 75.5/71.9 -> 89.1/85.4), pipe
  357/352 -> 437/430 ns (six traps per round trip), yield 80/78 -> 80/78 ns (EL1 traps run no loop), boot 213/192 ->
  222/201 us; `-smp 4` syscall 32/30 -> 45/44, pipe 358/351 -> 436/428, yield 80/77 -> 80/77, boot 237/212 -> 242/209.
  Pre-declared at or a little under Linux's 22.4 ns per trap: measured 13 ns. Boot: core 0's install costs 5.2 us cold
  (3.6 us warm), of which 3.5 us are the five ID-register reads the decision needs on this CPU (PFR0, MMFR1, ISAR2,
  ISAR1, PFR1), each an hvf trap of about 0.7 us; the first cut also asked the firmware on the boot path (one
  PSCI_FEATURES `hvc`, 5.5 us) and measured +16 us, so the report-only work (SSB's workaround 2, the record) moved
  after the `boot:` line. TCG instructions (`-icount shift=0`, exact): `cortex-a76` syscall 218 -> 293 (+75: `mov`,
  24 x 3, `dsb`, `isb`), pipe 2480 -> 2930 (six traps); `cortex-a72` picks the plain table, so 218 -> 218; the hvf
  k = 8 table runs 27 more instructions per EL0 trap.
- Step 60b, Spectre v1: built first as the step says, a clamp where each index is used (`arch::clamp`, `cmp`/`csel`/
  `csdb`, through a `Clamp` port on `Handles` and `Disk`, and `arch::mask_user` in `UserIn`/`UserOut`; commit
  "Step 60b (per-site clamps)"). Measured against 60a (hvf, 63 boots, load about 10): `enosys` +11 ns, a null console
  write +23, a pipe write +34, a file read +43, the pipe round trip +191, about 10 ns per `csdb`, not the 1 ns
  pre-declared. A `csdb` alone costs 8.8 ns on the M4 (a host loop of `cmp; csel; csdb` against 0.3 ns without it),
  so the cost is the barrier, which hvf runs natively; an A72's is unknown until phase 11. The orchestrating session
  then approved the cheaper form, one barrier per syscall, and the invariant changed (above). `dispatch` (generic over the
  kernel's `Clamp` port, the board's `Nospec` over `arch::clamp`: one `cmp`/`csel` per value, then one `csdb`) now
  clamps, before the jump on the number, 13 values: the number (below 26), the handle indexes in x0 and x3 (below
  16), the user buffers at x1, x2, x4 and x5 with the lengths after them (each pointer's offset in user space below
  its room, `2^39 - 2^32 - (len & 0x1fff) + 1`, so a clamped buffer ends in user space or, for a length a mispredicted
  check let through, at most 4 KiB past its top, where nothing translates; each length below `MAX_BUFFER + 1`), the
  file offset in x4 (below `MAX_FILE_SIZE + 2`) and the `readdir` start in x3 (below `MAX_FILE_SIZE + 1`). Each limit
  keeps every architecturally valid value unchanged, so the architectural checks decide the result as before; only
  the clamped values reach `Call`. Handle lookups take a `Handle` (value and clamped index); `split` (`spawn`'s
  handle list, read from user memory after `dispatch`) clamps each with `Handle::new` and its own barrier. MogFS indexes
  a record's block pointers `% PTRS` in `read`, `write_data`, `write`'s block count and `scan`: in bounds by
  construction on a mispredicted loop bound, which runs one block past the end (`Disk` has no clamp). The socket calls
  (20-25) take x0's clamped handle, and a receive or send the x2/x3 buffer. MogFS v2 (`crates/mogfs2`) is not wired
  into the kernel yet; it joins the list when it is. `fsbench` now checks that reads at and past a file's end and a
  `readdir` from past the last entry return 0 (green before and after: regression guards), and both pinned `spec:`
  lines read `v1 mitigated`. Benchmarks (hvf, base: main `22c07bc` with 60a, 63 interleaved boots, load 50 to 78, so
  min is the steadier number; median/min before -> after): `-smp 1` syscall 61/45 -> 78/60 ns, `mutex` (no index
  but the number) 60.1/52.7 -> 75.7/64.9, `dup` 61.0/51.3 -> 75.1/65.4, `close` 58.8/49.9 -> 74.6/62.8, pipe write
  84.5/74.5 -> 101.6/87.9, file read 106.7/90.6 -> 120.3/103.3, `open` 116.7/101.2 -> 136.4/115.6, `readdir`
  122.4/104.6 -> 141.4/120.9, `enosys` (rejected before the batch) 55.6/46.7 -> 55.2/47.3, yield and boot hold;
  `-smp 4` syscall 61/48 -> 77/60, pipe write 93.4/75.8 -> 113.0/95.5, file read 110.6/90.7 -> 126.9/109.7, pipe
  round trip 605/482 -> 708/544. So about 13-17 ns per syscall, one `csdb` and the batch's 13 selects, against
  23-43 ns for the per-site form. TCG instructions (`-icount`, `cortex-a72`): 84-98 per syscall (the batch), `enosys`
  +2, yield 0. At `opt-level = 1` the generic `dispatch` is instantiated in the board crate, so the handle lookups and
  `path`/`socket` are `#[inline(always)]` (without it each returned its `Object` through `memcpy`, 200 more
  instructions for `dup` and `close`).
