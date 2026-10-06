# Development

## Toolchain

- Pinned to Rust `1.99.0` in `rust-toolchain.toml` with target `aarch64-unknown-none-softfloat` and components `clippy`, `rustfmt`.
- Pinned on purpose: the host's `stable` rustup toolchain is corrupted. Never modify `~/.rustup`'s stable toolchain or global rustup config.
- Do not add `llvm-tools` or `rust-src` components (they conflicted). Stable only; no nightly flags.
- Links with the bundled `rust-lld`; no system linker needed. QEMU (`qemu-system-aarch64`) is the runner.

## Settings and why

| Setting | Where | Reason |
| --- | --- | --- |
| target `aarch64-unknown-none-softfloat` | `rust-toolchain.toml`, `.cargo/config.toml` | No FP/SIMD in the kernel, so traps and context switches never save v-registers (and boot needs no `CPACR_EL1` FP enable). |
| `jobs = 6` | `.cargo/config.toml` | Half of the 12 host cores so builds never take the whole CPU; rustc codegen threads share this jobserver. |
| `link-arg=--threads=6` | `.cargo/config.toml` | `rust-lld` ignores cargo's jobserver and would otherwise use every core. |
| `-T linker.ld` | `crates/board/qemu-virt/build.rs` | Kernel memory layout (load address, BSS, stack); binary only. |
| load address `0x4020_0000` | `crates/board/qemu-virt/linker.ld` | QEMU only places its 1 MiB DTB at RAM base (`0x4000_0000`) if it fits below the ELF image. |
| `test-host` alias | `.cargo/config.toml` | Runs tests for the host target, excluding the bare-metal-only `qemu-virt` and `arch`. A string, not an array, so a nested worktree's copy overrides it instead of concatenating. |
| `bench-host` alias | `.cargo/config.toml` | Runs the host `benches/*.rs` targets (`--bench '*'`) for the same crates as `test-host`. |
| `linked_list_allocator` (no features) | `crates/board/qemu-virt` | Kernel heap with `free` (phase 2 task stacks need it); a bare `Heap` with IRQs masked around each call (`arch::irq`), not its spinlock, which could deadlock on one core. In the board crate because it is the binary that owns `#[global_allocator]` and `unsafe` heap init. |
| `panic = "abort"` | both profiles | No unwinding in a kernel. |
| dev `opt-level = 1` | root `Cargo.toml` | Opt-level 0 kernel code has bloated stack frames and slow MMIO loops; measured build cost is zero. Trade-off: some locals show as optimized out in the debugger. |
| release `lto = true`, `codegen-units = 1` | root `Cargo.toml` | Smallest/fastest release image; release only, so the inner loop does not pay for it. |
| `unsafe_code = "forbid"` | `[workspace.lints.rust]` | Every crate is safe Rust by default; the compiler rejects `unsafe` outside `arch` and board crates. |
| `unsafe_op_in_unsafe_fn = "deny"` | `qemu-virt`, `arch` `[lints.rust]` | Each unsafe op inside an `unsafe fn` needs its own `unsafe {}` block and justification. |
| `clippy::undocumented_unsafe_blocks = "deny"` | `qemu-virt`, `arch` `[lints.clippy]` | Enforces the `// SAFETY:` comment rule mechanically. |
| `clippy::multiple_unsafe_ops_per_block = "warn"` | `qemu-virt`, `arch` `[lints.clippy]` | Keeps unsafe blocks small so each `SAFETY` comment covers one operation. |

Evaluated and not applied (all within noise on this crate): `debug = "line-tables-only"`, dev `codegen-units`, toggling `incremental`. No `rustfmt.toml`: defaults already pass.

Re-measure (`time cargo build`, median) before changing any of the above.

## Inner loop

```sh
cargo check            # type-check only, fastest
cargo clippy           # lints; must be clean
cargo fmt              # format (CI-style check: cargo fmt --check)
cargo build            # dev build
cargo test-host        # host tests + QEMU boot e2e tests (crates/e2e); must pass
cargo bench-host       # host benchmarks (min/median); see docs/BENCHMARKS.md
cargo run              # boot in QEMU; prints hello, exceptions, mmu, ram, frames, heap, boot lines and powers off
cargo run -- -append test=mmu-fault  # reads an unmapped address after MMU on; prints the data abort
cargo run -- -append test=yield      # tasks a and b print 0..2 in turn via `svc` yield
cargo run -- -append test=bench      # prints the yield round trip in ns
cargo run -- -append test=preempt    # timer preempts spinning task a; task b prints 0..2
cargo run -- -s -S     # boot halted, gdbstub on localhost:1234; attach lldb/gdb
cargo build --release  # LTO release image
```

Quit a hung QEMU with `Ctrl-A` then `X`.

## Rules for agents

- Run `cargo fmt` before finishing.
- `cargo clippy` must be clean (no warnings, no errors).
- `cargo test-host` must pass, including the e2e boot test; extend `crates/e2e/tests/boot.rs` when boot output changes.
- `cargo run` must still boot and print the hello line.
- Do not raise the `jobs` or linker `--threads` caps.
- New crates use `[lints] workspace = true`. Never opt a crate out of `unsafe_code = "forbid"` unless it is an arch/board crate; put `unsafe` behind a safe API there.
- Hot-path changes report before/after benchmark numbers; >5% regression needs justification (`docs/BENCHMARKS.md`).
- Do not add dependencies without a stated reason.
- Keep linker and profile settings unless a measurement (before/after `time cargo build`) justifies a change; record it in this file.
- Do not touch the toolchain pin, `~/.rustup`, or global rustup config.
- Every change gets a `reviewer` pass (correctness and simplicity) and its findings fixed before it is reported done (`docs/WORKFLOW.md`).
