# `crates/mm` - arch-independent memory management

## What this crate is

`PhysAddr`, the bitmap `FrameAllocator<WORDS>` for 4 KiB frames, and the per-process `Budget` that charges frames to
an owner. All in `src/lib.rs`.

It is **NOT** page tables or mapping (`crates/arch/src/aarch64/mmu.rs`), the kernel heap (`crates/board/qemu-virt`),
or the policy of who gets how many frames (`crates/kernel`, board `spawn`).

## Responsibilities

- `FrameAllocator`: `new` over a RAM range, `reserve`, `alloc`, `alloc_contiguous`, `free`, `free_count`.
  `alloc` and `alloc_contiguous` start their first-fit scan at `hint`, below which every word is full: `free` lowers
  it, `alloc` moves it to the word it took from (`WORDS` when none is free), `alloc_contiguous` past the full words
  at it. Results are first fit's for every `count >= 1` (`matches_a_bit_by_bit_model`); only the scan over the full
  prefix (the reserved image, then the frames in use) is skipped.
- `Budget`: `alloc` / `alloc_contiguous` / `free` against a `FrameAllocator`; `shrink` / `grow` when frames move
  between parent and child.

## Boundaries (hard)

- `#![cfg_attr(not(test), no_std)]`, no dependencies, workspace `forbid(unsafe_code)`.
- Leaf crate: `kernel`, `dtb`, `arch` and `qemu-virt` all depend on it; it depends on none of them.
- No addresses are dereferenced here; frames are numbers, the board writes to them.

## Invariants & rules

- Capacity is `WORDS * 64` frames; `new` trims RAM to whole frames and ignores frames past capacity. A set bit is in
  use, so `empty()` (all bits set) hands out nothing.
- `free` panics on a frame outside the allocator, misaligned, or not allocated (double free).
- `Budget::alloc` and `alloc_contiguous` charge only on success: over budget or out of frames charges nothing.
- `Budget::charge` counts frames the kernel holds outside the allocator (the network's socket pool) and refuses,
  charging nothing, over budget; `refund` undoes it.
- `Budget::shrink` panics if fewer than `frames` remain (`"budget overdrawn"`); `grow` returns a child's frames.
- `alloc` is a hot path (first non-full word, `trailing_ones`).
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Host: `cargo test --target aarch64-apple-darwin -p mm` (`tests/frames.rs`, `tests/budget.rs`).
- Benchmark: `cargo bench-host` runs `benches/frames.rs`; baseline row in `docs/BENCHMARKS.md`.
- End to end: the `free frames <n> before, <n> after` checks (`assert_no_leak`) in `crates/e2e/tests/boot.rs`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
