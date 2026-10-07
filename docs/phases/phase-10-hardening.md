# Phase 10: Observability, debugging, security hardening

Goal: production-grade visibility and a hardened, fuzzed trust boundary. Only the early hardening block below is
written; steps 60-66 (trace tooling, debug handle, crash dumps, audit and revocation, fuzzing CI, signed images,
mitigation completion) follow the survey ([linux-survey.md](../research/linux-survey.md) section 12) and are planned
when the phase starts.

## Phase 10 early: hardening baseline

Security hardening has no step before phase 10, yet it is the largest unpaid syscall cost in the cross-OS comparison:
Linux pays 22.4 ns per syscall for Spectre-BHB here (`docs/BENCHMARKS.md`, debt ledger). These steps are cheap now and
need neither SMP nor the phase 6 memory work, so they run beside phase 5. They touch `crates/arch` (vectors, MMU) and
the board's `linker.ld`, so each rebases onto the phase 5 step in flight there. Speed with complete safety is the
moat: no mitigation a CPU needs is skipped, and each one takes its cheapest correct form, measured. Each step's measured
cost goes into the ledger row it pays.

Benchmarks are hvf medians of 21 interleaved boots against the step's base commit: syscall (`test=bench-syscall` and
`test=bench-syscalls`), yield (`test=bench`), pipe (`test=bench-pipe`), boot. The cost is pre-declared and then measured;
"holds" means within noise. The e2e harness runs under TCG, whose `cortex-a72` is the model's own registers (no CSV2),
while hvf passes the host's ID_AA64PFR0_EL1 to the guest. So e2e checks the detection on several CPU models, and hvf
gives the cost.

| # | Step | Done when |
| --- | --- | --- |
| 60a | Speculation report and Spectre-BHB | At boot `arch` reads MIDR_EL1, ID_AA64PFR0/PFR1/MMFR1_EL1 and asks firmware through SMCCC (`SMCCC_VERSION` via PSCI_FEATURES, then `ARCH_FEATURES` for workarounds 1-3, over the DT's PSCI conduit). It decides each variant the way Linux's `proton-pack.c` does, with one deliberate difference: Linux skips the BHB loop when v2 is vulnerable ("no point mitigating Spectre-BHB alone"), while MogOs still applies it, since Arm's Spectre-BHB whitepaper says the loop works on A72 before r1p0. It then picks the vector table and prints one `spec:` line (v1, v2, BHB, SSB, Meltdown). BHB: CSV2_3 or ECBHB means none needed; otherwise the loop with k from Linux's MIDR lists (A72 and A57: 8); otherwise firmware `ARCH_WORKAROUND_3`; otherwise the largest k in Linux's lists, flagged as an unlisted CPU. The loop (`mov`, then k times `b . + 4; subs; b.ne`, then `dsb nsh; isb`) sits in the lower-EL entries (8-15) of a second vector table, after `stp x0, x1` and before the first branch. Boot points `VBAR_EL1` at the table it needs, so entries from EL1 and unaffected CPUs run no extra instruction. SSB: with FEAT_SSBS, `SCTLR_EL1.DSSBS` is left 0, so the kernel runs mitigated from every entry for free; without it and without `ARCH_WORKAROUND_2`, the line says vulnerable, as Linux does. e2e `spec_line_matches_the_cpu`: TCG `-cpu cortex-a72` prints BHB loop 8 and v2 vulnerable (no CSV2, no firmware), `-cpu cortex-a76` prints loop 24, and `-cpu max` prints whatever its ID registers state (check them in the step); each boot then runs the syscall and pipe scenarios on the selected table. Benchmarks: syscall and pipe with the pre-declared cost of about Linux's 22 ns per trap; yield and boot hold (yield traps from EL1). The measured cost goes into the ledger's BHB row. |
| 60b | Spectre v1: user pointers and handle indexes | The `at s1e0r/w` probe returns the address it checked, masked to 0 by `csel` and then `csdb` when the check fails, and every user access uses the returned address, so a mispredicted check cannot load kernel memory. The handle-table lookup clamps the user's index the same way (`cmp`, `csel`, `csdb`) before its load. `csdb` needs `arch`, and the lookup lives in the safe `kernel` crate, so it gets the clamp from an inlined port call (static dispatch, no indirect branch). The syscall number reaches a `match`, whose jump table is clamped too. e2e: every existing scenario passes, and `test=fuzz` stays clean, including kernel-address and out-of-range-handle arguments (`EFAULT`, `EBADF`). The step lists each user-controlled index the kernel loads through, and the reviewer checks it. Benchmarks: syscall, pipe, `bench-syscalls`' `open` and `readdir`; pre-declared about 1 ns per user buffer or handle. Measured into the ledger's v1 row. |
| 60c | Kernel W^X | Today the RAM GiB is one level-1 block, EL1 read-write and executable (`l1_block`: no PXN, no read-only bit), so kernel text is writable, and data, heap and every user frame's identity alias are executable at EL1. Device memory is already PXN and UXN. New map: the RAM GiB points at a level-2 table, the 2 MiB at the image base (0x40200000, 2 MiB aligned; text, rodata, data, bss and the four stacks take about 0.85 MiB in a debug build) at a level-3 table, and the rest of RAM uses 2 MiB blocks, RW, PXN and UXN. Inside the image, `.text` is read-only and executable, `.rodata` read-only with PXN, data, bss and stacks RW with PXN; `linker.ld` aligns each to 4 KiB. `SCTLR_EL1.WXN` is set, so the hardware refuses any writable mapping as executable at EL1. Every process's level 1 points at the shared kernel table, which is a table descriptor (`0b11`), not a block. `free_table` skipped kernel entries by their block type, so it now skips the kernel's index explicitly, or every process exit would free the kernel tables. e2e: `test=wx-text` (a store to `kmain`'s address) and `test=wx-exec` (a branch to a `.data` word) each end in the fault dump naming a permission fault at that address; `spawn`, `kill` and `assert_no_leak` scenarios pass, proving teardown leaves the kernel tables alone; ELF loading still writes through the RW alias, which is now PXN. Benchmarks: syscall, yield, pipe, boot, spawn; pre-declared 0, with the TLB cost of the 4 KiB image pages over today's single 1 GiB entry measured and recorded. |

### Step details

- **60a.** Invariants: the vector table is chosen once per core at boot, never patched at run time; the decision is the
  same on every core (`-smp 4` boots check that); an unknown CPU gets the stronger choice, never "not affected". Avoids:
  Linux's per-CPU vector indirection and code patching (`alternative_cb`). With fixed tables and no modules, two
  static vector tables cover every case. Under hvf the result is correct for the CPU the guest is shown: an A72 MIDR
  (r0p3) with the M4's CSV2. Correctness on a real A72 is checked in phase 11.
- **60b.** Invariants: one probe helper and one handle lookup remain the only ways user input reaches a kernel load.
  A new syscall that indexes a table with a user value goes through the clamp. Avoids: Linux's audit of scattered
  `array_index_nospec` call sites; here the choke points already exist.
- **60c.** Invariants: no kernel mapping is ever both writable and executable (WXN enforces it); user pages are
  unchanged (RX or RW+UXN, always PXN). Depends on nothing in phase 6. The higher-half move (phase 6) rebuilds this map
  under TTBR1 and keeps the invariant.

## Which variants apply (cortex-a72)

From Arm's Speculative Processor Vulnerability table (developer.arm.com, document 110280; read from the December 2023
snapshot, since the live page needs JavaScript), Linux master's `arch/arm64/kernel/proton-pack.c` and `cpufeature.c`,
and QEMU 9.2.1's `target/arm/hvf/hvf.c` and `target/arm/tcg/psci.c`, all read 2026-10-07:

| Variant | A72 before r1p0 (QEMU's model is r0p3) | A72 r1p0 and later | Under QEMU here | MogOs |
| --- | --- | --- | --- | --- |
| v1 bounds bypass (CVE-2017-5753) | affected | affected | affected | 60b: pointer and index masking |
| v2 branch target injection (CVE-2017-5715) | affected | not affected | hvf shows the host's CSV2, so not affected; TCG `cortex-a72` has no CSV2 and no firmware, so vulnerable | nothing to pay under hvf; the `ARCH_WORKAROUND_1` call at Linux's triggers (an EL0 instruction abort or PC fault on a kernel address, an EL0 IRQ with a kernel PC) in phase 11, on real firmware |
| v3 Meltdown (CVE-2017-5754) | not affected | not affected | not affected | no KPTI |
| v3a system register read (CVE-2018-3640) | affected | not affected | as the model | leaks VBAR_EL1 and other system registers, which matters only once KASLR exists: phase 6 |
| v4 speculative store bypass (CVE-2018-3639) | affected | affected | no SSBS (the model's PFR1 is 0, and the M4 lacks FEAT_SSBS), no firmware: vulnerable, as Linux reports | 60a: SSBS where present; A72's `ARCH_WORKAROUND_2` in phase 11 |
| Spectre-BHB (CVE-2022-23960) | affected, loop k = 8 | affected, loop k = 8 | affected (no CSV2_3, no ECBHB) | 60a: the loop |
| Spectre-BSE (CVE-2024-10929) | affected (Arm's CVE record) | not listed | as the model | 60a reads Arm's Spectre-BSE page and records whether the BHB loop covers it |

QEMU answers no SMCCC call: under both hvf and TCG, an HVC that is not PSCI returns -1, and `PSCI_FEATURES` for
`SMCCC_VERSION` is not supported. So firmware workarounds 1-3 do not exist in either guest. Linux under hvf therefore
reports `spectre_v2: Mitigation: CSV2, BHB` and `spec_store_bypass: Vulnerable`, the strings recorded in the cross-OS
run.

## Decided

- **The minimal mitigation set**, for the CPU the kernel is shown:
  - BHB loop (60a)
  - v1 masking (60b)
  - SSBS when present (60a)
  - W^X (60c)

  Firmware workarounds are called only when discovery says they exist and are needed, which means phase 11 on real
  A72 firmware. There they are priced against Linux, which issues `ARCH_WORKAROUND_2` on every kernel entry and exit.
  No mitigation is left out because it is slow; a costly one is measured and paid.
- **No per-syscall kernel stack offset randomization.** Linux's `RANDOMIZE_KSTACK_OFFSET` moves the stack pointer by
  up to about 6 bits of entropy per syscall, from `get_random_u16()` on 6.18 arm64. A per-CPU PRNG in 2026 master
  replaced it as "too costly for the level of protection".
  - What it hardens: attacks that need a predictable kernel stack layout across syscalls. These are stack buffer
    overflows, uninitialized-stack leaks, and stack spraying for use-after-return.
  - Why MogOs has none of them: each is a memory-safety bug in kernel code. The logic crates are
    `forbid(unsafe_code)`, so safe Rust cannot overflow a buffer or read an uninitialized stack slot. The `unsafe`
    left in `arch` and the board puts no user-sized data on a kernel stack: the trap frame is a fixed 288 bytes, and
    user data moves through frames and probed addresses.
  - It is a probabilistic blur, not a fix, and it costs on every syscall.
  - Its own threat model is ruled out, so declining it gives away no security. The ledger marks it declined, not
    paid, and the cross-OS rerun isolates Linux's share with `randomize_kstack_offset=off`, so the margin is labelled
    by design.
  - Reopen it if `unsafe` code ever puts user-sized or uninitialized data on a kernel stack.
- **KASLR stays in phase 6**, with the higher-half kernel (survey steps 32 and 38). It carries two questions from
  here:
  - A72 before r1p0 leaks VBAR_EL1 through v3a, and prefetch timing can find kernel mappings. Linux forces KPTI with
    KASLR on any CPU without E0PD (`kaslr_requires_kpti`). Phase 6 measures KPTI's cost against fixed-address vectors
    and a KASLR that randomizes only what v3a cannot reveal.
  - The mapping seal, which stays with KASLR there.

## Notes

- Survey numbering ([linux-survey.md](../research/linux-survey.md) section 12) against this doc:
  - 60a and 60b take the "first Spectre baseline" (SMCCC workarounds, index clamping) from survey step 38. That step
    keeps KASLR and the seal in phase 6.
  - 60c takes the W^X kernel map from survey step 32. That step keeps the higher-half move.
  - Survey steps 60-66 keep their numbers. Step 66 (mitigation completion: predictor scrub for untrusted processes,
    side-channel regression tests) builds on 60a.
- What these steps cost and pay is tracked in `docs/BENCHMARKS.md`'s debt ledger. A cross-OS rerun after 60a compares
  MogOs-with-mitigations against the `Linux` column, not the `mitigations=off` one.

## What was done

Filled in as each step lands.
