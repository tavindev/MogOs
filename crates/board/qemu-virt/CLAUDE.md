# `crates/board/qemu-virt` - QEMU `virt` board and the `mog_os` binary

## What this crate is

The adapter that implements `kernel::Board` for QEMU `virt` and owns everything stateful and unsafe outside
`crates/arch`: `kmain`, the global kernel state, the `#[global_allocator]` and the `Board` impl (`src/main.rs`), the
PL011 driver (`src/uart.rs`), the virtio-blk driver (`src/virtio_blk.rs`), the virtio-net driver and the net task (`src/virtio_net.rs`, `src/net.rs`), the trap hooks (`src/trap.rs`), process
construction, ELF loading and threads (`src/process.rs`), user memory access (`src/usermem.rs`), the file system's disk
(`src/fs.rs`), the asm test programs (`src/user.s`), `linker.ld`, and `build.rs`, which builds `crates/user` and
bundles it as the boot archive.

It is **NOT** where scheduling, handle, pipe, mutex or syscall-decoding logic lives (`crates/kernel`), nor raw
AArch64 register/table code (`crates/arch`). New policy goes in `kernel` as safe, host-testable code.

## Responsibilities

- `kmain`: turns on the MMU (`arch::enable_mmu`, first), reads the DTB at RAM base, stores its PSCI `method` (`CONDUIT`)
  and installs core 0's vectors with it (`arch::install_vectors`), reads its core count (at most
  `MAX_CPUS`), routes `UART_IRQ` to core 0 (`GICD_ITARGETSR`, else a GIC with several cores delivers it nowhere), builds
  `QemuVirt`, calls `kernel::run` with the image and DTB reserved. `Board::start_cpus`, the last step of boot, starts core 1
  with PSCI `CPU_ON` without waiting, and core 1 starts the rest (a refused `CPU_ON` panics). `kmain_secondary`: a started core installs its own vectors and records them, enables its GIC CPU
  interface, timer PPI and `RESCHEDULE_SGI` (its banked `ISENABLER0`) and idles in `wfi`; only under `test=smp`
  (`SMP_TEST`) does it print `cpu <n>: online` and arm its timer. It runs no task yet (step 25b).
- `Nospec`, the `kernel::Clamp` `dispatch` and `split` use (`arch::clamp`).
- `KERNEL: Lock<Kernel>` (`Scheduler` with its process table, `FrameAllocator`, `Pipes`, `Mutexes`, console `Line`,
  the MogFS `Fs<FsDisk>` and whether it is mounted, and `buf`, the 8 KiB a syscall copies user inputs into), `HEAP` and
  `CONSOLE: Lock<Uart>` statics. `Fs::new` is const, so the 48 KiB file system is built in the static with an empty
  `FsDisk(None)` (`Io` until `Board::mount` puts the `VirtioBlk` in through `Fs::disk`). File syscalls run their disk I/O inside the trap under `KERNEL`: a `sync` holds it
  for its writes and two flushes. Boot-spawned processes get the root directory as handle 3 once mounted (`spawn_init`).
- Trap hooks `task_switch`, `board_irq`, `board_syscall`, `board_user_fault`: execute the `kernel::syscall::Call`
  that `dispatch` returns (user buffers, pages, frames, wake/block). `board_unlock`, called by the trap exit, releases `KERNEL`.
- `Board::console` writes (`Console`) hold `CONSOLE` for a whole `write_fmt`, so no other `Console` line splits it (the unlocked writers below can); it is the PL011
  at `UART0`, like every other UART access. `test=bench-lock`'s `lock_round_trips` (ticket vs test-and-set) and `add_locked`;
  `test=smp`'s `cpus` and `ticked_cpus` (`TICKED`, a bit per core set on each tick). `report_speculation` records core 0's
  vectors, waits until every started core has recorded its own (`arch::speculation`), then prints `spec: ...`.
- Processes and threads: `spawn_process`, `spawn`, `thread`, `map` (`src/process.rs`); `end_thread`, `end_process`,
  `exit_thread`, `exit_process`, `kill`, `release`, and `switch`, which moves SP_EL0 and TPIDR_EL0 on every switch with
  a user thread on either side and writes TTBR0 only when the process changes (`src/trap.rs`).
- `VirtioBlk` (`src/virtio_blk.rs`) implements `kernel::Disk` (`mogfs::Disk`): modern (version 2) virtio-mmio only,
  one 4-entry queue in one frame, one request in flight, completion polled (no IRQ), DMA straight to the caller's
  blocks (only inside the identity-mapped RAM GiB); a buffer outside it, a request past the capacity or a device
  failure is `mogfs::Error::Io`. `Board::disk` hands it out
  once (`DISK_TAKEN`), scanning QEMU `virt`'s fixed virtio-mmio transports from the highest down and stopping at the first empty one
  (QEMU `virt` fills them from the top with no gaps; a board fact like `UART_IRQ`).
- `VirtioNet` (`src/virtio_net.rs`) implements `net::Nic`: modern virtio-mmio only, `VIRTIO_NET_F_MAC` and
  `VIRTIO_F_VERSION_1` (12-byte header, no offloads), RX queue 0 and TX queue 1 of `QUEUE_SIZE` (64) descriptors,
  each owning a fixed 2 KiB buffer of one `POOL_FRAMES` (64) pool; RX buffers stay posted (re-posted after the stack
  reads them, one notify per poll), TX descriptors are a free bitmask reclaimed from the used ring on demand, and only
  RX interrupts. Used-ring ids and lengths are device-written and range-checked before a buffer is touched.
- `src/net.rs`: `Board::nic` scans the transports for the first net device (the kernel asks only with a `net=`
  bootarg, so no boot without one pays for it); `Board::memory` hands out frames as a `&'static mut [u8]`;
  `Board::start_net` stores the NIC and the kernel's `Network` in `NET`, unmasks the SPI (`VIRTIO_IRQ` + transport
  index, routed to core 0), spawns the net task, starts the timer and sets `STARTED`, after which `spawn_init` adds a
  NetStack handle (connect, listen, duplicate, transfer) and `Board::tasks` stops counting the net task. The task polls
  whenever `PENDING` is set (the NIC's interrupt, which `board_irq` acks; the tick once `DEADLINE` passed; every socket
  call; frames left on the loopback wire), then wakes `Event::NetIo`; otherwise it blocks on `Event::Net`. The socket
  syscalls (`socket`, `bind`, `listen`, `shutdown`, `submit`, `io_wait`) and a socket handle's `dup` and last `close`
  (which refunds the owner's budget) are thin calls into `Network` under `NET`.
- `build.rs`: nested `cargo build` of `crates/user` into `target/user`, `make -C c` (musl, busybox and the C
  programs into `target/c`, `c/CLAUDE.md`), newc `boot.cpio` into `OUT_DIR` (every user program, busybox as `sh`,
  `hello`, `cbench`, plus a non-ELF `bad` entry), `-T linker.ld`. Why it is built this way: `docs/DEVELOPMENT.md` settings table.

## Boundaries (hard)

- `#![no_std]`, `#![no_main]`. Deps: `arch`, `dtb`, `kernel`, `mm`, `mogfs` (its `Error`), `net` (`Nic`, `Stack`), `linked_list_allocator` (no features).
- Opts out of `forbid(unsafe_code)` (lints: `docs/DEVELOPMENT.md` settings table); every `unsafe` block has a one-line
  `// SAFETY:` and every `unsafe fn` a `# Safety` section.
- Depends on `kernel`, never the reverse. GIC and RAM come from the DTB; board constants fix the rest:
  `UART0` (the PL011 at `0x0900_0000`: all console output and input, never read from the DTB), `DTB` (RAM base), `KERNEL_L1` (GiB 0 device, GiB 1 RAM),
  `UNMAPPED`, `TIMER_IRQ` (27), `UART_IRQ` (33), `VIRTIO_IRQ` (48, transport `i`'s SPI is `48 + i`), `RESCHEDULE_SGI` (0), core `n`'s MPIDR (`n`), `SECONDARY_STACK` (16 KiB), `VIRTIO`, `VIRTIO_STRIDE`, `VIRTIO_COUNT` (32 virtio-mmio transports from `0x0a00_0000`, `0x200` apart), the PSCI calls (`SYSTEM_OFF`, `CPU_ON`, by HVC). QEMU runs with
  `-global virtio-mmio.force-legacy=false` (the driver rejects legacy) and `-global virtio-mmio.ioeventfd=off`
  (`docs/DEVELOPMENT.md` settings table).
- Bare-metal only: excluded from `cargo test-host`.

## Vocabulary

- **Boot context**: slot 0, the `kmain` stack, boot table, ASID 0.
- **`Program`**: an asm program in `user.s`, spawned by `spawn_user`. **Archived program**: an ELF from
  `crates/user` in the boot archive, spawned by `spawn_archived` or the `spawn` syscall. Keep the two distinct.
- **Trap frame = task context**: `switch` / `task_switch` save the current frame address and return the next one.

## Invariants & rules

- Kernel state is reached only through `arch::Lock`s: `KERNEL` (the big lock), then `NET` (the NIC and stack, only
  under `KERNEL`), then `HEAP` or `CONSOLE` (leaves, nothing taken under them); never another order. Every trap hook takes `KERNEL` with `lock_masked` and returns holding
  it (`Guard::leak`); the trap exit releases it once, after `mov sp, x0`, through `board_unlock`, so no core resumes a
  task whose kernel stack another core still runs on. `breakpoint_self_test` takes it before its `brk`. `Board`
  methods take `lock()` guards and never hold one across a switch (`run_others` drops it before `yield_now`). Panic,
  fault, echo and user `write` output use `UART0` directly, never `CONSOLE`.
- Locks need the MMU on (exclusives), so `kmain` calls `enable_mmu` first, before any output, trap or secondary core.
- Only core 0 runs tasks: `board_irq` switches only there, since the scheduler has one `current`. Every core still
  takes `KERNEL` in its trap hooks. IRQs dispatch on `iar & 0x3ff` and EOI the full IAR.
- Everything a secondary reads (`GIC_DIST`, `GIC_CPU`, `CPUS`, `SMP_TEST`, `CONDUIT`) is stored before its `CPU_ON`, which `dsb ish` precedes.
- A process's index is its ASID (`MAX_PROCESSES <= 256`, const-asserted); index 0 is the kernel, whose boot table
  keeps ASID 0 (`switch`). Tables: `MAX_TASKS` (8) threads, the boot context included, and `MAX_PROCESSES` (8)
  processes, the kernel included.
- Every frame a process uses (tables, pages, each thread's kernel stack, pipe pages it creates) is charged to its
  `Budget`; a thread's end refunds its stack (`free_stack`). `spawn_process` returns every frame on failure; a failed
  `spawn` or `thread` changes nothing.
- A thread's end frees the kernel stack it may run on, and a process's end frees its index before `exit_process` has
  switched away from its address space and freed it (`flush_asid`, then `free_space`): sound because both go back
  under `KERNEL`, which no core can take until the trap exit has left that stack and released it. Step 25b, with
  threads on other cores, makes the last thread to leave a core do the free, and frees the index only after it.
- Ending a process (`exit`, a fault, `kill`) takes and releases its handles first, then ends every thread (mutexes
  released, a lent boost dropped, stacks refunded), all before `switch` picks the next task, so whatever they woke can
  be it.
- Every new `Process` or `Thread` handle is counted (`Scheduler::held`): the one `spawn_process` hands out (the
  spawner's, or init's own), `thread`'s, and each `dup`; `release` uncounts each closed one.
- A blocking call rewinds its `svc` (`block` calls `TrapFrame::restart`) and reruns when woken.
- User memory is reached only through `UserIn` / `UserOut` (`src/usermem.rs`), which take pointers `dispatch`
  clamped into user space, then probe every page with
  `arch::user_readable` / `user_writable` once and then move bytes by raw copy, in the same trap, before any switch;
  never through a reference, since a sibling thread may write the memory meanwhile. Inputs the kernel parses (paths,
  spawn arguments and handle lists) are copied into `buf` (`copy_in`) once and validated there; bulk data goes straight
  between user memory and its destination (a pipe page) or through `buf` (console, files, `readdir`). A pipe read that
  would wait skips the probe (`Pipe::read_waits`): under hvf a probe costs more than the rest of the call.
- User layout: code at `USER_BASE` (4 GiB), ELF segments within `IMAGE` (below the top two pages and an unmapped guard page, so a stack overflow faults), one stack page
  below `USER_STACK_TOP`; a `spawn` with arguments copies them to the end of that page and adds a stack page below it
  (both charged to the child), and the child starts with x0-x2 = count, address, length,
  `map` from `MAP_BASE` upward. Kernel blocks (`KERNEL_L1`) are EL1-only in every address space.
- `MAX_MUTEXES = MAX_PROCESSES * MAX_HANDLES`: every live mutex holds a handle, so the handle tables are the quota.
- `linker.ld` provides `__stack_top`, `__bss_start`, `__bss_end`, `__kernel_start`, `__kernel_end`, and above
  `__stack_top` the secondaries' stacks (core `n`'s ends at `__stack_top + n * 0x4000`), inside the reserved image; its load address
  is explained in `docs/DEVELOPMENT.md`.
- Performance is the moat: a slowdown is never accepted because it has an explanation; it is removed, or shown to
  be unavoidable with before/after numbers (`docs/BENCHMARKS.md`).

## How it's tested

- Every scenario in `crates/e2e/tests/boot.rs` boots this binary; run one with
  `cargo test --target aarch64-apple-darwin -p e2e -- <test name>`. Manual: `cargo run -- -append test=<name>`
  (list in `docs/DEVELOPMENT.md`).
- `cargo build`, `cargo clippy` clean. Hot paths (`switch`, `board_syscall`, pipes, the lock) report `test=bench`,
  `test=bench-syscall`, `test=bench-pipe`, `test=bench-lock` numbers (`docs/BENCHMARKS.md`).

---

> After changing anything in this crate, run the `reviewer` pass (`docs/WORKFLOW.md`, step 5). Facts a change makes
> stale (names, signatures, constants, test names) are updated in the same commit; changing a boundary, rule or
> invariant needs explicit user approval.
