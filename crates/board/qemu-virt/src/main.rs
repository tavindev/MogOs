#![no_std]
#![no_main]

extern crate alloc;

mod fs;
mod net;
mod process;
mod trap;
mod uart;
mod usermem;
mod virtio_blk;
mod virtio_net;

use core::alloc::{GlobalAlloc, Layout};
use core::fmt::{self, Write};
use core::hint::spin_loop;
use core::ops::Range;
use core::panic::PanicInfo;
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

use arch::{Guard, Lock, MemoryType, l1_block};
use dtb::Dtb;
use fs::FsDisk;
use kernel::console::Line;
use kernel::elf::Segment;
use kernel::handle::{INIT_ARCHIVE, MAX_HANDLES, Rights};
use kernel::mutex::Mutexes;
use kernel::network::Network;
use kernel::pipe::Pipes;
use kernel::syscall::{ENOENT, MAX_BUFFER};
use kernel::{Event, FRAME_WORDS, Full, PRIORITIES, Program, Scheduler};
use linked_list_allocator::Heap;
use mm::{FrameAllocator, PhysAddr};
use mogfs::{Error, Fs};
use process::{executable, spawn_init, user_program};
use uart::Uart;
use virtio_blk::VirtioBlk;
use virtio_net::VirtioNet;

/// The PL011 every console write and read uses (QEMU `virt` fixes it there).
const UART0: PhysAddr = PhysAddr(0x0900_0000);
/// QEMU loads the DTB at RAM base for an ELF kernel (x0 stays 0), if it fits below the image.
const DTB: PhysAddr = PhysAddr(0x4000_0000);
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const GIB: u64 = 1 << 30;
/// Outside both mapped GiBs.
const UNMAPPED: PhysAddr = PhysAddr(0x8000_0000);
/// Device memory in GiB 0 (MMIO), the kernel image, stack, heap and DTB in RAM in GiB 1; EL1-only, global.
const KERNEL_L1: [u64; 2] = [
    l1_block(PhysAddr(0), MemoryType::Device),
    l1_block(PhysAddr(GIB), MemoryType::Normal),
];
const PAGE: usize = 4096;
/// Where user programs' code is mapped; one 2 MiB region, so a process needs a single level-3 table.
const USER_BASE: u64 = 1 << 32;
/// Each process's one stack page ends here.
const USER_STACK_TOP: u64 = USER_BASE + (2 << 20);
/// Where a process's first `map` goes; later ones follow it.
const MAP_BASE: u64 = USER_STACK_TOP;
/// No `map` reaches it: the lower of the first GiB from `USER_BASE` up that the DTB's GIC regions occupy, which every
/// address space maps for EL1, and 511 GiB, so the last GiB of the 39-bit VA stays unmapped. Set by `kmain`.
static USER_END: AtomicU64 = AtomicU64::new(0);
/// Where an executable's segments may go: below the two stack pages and an unmapped guard page, in one level-3 table.
const IMAGE: Range<u64> = USER_BASE..USER_STACK_TOP - 3 * PAGE as u64;
/// The boot archive (cpio, newc), built by `build.rs` from `crates/user`.
static ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/boot.cpio"));
/// EL1 virtual timer PPI.
const TIMER_IRQ: u32 = 27;
/// PL011 SPI 1 on QEMU `virt`.
const UART_IRQ: u32 = 33;
const TICK_US: u64 = 10_000;

/// The SGI that wakes an idle core to reschedule, or a core to end its marked thread; enabled on every core.
const RESCHEDULE_SGI: u32 = 0;
const PSCI_CPU_ON: u64 = 0xc400_0003;
/// Each secondary core's stack, and core 0's idle context's, reserved in `linker.ld` above `__stack_top`.
const SECONDARY_STACK: u64 = 0x4000;

/// GIC distributor and CPU interface bases, set before the first IRQ can be delivered and before any secondary starts.
static GIC_DIST: AtomicU64 = AtomicU64::new(0);
static GIC_CPU: AtomicU64 = AtomicU64::new(0);
/// Bit `n` is set once core `n` has taken a timer tick.
static TICKED: AtomicUsize = AtomicUsize::new(0);
/// `Board::start_timer` was called: a core that runs a task ticks.
static TICKS: AtomicBool = AtomicBool::new(false);
/// `test=smp`: secondaries announce themselves and run their timer; otherwise they sleep until 25b gives them work.
static SMP_TEST: AtomicBool = AtomicBool::new(false);
/// The DTB's cores, at most `MAX_CPUS`; `start_cpus` starts them all or panics.
static CPUS: AtomicUsize = AtomicUsize::new(1);
/// Set once `Board::disk` handed out the block device.
static DISK_TAKEN: AtomicBool = AtomicBool::new(false);
/// QEMU `virt`'s 32 virtio-mmio transports: the first one's base, and the stride between them.
const VIRTIO: PhysAddr = PhysAddr(0x0a00_0000);
const VIRTIO_STRIDE: u64 = 0x200;
const VIRTIO_COUNT: u64 = 32;
/// Threads, the boot context included.
const MAX_TASKS: usize = 8;
/// The kernel included; a process's index is its ASID (8 bits).
const MAX_PROCESSES: usize = 8;
const _: () = assert!(MAX_PROCESSES <= 256);
type Sched = Scheduler<MAX_TASKS, MAX_PROCESSES>;
/// Kernel stack per task: 16 KiB.
const TASK_STACK_FRAMES: usize = 4;
const MAX_PIPES: usize = 16;
/// Each live mutex has a handle, so the handle tables are the per-process quota.
const MAX_MUTEXES: usize = MAX_PROCESSES * MAX_HANDLES;

#[global_allocator]
static HEAP: KernelHeap = KernelHeap(Lock::new(Heap::empty()));

/// A leaf lock: nothing else is taken while it is held.
struct KernelHeap(Lock<Heap>);

// SAFETY: `Heap` hands out non-overlapping blocks of at least `layout` from the region `init_heap` gave it.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0
            .lock()
            .allocate_first_fit(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `GlobalAlloc` only passes pointers that `alloc` returned, which are non-null.
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        // SAFETY: `ptr` was allocated from this heap with `layout`.
        unsafe { self.0.lock().deallocate(ptr, layout) }
    }
}

/// The big lock over threads and processes, free frames, pipes, mutexes, console input, the file system and the
/// buffer user inputs are copied into. Every trap hook
/// takes it and returns holding it, and the trap exit releases it (`board_unlock`). Lock order: `KERNEL`, then `HEAP`
/// or `CONSOLE`. File system calls do their disk I/O under it, so a `sync` holds it for its flushes.
static KERNEL: Lock<Kernel> = Lock::new(Kernel {
    sched: Scheduler::new(),
    frames: FrameAllocator::empty(),
    pipes: Pipes::new(),
    mutexes: Mutexes::new(),
    line: Line::new(),
    fs: Fs::new(FsDisk(None)),
    mounted: false,
    buf: [0; 2 * MAX_BUFFER as usize],
});

struct Kernel {
    sched: Sched,
    frames: FrameAllocator<FRAME_WORDS>,
    pipes: Pipes<MAX_PIPES>,
    mutexes: Mutexes<MAX_MUTEXES>,
    line: Line,
    fs: Fs<FsDisk>,
    /// `fs` is mounted: boot-spawned processes get its root as handle 3.
    mounted: bool,
    /// Where a syscall copies user inputs and stages outputs: room for `rename`'s two paths.
    buf: [u8; 2 * MAX_BUFFER as usize],
}

/// Every console write and read on `UART0`, a leaf lock, except panic output, which goes straight to `UART0` so a panic
/// under this lock still prints.
static CONSOLE: Lock<Uart> = Lock::new(Uart::new(UART0));

/// `Board::console`: each formatted write holds `CONSOLE` for its whole line.
#[derive(Clone)]
struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        CONSOLE.lock().write_str(s)
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        CONSOLE.lock().write_fmt(args)
    }
}

/// What a new task's first frame hands to `task_start`; lives at the top of its stack.
struct Start {
    board: QemuVirt,
    entry: fn(&mut QemuVirt, usize) -> !,
    arg: usize,
}

extern "C" fn task_start(start: usize) -> ! {
    // SAFETY: `spawn` wrote a `Start` here, above the task's stack, and only this task uses it.
    let start = unsafe { &mut *(start as *mut Start) };
    (start.entry)(&mut start.board, start.arg)
}

#[derive(Clone)]
struct QemuVirt {
    console: Console,
    entry_us: u64,
}

impl kernel::Board for QemuVirt {
    type Console = Console;
    type Disk = VirtioBlk;
    type Nic = VirtioNet;

    fn console(&mut self) -> &mut Console {
        &mut self.console
    }

    fn exception_level(&self) -> u8 {
        let el: u64;
        // SAFETY: reading CurrentEL has no side effects.
        unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el) };
        (el >> 2) as u8
    }

    fn breakpoint_self_test(&mut self) {
        let _ = Guard::leak(KERNEL.lock());
        // SAFETY: `KERNEL` is held through the guard leaked above, which the trap exit releases.
        unsafe { arch::breakpoint_self_test() }
    }

    fn read_unmapped(&mut self) {
        // SAFETY: the address is unmapped, so the read takes a data abort, which panics instead of returning.
        unsafe { (UNMAPPED.0 as *const u64).read_volatile() };
    }

    fn init_heap(&mut self, region: Range<PhysAddr>) {
        let size = (region.end.0 - region.start.0) as usize;
        let mut heap = HEAP.0.lock();
        assert!(heap.bottom().is_null(), "heap already initialized");
        // SAFETY: the heap is empty (checked above); `region` being unused, mapped RAM is the `Board::init_heap` contract the kernel upholds.
        unsafe { heap.init(region.start.0 as *mut u8, size) }
    }

    fn uptime_us(&self) -> u64 {
        arch::uptime_us() - self.entry_us
    }

    fn start_timer(&mut self) {
        TICKS.store(true, Relaxed);
        arch::timer::arm(TICK_US);
    }

    fn idle(&mut self) {
        arch::irq::wait()
    }

    fn power_off(&mut self) -> ! {
        shutdown()
    }

    fn spawn(&mut self, entry: fn(&mut Self, usize) -> !, arg: usize) -> Result<(), Full> {
        let board = self.clone();
        let mut kernel = KERNEL.lock();
        let Kernel { sched, frames, .. } = &mut *kernel;
        sched.free_slot().ok_or(Full).and_then(|slot| {
            let stack = frames.alloc_contiguous(TASK_STACK_FRAMES).ok_or(Full)?;
            let start = (stack.end.0 as usize - size_of::<Start>()) & !15;
            // SAFETY: `start` is 16-byte aligned and inside the fresh stack, which nothing else references.
            unsafe { (start as *mut Start).write(Start { board, entry, arg }) };
            // SAFETY: `start` is 16-byte aligned and the stack below it is fresh and owned by the new task.
            let frame = unsafe { arch::new_task(start, task_start, start) };
            sched.add(slot, 0, (frame, stack.start), 0);
            kick(sched, arch::cpu());
            Ok(())
        })
    }

    fn yield_now(&mut self) {
        arch::yield_now()
    }

    fn run_others(&mut self) {
        KERNEL.lock().sched.block(arch::cpu(), Event::Idle);
        arch::yield_now()
    }

    fn init_frames(&mut self, frames: FrameAllocator<FRAME_WORDS>) {
        let top = &raw const __stack_top as usize + arch::MAX_CPUS * SECONDARY_STACK as usize;
        // SAFETY: core 0's idle stack, reserved in `linker.ld` above the secondaries' and used by nothing else.
        let idle = unsafe { arch::new_task(top, idle, 0) };
        let mut kernel = KERNEL.lock();
        kernel.frames = frames;
        kernel.sched.start_cores(CPUS.load(Relaxed), idle);
    }

    fn free_frames(&self) -> usize {
        KERNEL.lock().frames.free_count()
    }

    fn spawn_user(&mut self, program: Program, budget: usize) -> Result<(), i64> {
        // `test=map-end` starts the map cursor two pages below `USER_END`.
        let next = match program {
            Program::MapEnd => USER_END.load(Relaxed) - 2 * PAGE as u64,
            _ => MAP_BASE,
        };
        let (code, entry) = user_program(program);
        let segment = Segment {
            vaddr: entry,
            data: 0..code.len(),
            size: code.len() as u64,
            writable: false,
        };
        spawn_init(
            (code, [segment].into_iter(), entry),
            (budget, next),
            0,
            INIT_ARCHIVE,
            &[],
        )
    }

    fn spawn_archived(
        &mut self,
        name: &str,
        budget: usize,
        archive: Rights,
        args: &[u8],
    ) -> Result<(), i64> {
        let file = kernel::cpio::find(ARCHIVE, name.as_bytes()).ok_or(ENOENT)?;
        spawn_init(
            executable(file)?,
            (budget, MAP_BASE),
            PRIORITIES - 1,
            archive,
            args,
        )
    }

    fn tasks(&self) -> usize {
        KERNEL.lock().sched.count() - net::STARTED.load(Relaxed) as usize
    }

    fn disk(&mut self) -> Option<VirtioBlk> {
        let alloc = || KERNEL.lock().frames.alloc();
        if DISK_TAKEN.swap(true, Relaxed) {
            return None;
        }
        // QEMU `virt` fills the transports from the highest address down with no gaps, so the first empty one ends them.
        for i in (0..VIRTIO_COUNT).rev() {
            let base = PhysAddr(VIRTIO.0 + i * VIRTIO_STRIDE);
            // SAFETY: QEMU `virt`'s virtio-mmio transports, in the device-mapped GiB 0, driven only here (`DISK_TAKEN`);
            // frames from the allocator are identity-mapped RAM nobody else uses.
            match unsafe { VirtioBlk::new(base, alloc) } {
                Ok(disk) => return Some(disk),
                Err(0) => break,
                Err(_) => {}
            }
        }
        None
    }

    fn mount(&mut self, disk: VirtioBlk) -> Result<(), Error> {
        let mut kernel = KERNEL.lock();
        *kernel.fs.disk() = FsDisk(Some(disk));
        let mounted = kernel.fs.mount();
        kernel.mounted = mounted.is_ok();
        mounted
    }

    fn has_nic(&self) -> bool {
        net::present()
    }

    fn start_net(&mut self, config: Option<::net::Config>, key: [u64; 2]) {
        net::start(self, config, key)
    }

    fn with_net<R>(&mut self, f: impl FnOnce(&mut Network, Option<&mut VirtioNet>, u64) -> R) -> R {
        net::with(f)
    }

    fn lock_round_trips(&mut self, n: u64, ticket: bool) {
        static TICKET: Lock<()> = Lock::new(());
        static TAS: AtomicBool = AtomicBool::new(false);
        if ticket {
            for _ in 0..n {
                drop(TICKET.lock_masked());
            }
            return;
        }
        for _ in 0..n {
            while TAS.swap(true, Acquire) {
                spin_loop();
            }
            TAS.store(false, Release);
        }
    }

    fn add_locked(&mut self, n: u64) -> u64 {
        static COUNT: Lock<u64> = Lock::new(0);
        for _ in 0..n {
            *COUNT.lock() += 1;
        }
        *COUNT.lock()
    }

    fn start_cpus(&mut self, smp_test: bool) {
        SMP_TEST.store(smp_test, Relaxed);
        // Core 1 starts the rest, so boot pays one call.
        if CPUS.load(Relaxed) > 1 {
            start_cpu(1);
        }
    }

    fn cpus(&self) -> usize {
        CPUS.load(Relaxed)
    }

    fn cpu(&self) -> usize {
        arch::cpu()
    }

    fn ticked_cpus(&self) -> usize {
        TICKED.load(Relaxed).count_ones() as usize
    }
}

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
    static __stack_top: u8;
}

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let entry_us = arch::uptime_us();
    // SAFETY: core 0, MMU off, before any atomic RMW; MMIO is in GiB 0, the image, stacks and DTB in RAM in GiB 1.
    unsafe { arch::enable_mmu(&KERNEL_L1) };
    arch::install_vectors();
    arch::timer::allow_user_counter();

    // SAFETY: RAM base is mapped RAM; we only read the 8-byte FDT header there.
    let header = unsafe { slice::from_raw_parts(DTB.0 as *const u8, 8) };
    let size = dtb::total_size(header).expect("no DTB at RAM base");
    // SAFETY: the magic matched, so QEMU loaded `size` bytes of DTB here and nothing writes them.
    let blob = unsafe { slice::from_raw_parts(DTB.0 as *const u8, size) };

    let image =
        PhysAddr(&raw const __kernel_start as u64)..PhysAddr(&raw const __kernel_end as u64);
    let dtb_range = DTB..PhysAddr(DTB.0 + size as u64);

    let dtb = Dtb::new(blob).expect("bad DTB");
    let gic = dtb.gic().expect("no GICv2 in DTB");
    GIC_DIST.store(gic.0.0, Relaxed);
    GIC_CPU.store(gic.1.0, Relaxed);
    let gic_gib = [gic.0, gic.1].map(|r| r.0 / GIB).into_iter();
    let user_end = gic_gib
        .filter(|&g| g >= USER_BASE / GIB)
        .fold(511, u64::min);
    USER_END.store(user_end * GIB, Relaxed);
    // SAFETY: the DTB's GICv2 registers, in the device-mapped GiB 0.
    unsafe { arch::gic::enable(gic.0, gic.1) };
    for irq in [UART_IRQ, TIMER_IRQ] {
        // SAFETY: as above.
        unsafe { arch::gic::unmask(gic.0, irq) };
    }
    Uart::new(UART0).enable_rx_irq();
    let cpus = dtb.cpus().min(arch::MAX_CPUS);
    CPUS.store(cpus, Relaxed);
    // A one-core GIC delivers every interrupt to that core (ITARGETSR is RAZ/WI), and no other core sends it an SGI.
    if cpus > 1 {
        // SAFETY: as above; UART_IRQ is an SPI and core 0's CPU interface is 0.
        unsafe { arch::gic::route(gic.0, UART_IRQ, 0) };
        // SAFETY: as above.
        unsafe { arch::gic::unmask(gic.0, RESCHEDULE_SGI) };
    }

    kernel::run(
        &mut QemuVirt {
            console: Console,
            entry_us,
        },
        dtb,
        &[image, dtb_range],
    )
}

/// Starts core `cpu` (MPIDR `cpu` on QEMU `virt`) at `arch::secondary_entry` on its stack, without waiting for it.
fn start_cpu(cpu: usize) {
    let stack_top = &raw const __stack_top as u64 + cpu as u64 * SECONDARY_STACK;
    let status: i64;
    // SAFETY: CPU_ON starts an off core on an unused stack; `dsb ish` first completes the stores it reads.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "hvc #0",
            inlateout("x0") PSCI_CPU_ON => status,
            in("x1") cpu,
            in("x2") arch::secondary_entry(),
            in("x3") stack_top,
            clobber_abi("C"),
        )
    };
    assert_eq!(status, 0, "PSCI CPU_ON {cpu}");
}

/// A secondary core's first Rust code, from `arch::secondary_entry`: MMU on, on its own stack, IRQs masked. It turns on
/// its GIC CPU interface, timer PPI and reschedule SGI, and becomes its idle context.
#[unsafe(no_mangle)]
extern "C" fn kmain_secondary() -> ! {
    arch::install_vectors();
    arch::timer::allow_user_counter();
    if arch::cpu() == 1 {
        (2..CPUS.load(Relaxed)).for_each(start_cpu);
    }
    let dist = PhysAddr(GIC_DIST.load(Relaxed));
    // SAFETY: the DTB's GICv2 CPU interface, stored by `kmain` before it started this core, in device-mapped GiB 0.
    unsafe { arch::gic::enable_cpu(PhysAddr(GIC_CPU.load(Relaxed))) };
    for irq in [TIMER_IRQ, RESCHEDULE_SGI] {
        // SAFETY: as above, the distributor; below 32, so this core's banked ISENABLER0.
        unsafe { arch::gic::unmask(dist, irq) };
    }
    if SMP_TEST.load(Relaxed) {
        let _ = writeln!(Console, "cpu {}: online", arch::cpu());
        arch::timer::arm(TICK_US);
    }
    idle(0)
}

/// A core's idle context: sleeps until an IRQ, whose handler switches to a ready task, if any.
extern "C" fn idle(_: usize) -> ! {
    loop {
        arch::irq::wait();
    }
}

/// Sends `cpu` the reschedule SGI.
fn send_sgi(cpu: usize) {
    // SAFETY: the DTB's GICv2 distributor, stored before any IRQ or secondary core, in device-mapped GiB 0; `cpu` is
    // below `MAX_CPUS`.
    unsafe { arch::gic::send_sgi(PhysAddr(GIC_DIST.load(Relaxed)), cpu, RESCHEDULE_SGI) };
}

/// Signals an idle core for each task made ready since the last call, while one is left to signal.
fn kick(sched: &mut Sched, cpu: usize) {
    for _ in 0..sched.take_woken() {
        let Some(core) = sched.claim_idle(cpu) else {
            return;
        };
        send_sgi(core);
    }
}

fn shutdown() -> ! {
    // SAFETY: PSCI SYSTEM_OFF via HVC is the power-off call on QEMU `virt` at EL1.
    unsafe { core::arch::asm!("hvc #0", in("x0") PSCI_SYSTEM_OFF, options(noreturn)) }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let _ = writeln!(Uart::new(UART0), "panic: {info}");
    shutdown()
}
