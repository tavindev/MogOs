# Phase 1: Foundations

Status: done. All done-when criteria pass in `cargo test-host`.

Goal: the kernel knows its hardware, survives faults, and manages memory.

## Steps

| # | Step | Done when |
| --- | --- | --- |
| 1 | Boot + console | `_start` sets up the stack and BSS, PL011 UART prints, PSCI powers off. |
| 2 | Exception vectors | A `brk #0` is caught and resumed on every boot (`exceptions: ok`); unexpected faults print type, `ESR`/`FAR`/`ELR` and stop instead of hanging. |
| 3 | Device tree (DTB) | RAM range and UART address are read from the DTB QEMU provides, not hardcoded. |
| 4 | Physical page allocator | 4 KiB frames allocated and freed from usable RAM (minus kernel image and DTB); host unit tests. |
| 5 | Virtual memory (MMU) | Identity-mapped page tables (device memory + RAM), MMU and caches on; an unmapped access takes a level-1 translation fault with the right `FAR`. Higher-half kernel is deferred to phase 3. |
| 6 | Kernel heap | `#[global_allocator]` on top of the page allocator; `alloc::vec::Vec` works in the kernel. |

## Design rules

- Pure logic (DTB parsing, frame allocator) lives in safe crates and is unit-tested on the host. Page-table encoding lives with the arch MMU code with build-time `const` asserts on the descriptor bits (TCG ignores memory attributes); the e2e tests cover the mapping itself.
- Register access, assembly, and raw memory live only in the arch/board crates behind small safe APIs.
- Physical and virtual addresses are distinct newtypes (`PhysAddr`, `VirtAddr`), never bare `usize`.

## What was done

Filled in as each step lands.

- Step 1: boot assembly, PL011 UART driver, PSCI `SYSTEM_OFF`, `kernel` / `qemu-virt` crate split with `unsafe_code = "forbid"` by default.
- Tests: `cargo test-host` runs host tests and `crates/e2e/tests/boot.rs`, which boots QEMU and asserts every boot line plus a clean exit.
- Step 2: `crates/arch` (`src/aarch64/trap.rs`) holds the 2 KiB-aligned vector table (16 x 0x80 entries into one save/restore path, 272-byte `TrapFrame`), `install_vectors` and `breakpoint_self_test`. A `brk #0` at current-EL/SPx (EC 0x3C, immediate 0; compiler-emitted `brk #0x1` traps still panic) is skipped (`ELR += 4`), so returning from `breakpoint_self_test` is the proof and boot prints `exceptions: ok`. Anything else panics with kind, source, `ESR`/`FAR`/`ELR`; the board panic handler prints and powers off. `_start` now sets `CPACR_EL1.FPEN`, because Rust for this target emits FP/SIMD, which traps by default.
- Step 3 (memory half): `crates/dtb` parses the FDT header and the root `#address-cells`/`#size-cells` and memory `reg`; the e2e boot test checks the result; a host test covers header rejection. DTB location (QEMU 9.2.1, ELF `-kernel`): x0 is 0 at entry, and QEMU places the DTB at RAM base `0x4000_0000` only if its 1 MiB blob fits below the image's lowest address. At the old `0x4008_0000` it silently skipped it, so the image now loads at `0x4020_0000`. UART address from the DTB is done below.
- Step 4: `crates/mm` has `PhysAddr` and a const-capacity bitmap `FrameAllocator` (kernel uses 512 words = 128 MiB, no heap). The kernel image (`__kernel_start`..`__kernel_end`, stack included) and the DTB are reserved; boot prints the free frame count.
- Step 3 (UART half): `Dtb::uart` returns the `reg` base of the first top-level node whose `compatible` list contains `arm,pl011`; `kmain` parses the DTB once, builds the console from it and passes the `Dtb` to `kernel::run`. The panic handler keeps the constant `0x0900_0000` so it works even when the DTB is missing or bad (no output happens before the DTB parse otherwise).
- Step 5: `arch::l1_block` (`src/aarch64/mmu.rs`) encodes 1 GiB level-1 block descriptors (AF set; Device gets PXN|UXN, Normal is inner shareable) and `arch::MAIR` (index 0 Device-nGnRE, 1 Normal write-back). `arch::enable_mmu` copies the entries into a 4 KiB-aligned static table and programs MAIR, TCR (T0SZ 25, 4 KiB granule, write-back inner-shareable walks, EPD1, IPS from `ID_AA64MMFR0_EL1`) and TTBR0, then `tlbi vmalle1`, barriers, and SCTLR M|C|I. The board maps GiB 0 as device and GiB 1 as RAM; the MMU goes on right after the exception self-test, before the frame allocator is built, so the rest of init runs with caches on; boot prints `mmu: on`. `-append` does reach `/chosen/bootargs` for an ELF kernel (checked with `dumpdtb`); with `test=mmu-fault` the kernel reads `0x8000_0000` and the e2e test asserts the report (ESR EC 0x25, level-1 translation fault, `FAR_EL1=0x80000000`) and a clean exit.
- Step 6: the kernel takes 256 contiguous frames (1 MiB) with `FrameAllocator::alloc_contiguous` after the MMU is on (the allocator's spin lock uses exclusives, which need Normal memory) and hands them to the board's `linked_list_allocator::LockedHeap` `#[global_allocator]` (a second `init_heap` panics). Boot asserts the sum of a 1000-element `Vec` and prints `heap: ok`.
- Benchmarks: `cargo bench-host` runs `crates/mm/benches/frames.rs`; every boot prints `boot: <N> us` (kmain entry to end of init). Baselines and the hvf result are in [BENCHMARKS.md](../BENCHMARKS.md).
- Layout: `crates/arch` is the only arch-specific crate (`src/aarch64/`: `boot.s` with `_start`, `trap.rs`, `mmu.rs` with the descriptor encoding moved out of `mm`); the board lives in `crates/board/qemu-virt`. The workspace names the top-level crates and globs `crates/board/*`, because `crates/*` would also match the manifest-less `crates/board`.
