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
  and installs core 0's boot table (`arch::install_boot_vectors`; the choice waits for `report_speculation`), and, in one walk of its cores, writes the cores'
  table right after the image (reserved with it): the MPIDR of each core by dense index (0 the boot core, the rest in
  DTB order), then the GIC's redistributor regions (base, frame count); maps any GiB of those regions past
  `KERNEL_ENTRIES` into the boot table (`arch::map_device_gib`; spawned address spaces copy them too, `gic_gibs`);
  enables the GICv3 distributor (affinity routing, Group 1; it panics without an `arm,gic-v3`), routes `UART_IRQ` to
  core 0 (Group 1, `GICD_IROUTER`), builds `QemuVirt`, calls `kernel::run` with the DTB's block, the image and the
  table reserved. `Board::init_cpus`, called by `kernel::run` right after the frame allocator is built, takes one
  `alloc_contiguous` of a block per core (a 16 KiB stack, then a copy of the `.percpu` template whose start is the
  stack's top; no guard page) and makes block 0's area core 0's (`arch::enter_percpu`). `Board::start_cpus`, the last
  step of boot, starts core 1 with PSCI `CPU_ON` (target from the table, context id its area and index) without
  waiting; core k starts 2k and 2k + 1 first thing (a tree: about log2 N levels; a refused `CPU_ON` panics).
  `kmain_secondary`: a started core installs its own vectors and records them, enables its GIC CPU interface
  (`enable_gic_cpu`: its redistributor, checked against its MPIDR, woken, SGIs and PPIs in Group 1, then the ICC
  system registers), timer PPI, `RESCHEDULE_SGI` and `PING_SGI` (`GICR_ISENABLER0`), lets EL0 read the counter, and
  becomes its idle context (`idle`, `wfi` in a loop); each counts itself in `ONLINE` once its GIC is up; only under
  `test=smp` (`SMP_TEST`) does it arm its timer once. Core 0's idle context runs on block 0's stack (core 0 keeps its
  boot stack), its first frame built by `init_frames`, which also gives the scheduler its cores
  (`Scheduler::start_cores`).
- `Nospec`, the `kernel::Clamp` `dispatch` and `split` use (`arch::clamp`).
- `KERNEL: Lock<Kernel, Kernel>` (`Scheduler` with its process table, `Pipes`, `Mutexes`, console `Line`, the MogFS
  `Fs<FsDisk>` and whether it is mounted, the per-inode open counts `Opens`), `FRAMES` (the `FrameAllocator`), `HEAP`
  and `CONSOLE: Lock<Uart, Console>` statics; `PROCESSES` (`process.rs`), per process index its lock (`kernel::Process`,
  the map cursor), its atomic `Budget` and its handle `Table`, each starting a 128-byte line, reached without
  `KERNEL`; per-CPU `CURRENT` (the core's current process index, written by its own `switch`), `BUF` (the 8 KiB a
  syscall copies user inputs into) and `DEFERRED` (`trap.rs`). `Fs::new` is const, so the 48 KiB file system is built in the static with an empty
  `FsDisk(None)` (`Io` until `Board::mount` puts the `VirtioBlk` in through `Fs::disk`). File syscalls run their disk I/O inside the trap under `KERNEL`: a `sync` holds it
  for its writes and two flushes. Boot-spawned processes get the root directory as handle 3 once mounted (`spawn_init`).
- Trap hooks `task_switch`, `board_irq`, `board_syscall`, `board_user_fault`: execute the `kernel::syscall::Call`
  that `dispatch` returns (user buffers, pages, frames, wake/block). `board_syscall` only sorts the call into a
  function per class, each a tail call so a path saves only the registers it uses: `io_call` (`io`, decoded by
  `dispatch_io`, no jump table), `shared_call`, `alone_table_call` and `other_call`. It looks the caller's handles up
  without a lock: a console write takes only `CONSOLE` and `map` only its process's lock and `FRAMES`; a call on
  shared state alone (`SHARED_CALLS`: `exit`, `wait`, `lock`, `kill`, the file and socket calls but `open`, ...) takes
  `KERNEL` before its lookups (no recheck) and returns holding it; a call that writes the table (`dup`, `close`,
  `pipe`, `mutex`, `open`, `spawn`, `thread`, `socket`, `io_wait`) takes the process lock, then `KERNEL`, and releases
  both before it returns, rechecking the entries it read (a change reruns the call); `io` on a pipe, console read or
  file takes `KERNEL` and returns holding it after the same recheck. A process's only thread (`ProcessEntry::alone`,
  an `OnlyThread` read before `dispatch`: only the caller's own `thread` raises the count) skips the process lock and
  the recheck:
  its `TABLE_CALLS` take `KERNEL` first like the shared ones, its `dup` of a stateless object and its `close` take
  no lock but what the object's release needs, and its table writes skip the seqlock (`ProcessEntry::unshared` takes
  the `OnlyThread` and gives the `Alone` writer; the locked path's writer is the guard's `Process`). `board_unlock` and `board_unlock_work`, called by the
  trap exit, release `KERNEL`, the second after the hook's deferred work.
- `Board::console` writes (`Console`) hold `CONSOLE` for a whole `write_fmt`, so no other `Console` line splits it (the unlocked writers below can); it is the PL011
  at `UART0`, like every other UART access. `test=bench-lock`'s `round_trips` (ticket vs test-and-set lock, `cpu()`, `PerCpu::with`) and `add_locked`; `test=bench-ipi`'s `ipi_round_trips` (`PING_SGI`, answered in `board_irq`);
  `test=smp`'s `cpus`, `cpu`, `online_cpus` (`ONLINE`) and `ticked_cpus` (`TICKED`, a count each core adds 1 to on
  its first tick, guarded by its `PerCpu<bool>` `TICK_COUNTED`, so no core reads another's per-CPU area).
  `report_speculation` chooses and records core 0's vectors (before any EL0 code on core 0), waits until every started core has recorded its own
  (`arch::speculation`), then prints `spec: ...`.
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
- `src/net.rs`: `Board::start_net` only spawns the net task and sets `STARTED`, after which `spawn_init` adds a
  NetStack handle (connect, listen, duplicate, transfer) and `Board::tasks` stops counting the net task. The task first
  sets up (`setup`): it scans the transports for the first net device (only with an address, so no boot without one
  pays for it), takes the ring memory from the frame allocator, builds the kernel's `Network`, stores both in `NET`,
  unmasks the SPI (`VIRTIO_IRQ` + transport index, routed to core 0) and starts the timer; `with_net` waits for it.
  Then it polls
  whenever `PENDING` is set (the NIC's interrupt, which `board_irq` acks; the tick once `DEADLINE` passed; every socket
  call; frames left on the loopback wire), then wakes `Event::NetIo`; otherwise it blocks on `Event::Net`. The socket
  syscalls (`socket`, `bind`, `listen`, `shutdown`, `submit`, `io_wait`) and a socket handle's `dup` and `close` are
  thin calls into `Network` under `NET`; `release` passes the process a handle leaves, and `spawn` calls
  `spawn_charge` (the child's budget pays for the sockets it gets, before the process is built) and `spawned` (it
  holds them; the parent stops paying for those it no longer holds).
- `build.rs`: nested `cargo build` of `crates/user` into `target/user`, `make -C c` (musl, busybox and the C
  programs into `target/c`, `c/CLAUDE.md`), newc `boot.cpio` into `OUT_DIR` (every user program, busybox as `sh`,
  `hello`, `cbench`, plus a non-ELF `bad` entry), `-T linker.ld`. Why it is built this way: `docs/DEVELOPMENT.md` settings table.

## Boundaries (hard)

- `#![no_std]`, `#![no_main]`. Deps: `arch`, `dtb`, `kernel`, `mm`, `mogfs` (its `Error`), `net` (`Nic`, `Stack`), `linked_list_allocator` (no features).
- Opts out of `forbid(unsafe_code)` (lints: `docs/DEVELOPMENT.md` settings table); every `unsafe` block has a one-line
  `// SAFETY:` and every `unsafe fn` a `# Safety` section.
- Depends on `kernel`, never the reverse. GIC and RAM come from the DTB; board constants fix the rest:
  `UART0` (the PL011 at `0x0900_0000`: all console output and input, never read from the DTB), `DTB` (RAM base; its pages read-only and reserved, core 0's boot stack at the top of its 2 MiB block), `KERNEL_ENTRIES` (2: the boot table's GiB 0 device and GiB 1 RAM entries every address space copies),
  `UNMAPPED`, `TIMER_IRQ` (27), `UART_IRQ` (33), `VIRTIO_IRQ` (48, transport `i`'s SPI is `48 + i`), `RESCHEDULE_SGI` (0), `PING_SGI` (1), `CPU_STACK` (16 KiB), `REDIST_STRIDE` (128 KiB), `VIRTIO`, `VIRTIO_STRIDE`, `VIRTIO_COUNT` (32 virtio-mmio transports from `0x0a00_0000`, `0x200` apart), the PSCI calls (`SYSTEM_OFF`, `CPU_ON`, by HVC). QEMU runs with
  `-M virt,gic-version=3`, `-global virtio-mmio.force-legacy=false` (the driver rejects legacy) and `-global virtio-mmio.ioeventfd=off`
  (`docs/DEVELOPMENT.md` settings table).
- Bare-metal only: excluded from `cargo test-host`.

## Vocabulary

- **Boot context**: slot 0, the `kmain` stack, boot table, ASID 0.
- **`Program`**: an asm program in `user.s`, spawned by `spawn_user`. **Archived program**: an ELF from
  `crates/user` in the boot archive, spawned by `spawn_archived` or the `spawn` syscall. Keep the two distinct.
- **Trap frame = task context**: `switch` / `task_switch` save the current frame address and return the next one.

## Invariants & rules

- Kernel state is reached only through `arch::Lock`s, in one order, which the lock levels check at compile time
  (`crates/lock-order`): a process's lock, then `KERNEL`, then `NET` and `SETUP`, then `FRAMES`, then `CONSOLE`;
  `HEAP` is the only leaf (no witness, nothing taken under it). No step holds two locks of one level: `spawn` holds
  only the parent's and writes the unpublished child through `Lock::unshared`. Each trap hook and `Board` method
  starts from `arch::root()`. A hook that switches takes `KERNEL` and returns holding it; the trap exit releases it
  after `mov sp, x0`. A frame is never freed by the core running on it: a thread's own stack is deferred per-CPU work
  (`DEFERRED`) the trap exit's `board_unlock_work` runs after `mov sp, x0` in the same trap; an ended process is
  released in the hold that ended it, once no core runs it (`finish_release`); a process index (its ASID) is freed only
  after `flush_asid` and `free_space`, and `wait` returns only after that. No
  hook holds a process lock across a switch (`io_wait` blocks in two holds). `Board` methods take `lock()` guards and
  never hold one across a switch (`run_others` drops it before `yield_now`). Fault, echo and every non-empty user
  `write` take `CONSOLE`, so each write is whole on any core; only panic output uses `UART0` directly, so a panic under
  `CONSOLE` still prints.
- Locks need the MMU on (exclusives), so `kmain` calls `enable_mmu` first, before any output, trap or secondary core.
- Every core runs tasks from the one run queue under `KERNEL`; each hook reads `arch::cpu()` once and passes it to the
  scheduler. A core with no task runs its idle context (process 0, so `switch` loads the boot table); an idle core's
  only trap is an IRQ, after which it always reschedules. A task made ready (`add`, `wake`) that is still ready and
  run nowhere at the end of the hook signals one idle core with `RESCHEDULE_SGI` (`kick`, `Scheduler::take_woken`;
  once per idle period), never the calling core; a waker that blocked and took the task itself signals none. A thread
  end on another core signals core 0 when it idles or runs the boot context (`boot_waits`), whose `wait` loop counts
  tasks. The tick only preempts a running task: it is rearmed only while the core runs a slot after `start_timer`
  (`TICKS`), and stopped otherwise; an idle core starts it again when it picks a task. A thread another core runs is
  never ended in place: `end_process` and `kill` mark it (`Scheduler::mark`) and signal its core, which ends it in
  `board_irq`, or in `block` if it blocks first; a marked caller gets `EAGAIN` from `thread`, so its process gains
  none. The last thread to end frees the address space (`exit_process`). IRQs dispatch on the INTID `ICC_IAR1_EL1` returns and EOI it
  (`ICC_EOIR1_EL1`), except the special IDs from 1020.
- Everything a secondary reads (`GIC_DIST`, `CPU_TABLE`, `CPUS`, `REDIST_REGIONS`, `BLOCKS`, `BLOCK`, `SMP_TEST`,
  `CONDUIT`) is stored before core 1's `CPU_ON`, which `dsb ish` precedes; a core starts others only after its own entry.
- Secondaries must set every per-core register core 0 sets (vectors, `CNTKCTL_EL1`): EL0 on a core without
  `allow_user_counter` traps its counter reads.
- A process's index is its ASID (`MAX_PROCESSES <= 256`, const-asserted); index 0 is the kernel, whose boot table
  keeps ASID 0 (`switch`). Tables: `MAX_TASKS` (8) threads, the boot context included, and `MAX_PROCESSES` (8)
  processes, the kernel included.
- Every frame a process uses (tables, pages, each thread's kernel stack, pipe pages it creates) is charged to its
  `Budget`; a thread's end refunds its stack under `KERNEL` and frees its frames (`free_stack`), at once or at the trap
  exit. `spawn_process` takes nothing on failure; a failed
  `spawn` or `thread` changes nothing.
- A thread's end frees a kernel stack no core runs on at once, and parks the one its own core runs on (`DEFERRED`)
  until the trap exit has left it. A process's last thread leaves its release to the end of the hook that ended it,
  after that hook's switch away from it (`finish_release`), in the same hold of `KERNEL`: its handles (its lock is
  reached through `Lock::unshared`, as no thread of it is left to take it), then its memory (`flush_asid`,
  `free_space`), then `Scheduler::exited`, so it is reapable, and its index free, only once nothing of it is left, and
  no other core sees it half released. A core that ended its own process's last thread goes to its idle context for
  the release (`switch_after_end`, `Scheduler::to_idle`) and then picks a task, which the release may have woken,
  with no other core signalled for it; after a `kill` of another process it switches only if what the release woke
  should run (it idles, or a ready task outranks it and it is not marked to end).
- Ending a process (`exit`, a fault, `kill`) ends every thread no other core runs (mutexes released, a lent boost
  dropped, stacks refunded) and marks the others, all before `switch` picks the next task.
- Every new `Process` or `Thread` handle is counted (`Scheduler::held`): the one `spawn_process` hands out (the
  spawner's, or init's own), `thread`'s, and each `dup`; `release` uncounts each closed one.
- A blocking call rewinds its `svc` (`block` calls `TrapFrame::restart`) and reruns when woken.
- User memory is reached only through `UserIn` / `UserOut` (`src/usermem.rs`), which take pointers `dispatch`
  clamped into user space, then probe every page with
  `arch::user_readable` / `user_writable` once and then move bytes by raw copy, in the same trap, before any switch;
  never through a reference, since a sibling thread may write the memory meanwhile. Inputs the kernel parses (paths,
  spawn arguments and handle lists) are copied into the core's `BUF` (`copy_in`) once and validated there; bulk data
  goes straight between user memory and its destination (a pipe page) or through `BUF` (console, files, `readdir`). A pipe read that
  would wait skips the probe (`Pipe::read_waits`): under hvf a probe costs more than the rest of the call.
- User layout: code at `USER_BASE` (4 GiB), ELF segments within `IMAGE` (below the top two pages and an unmapped guard page, so a stack overflow faults), one stack page
  below `USER_STACK_TOP`; a `spawn` with arguments copies them to the end of that page and adds a stack page below it
  (both charged to the child), and the child starts with x0-x2 = count, address, length,
  `map` from `MAP_BASE` upward, never reaching `USER_END` (the lower of the first GiB from 4 GiB up that a DTB GIC
  region occupies and 511 GiB; `ENOMEM` past it). Kernel entries (`KERNEL_ENTRIES`, copied from `arch::boot_table()`,
  and any GIC GiB past them) are EL1-only in every address space.
- Interim caps until step 31: `MAX_TASKS`, `MAX_PROCESSES`, `MAX_PIPES` 64 each; `MAX_MUTEXES` stays 8 processes'
  handle tables (128: each thread end scans it) until step 27 deletes mutexes, so `mutex` can be `ENFILE` before the
  handle tables are full.
- `PerCpu` statics are `#[unsafe(link_section = ".percpu")]`, built with `unsafe` `PerCpu::new`; none is touched
  before `init_cpus` (TPIDR_EL1 is 0 until then, which would reach the template).
- `linker.ld` provides `__kernel_start`, `__text_end` and `__rodata_end` (each 2 MiB aligned: `kmain`'s `KernelMap`
  maps text, rodata and the rest as 2 MiB blocks, RX, RO and RW), `.percpu` (`__percpu_start`, `__percpu_end`: the
  per-CPU template, loaded with the image, never written), `__bss_start`, `__bss_end`, `__kernel_end`, and below the
  image, in RAM's first 2 MiB (mapped by pages, the DTB below): `__stack_top` (= `__kernel_start`, core 0's 64 KiB
  stack ends there) and `__boot_guard` (its guard page, also `__stacks`); `kmain` reserves the DTB and `__stacks` up to
  `__kernel_end` plus the cores' table (the text and rodata blocks' padding included); its load address
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
