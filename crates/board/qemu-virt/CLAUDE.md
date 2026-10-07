# `crates/board/qemu-virt` - QEMU `virt` board and the `mog_os` binary

## What this crate is

The adapter that implements `kernel::Board` for QEMU `virt` and owns everything stateful and unsafe outside
`crates/arch`: `kmain`, the PL011 driver (`src/uart.rs`), the virtio-blk driver (`src/virtio_blk.rs`), the global kernel state, the `#[global_allocator]`, the
trap hooks, process construction and ELF loading, the asm test programs (`src/user.s`), `linker.ld`, and `build.rs`,
which builds `crates/user` and bundles it as the boot archive.

It is **NOT** where scheduling, handle, pipe, mutex or syscall-decoding logic lives (`crates/kernel`), nor raw
AArch64 register/table code (`crates/arch`). New policy goes in `kernel` as safe, host-testable code.

## Responsibilities

- `kmain`: reads the DTB at RAM base, builds `QemuVirt`, calls `kernel::run` with the image and DTB reserved.
- `KERNEL` (`Scheduler`, `FrameAllocator`, `Pipes`, `Mutexes`, console `Line`, the MogFS `Fs<FsDisk>` and whether it
  is mounted) and `HEAP` statics. `Fs::new` is const, so the 48 KiB file system is built in the static with an empty
  `FsDisk(None)` (`Io` until `Board::mount` puts the `VirtioBlk` in through `Fs::disk`). File syscalls run their disk I/O inside the trap with IRQs masked: a `sync` holds the core
  for its writes and two flushes. Boot-spawned processes get the root directory as handle 3 once mounted (`spawn_init`).
- Trap hooks `task_switch`, `board_irq`, `board_syscall`, `board_user_fault`: execute the `kernel::syscall::Call`
  that `dispatch` returns (user buffers, pages, frames, wake/block).
- Processes: `spawn_process`, `spawn`, `task_exit`, `kill`, `map`, `release`, `enter` (TTBR0/ASID switch).
- `VirtioBlk` (`src/virtio_blk.rs`) implements `kernel::Disk` (`mogfs::Disk`): modern (version 2) virtio-mmio only,
  one 4-entry queue in one frame, one request in flight, completion polled (no IRQ), DMA straight to the caller's
  blocks (only inside the identity-mapped RAM GiB); a buffer outside it, a request past the capacity or a device
  failure is `mogfs::Error::Io`. `Board::disk` hands it out
  once (`DISK_TAKEN`), scanning QEMU `virt`'s fixed virtio-mmio transports from the highest down and stopping at the first empty one
  (QEMU `virt` fills them from the top with no gaps; a board fact like `UART_IRQ`).
- `build.rs`: nested `cargo build` of `crates/user` into `target/user`, newc `boot.cpio` into `OUT_DIR` (plus a
  non-ELF `bad` entry), `-T linker.ld`. Why it is built this way: `docs/DEVELOPMENT.md` settings table.

## Boundaries (hard)

- `#![no_std]`, `#![no_main]`. Deps: `arch`, `dtb`, `kernel`, `mm`, `mogfs` (its `Error`), `linked_list_allocator` (no features).
- Opts out of `forbid(unsafe_code)` (lints: `docs/DEVELOPMENT.md` settings table); every `unsafe` block has a one-line
  `// SAFETY:` and every `unsafe fn` a `# Safety` section.
- Depends on `kernel`, never the reverse. UART, GIC and RAM come from the DTB; board constants fix the rest:
  `UART0` (user `write`, panic and fault output), `DTB` (RAM base), `KERNEL_L1` (GiB 0 device, GiB 1 RAM),
  `UNMAPPED`, `TIMER_IRQ` (27), `VIRTIO`, `VIRTIO_STRIDE`, `VIRTIO_COUNT` (32 virtio-mmio transports from `0x0a00_0000`, `0x200` apart), the PSCI call. QEMU runs with
  `-global virtio-mmio.force-legacy=false` (the driver rejects legacy) and `-global virtio-mmio.ioeventfd=off`
  (`docs/DEVELOPMENT.md` settings table).
- Bare-metal only: excluded from `cargo test-host`.

## Vocabulary

- **Boot context**: slot 0, the `kmain` stack, boot table, ASID 0.
- **`Program`**: an asm program in `user.s`, spawned by `spawn_user`. **Archived program**: an ELF from
  `crates/user` in the boot archive, spawned by `spawn_archived` or the `spawn` syscall. Keep the two distinct.
- **Trap frame = task context**: `switch` / `task_switch` save the current frame address and return the next one.

## Invariants & rules

- `KERNEL` and `HEAP` are `UnsafeCell` globals touched only with IRQs masked on the one core; trap hooks are entered
  masked, `Board` methods wrap access in `arch::irq::disable` / `restore`. Every `// SAFETY` that touches them rests on this.
- A process's slot is its ASID (`MAX_TASKS <= 256`, const-asserted); the boot table keeps ASID 0 (`enter`).
- Every frame a process uses (tables, pages, kernel stack, pipe pages it creates) is charged to its `Budget`;
  `spawn_process` returns every frame on failure, `spawn` moves nothing on failure.
- `task_exit` frees the kernel stack it runs on: sound only while nothing allocates before the trap returns.
- A blocking call rewinds its `svc` (`block` calls `TrapFrame::restart`) and reruns when woken.
- User memory is read only through `user_bytes` / `user_bytes_mut`, which probe every page with
  `arch::user_readable` / `user_writable`; slices live only until the trap returns.
- User layout: code at `USER_BASE` (4 GiB), ELF segments within `IMAGE` (below the top two pages), one stack page
  below `USER_STACK_TOP`; a `spawn` with arguments copies them to the end of that page and adds a stack page below it
  (both charged to the child), and the child starts with x0-x2 = count, address, length,
  `map` from `MAP_BASE` upward. Kernel blocks (`KERNEL_L1`) are EL1-only in every address space.
- `MAX_MUTEXES = MAX_TASKS * MAX_HANDLES`: every live mutex holds a handle, so the handle tables are the quota.
- `linker.ld` provides `__stack_top`, `__bss_start`, `__bss_end`, `__kernel_start`, `__kernel_end`; its load address
  is explained in `docs/DEVELOPMENT.md`.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Every scenario in `crates/e2e/tests/boot.rs` boots this binary; run one with
  `cargo test --target aarch64-apple-darwin -p e2e -- <test name>`. Manual: `cargo run -- -append test=<name>`
  (list in `docs/DEVELOPMENT.md`).
- `cargo build`, `cargo clippy` clean. Hot paths (`switch`, `board_syscall`, pipes) report `test=bench`,
  `test=bench-syscall`, `test=bench-pipe` numbers (`docs/BENCHMARKS.md`).

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
