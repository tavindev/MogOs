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
| nested `cargo build` of `crates/user` | `crates/board/qemu-virt/build.rs` | The boot archive must exist before the kernel compiles (`include_bytes!`); stable cargo has no artifact dependencies, and QEMU `-initrd` would need DTB parsing, frame reservation and a runner change. So the board's build script builds the user programs (release, stripped, own `target/user` dir so it does not wait on the outer build's lock, wrappers removed so `cargo clippy` does not invalidate them) and writes a newc cpio into `OUT_DIR`; `cargo build`/`run` stay one command. |
| `crates/user` excluded from the workspace | root `Cargo.toml`, `[workspace]` in `crates/user/Cargo.toml` | Its bins would make `cargo run` ambiguous and get built for the host by `test-host`. Lint and format it with `--manifest-path crates/user/Cargo.toml` (commands below). Its own `[workspace]` table keeps a worktree nested under the main checkout from attaching it to the outer workspace. |
| `-T link.ld`, `-zmax-page-size=4096` | `crates/user/build.rs` | User programs: one RX `PT_LOAD` (text, rodata) and one RW (data, bss), page-aligned from 4 GiB; lld's 64 KiB default page size padded each program to 64 KiB. |
| load address `0x4020_0000` | `crates/board/qemu-virt/linker.ld` | QEMU only places its 1 MiB DTB at RAM base (`0x4000_0000`) if it fits below the ELF image. |
| `-global virtio-mmio.force-legacy=false` | runners in `.cargo/config.toml`, `crates/e2e` | QEMU 9.2 defaults virtio-mmio to legacy (version 1); the driver speaks modern (version 2), which takes three 64-bit queue addresses instead of legacy's page-size register and one page-aligned ring block. |
| `test-host` alias | `.cargo/config.toml` | Runs tests for the host target, excluding the bare-metal-only `qemu-virt` and `arch`. A string, not an array, so a nested worktree's copy overrides it instead of concatenating. |
| `bench-host` alias | `.cargo/config.toml` | Runs the host `benches/*.rs` targets (`--bench '*'`) for the same crates as `test-host`. |
| `linked_list_allocator` (no features) | `crates/board/qemu-virt` | Kernel heap with `free` (phase 2 task stacks need it); a bare `Heap` with IRQs masked around each call (`arch::irq`), not its spinlock, which could deadlock on one core. In the board crate because it is the binary that owns `#[global_allocator]` and `unsafe` heap init. |
| `panic = "abort"` | both profiles | No unwinding in a kernel. |
| dev `opt-level = 1` | root `Cargo.toml` | Opt-level 0 kernel code has bloated stack frames and slow MMIO loops; measured build cost is zero. Trade-off: some locals show as optimized out in the debugger. |
| release `lto = true`, `codegen-units = 1` | root `Cargo.toml` | Smallest/fastest release image; release only, so the inner loop does not pay for it. |
| `unsafe_code = "forbid"` | `[workspace.lints.rust]` | Every crate is safe Rust by default; the compiler rejects `unsafe` outside `arch` and board crates. |
| `unsafe_op_in_unsafe_fn = "deny"` | `qemu-virt`, `arch`, `user` `[lints.rust]` | Each unsafe op inside an `unsafe fn` needs its own `unsafe {}` block and justification. |
| `clippy::undocumented_unsafe_blocks = "deny"` | `qemu-virt`, `arch`, `user` `[lints.clippy]` | Enforces the `// SAFETY:` comment rule mechanically. |
| `clippy::multiple_unsafe_ops_per_block = "warn"` | `qemu-virt`, `arch`, `user` `[lints.clippy]` | Keeps unsafe blocks small so each `SAFETY` comment covers one operation. |

Evaluated and not applied (all within noise on this crate): `debug = "line-tables-only"`, dev `codegen-units`, toggling `incremental`. No `rustfmt.toml`: defaults already pass.

Re-measure (`time cargo build`, median) before changing any of the above.

## Inner loop

```sh
cargo check            # type-check only, fastest
cargo clippy           # lints; must be clean
cargo fmt              # format (CI-style check: cargo fmt --check)
cargo clippy --manifest-path crates/user/Cargo.toml --target-dir target/user  # user programs (outside the workspace)
cargo fmt --manifest-path crates/user/Cargo.toml
cargo build            # dev build
cargo test-host        # host tests + QEMU boot e2e tests (crates/e2e); must pass
cargo bench-host       # host benchmarks (min/median); see docs/BENCHMARKS.md
cargo run              # boot in QEMU; prints hello, exceptions, mmu, ram, frames, heap, boot lines and powers off
cargo run -- -append test=mmu-fault  # reads an unmapped address after MMU on; prints the data abort
cargo run -- -append test=yield      # tasks a and b print 0..2 in turn via `svc` yield
cargo run -- -append test=bench      # prints the yield round trip in ns
cargo run -- -append test=preempt    # timer preempts spinning task a; task b prints 0..2
cargo run -- -append test=user       # EL0 process A writes A: 0..9 to its console handle; B reads A's address, then C (B's slot) kernel RAM: both killed (fault: 2 ec=0x24 far=...)
cargo run -- -append test=bench-syscall  # EL0 loop of no-op syscalls, prints the round trip in ns
cargo run -- -append test=handles    # EL0 process writes via its console handle, then a no-write duplicate, a closed and a stale handle fail (H: lines)
cargo run -- -append test=budget     # EL0 process maps pages until ENOMEM (M: lines), exits; free frames before/after its lifetime match
cargo run -- -append test=spawn      # spawner (from the boot archive) checks failing spawns move nothing, spawns child with only the console (S: and C: lines); free frames before/after match
cargo run -- -append test=pipe       # reader blocks on an empty pipe until its child writer writes, reads EOF, waits for exit code 7, respawns into the reused slot; a stale process handle is EBADF; 8 KiB I/O moves 4 KiB (R: and W: lines); free frames before/after match
cargo run -- -append test=wait       # waiter's child A exits before child B is spawned; wait still returns both codes and budgets; closing a third, exited child's handle returns its budget too (P: and C: lines); free frames before/after match
cargo run -- -append test=pi         # timer on: L (priority 1) holds a mutex H (3) blocks on while Mid (2) is ready to spin forever; H acquires only through priority inheritance, then init kills Mid (L:, H:, P: lines; no M: line); free frames before/after match
cargo run -- -append test=bench-pipe # ping and pong echo one byte over two pipes 100000 times; prints the round trip in ns
cargo run -- -drive file=disk.img,if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0 -append test=disk  # attach a raw image (`truncate -s 1M disk.img`); first boot writes block 1 and flushes (disk: wrote), the next reads it back (disk: read ok); without a disk every boot prints disk: none
cargo run -- -drive file=disk.img,if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0 -append test=bench-disk  # image of at least 8 MiB; sequential 4 KiB write+flush and read throughput in MiB/s
cargo run -- -s -S     # boot halted, gdbstub on localhost:1234; attach lldb/gdb
cargo build --release  # LTO release image
```

Quit a hung QEMU with `Ctrl-A` then `X`.

## Rules for agents

- Run `cargo fmt` before finishing.
- `cargo clippy` must be clean (no warnings, no errors), and so must `crates/user` (command above).
- `cargo test-host` must pass, including the e2e boot test; extend `crates/e2e/tests/boot.rs` when boot output changes.
- `cargo run` must still boot and print the hello line.
- Do not raise the `jobs` or linker `--threads` caps.
- New crates use `[lints] workspace = true`. Never opt a crate out of `unsafe_code = "forbid"` unless it is an arch/board crate; put `unsafe` behind a safe API there.
- Hot-path changes report before/after benchmark numbers; >5% regression needs justification (`docs/BENCHMARKS.md`).
- Do not add dependencies without a stated reason.
- Keep linker and profile settings unless a measurement (before/after `time cargo build`) justifies a change; record it in this file.
- Do not touch the toolchain pin, `~/.rustup`, or global rustup config.
- Every change gets a `reviewer` pass (correctness and simplicity) and its findings fixed before it is reported done (`docs/WORKFLOW.md`).
