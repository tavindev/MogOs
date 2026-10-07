# `crates/dtb` - minimal FDT parser

## What this crate is

Reads the flattened device tree QEMU passes: `total_size`, then `Dtb::memory`, `gic` (GICv2), `cpus` (the count of second-level nodes, the `/cpus` children, with `device_type = "cpu"`),
`bootargs` (`/chosen`) and `psci_method` (`/psci`'s `method`: `hvc` or `smc`). All in `src/lib.rs`. It is **NOT** a general device-tree library: no writing, no phandles, no
nested-bus address translation.

## Boundaries (hard)

- `#![cfg_attr(not(test), no_std)]`, depends only on `mm` (`PhysAddr`), workspace `forbid(unsafe_code)`.
- Works on a `&[u8]`; the board (`kmain` in `crates/board/qemu-virt`) builds that slice from RAM base.
- Callers: `qemu-virt` (`gic`, `cpus`, `psci_method`) and `kernel::run` (`memory`, `bootargs`).

## Invariants & rules

- Slice reads go through `get` / `be32` and return `None` when out of bounds, never index past the blob.
- `total_size` checks the magic before the board trusts the size.
- Lookups match top-level nodes only (`depth == 2`) and decode `reg` with the root's `#address-cells` /
  `#size-cells` (spec defaults 2 and 1).
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Host: `cargo test --target aarch64-apple-darwin -p dtb` (`tests/virt.rs`: bad and truncated headers).
- The real QEMU blob: e2e `boots_and_powers_off` asserts the `ram: 0x40000000..0x48000000` line; every `test=*`
  scenario depends on `bootargs`.

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
