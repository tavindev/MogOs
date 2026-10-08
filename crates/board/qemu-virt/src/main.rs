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
use core::hint::{black_box, spin_loop};
use core::ops::Range;
use core::panic::PanicInfo;
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize};

use arch::{Conduit, KernelMap, Lock, PerCpu};
use dtb::Dtb;
use fs::FsDisk;
use kernel::console::Line;
use kernel::elf::Segment;
use kernel::file::Opens;
use kernel::handle::{INIT_ARCHIVE, MAX_HANDLES, Rights};
use kernel::mutex::Mutexes;
use kernel::network::Network;
use kernel::pipe::Pipes;
use kernel::syscall::{ENOENT, MAX_BUFFER};
use kernel::{Event, FRAME_WORDS, Full, PRIORITIES, Program, RoundTrip, Scheduler, Violation};
use linked_list_allocator::Heap;
use lock_order as level;
use mm::{FrameAllocator, PhysAddr};
use mogfs::{Error, Fs};
use process::{PROCESSES, executable, spawn_init, user_program};
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
/// The boot table's entries every address space copies: device memory (GiB 0) and RAM (GiB 1), EL1-only, global.
const KERNEL_ENTRIES: usize = 2;
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
/// `test=bench-ipi`'s SGI: core 0 sends it, the target answers with it, and core 0 counts the answer in `PONGS`.
const PING_SGI: u32 = 1;
static PONGS: AtomicU64 = AtomicU64::new(0);
const PSCI_CPU_ON: u64 = 0xc400_0003;
/// Each core's per-CPU block: a stack of `CPU_STACK` bytes, then its copy of the `.percpu` template, whose start is
/// the stack's top (core 0 keeps its boot stack; block 0's runs its idle context).
const CPU_STACK: u64 = 0x4000;
/// The blocks, one `alloc_contiguous` sized at boot: the first's address, and each one's size.
static BLOCKS: AtomicU64 = AtomicU64::new(0);
static BLOCK: AtomicU64 = AtomicU64::new(0);
/// The cores' table, written by `kmain` right after the image (and reserved with it): `CPUS` MPIDRs by dense index (0
/// the boot core, the rest in DTB order), then `REDIST_REGIONS` pairs of the DTB's redistributor regions' base and
/// frame count, then a `u32` speculation record per core.
static CPU_TABLE: AtomicU64 = AtomicU64::new(0);
static REDIST_REGIONS: AtomicUsize = AtomicUsize::new(0);

/// The GICv3 distributor, set before the first IRQ can be delivered and before any secondary starts.
static GIC_DIST: AtomicU64 = AtomicU64::new(0);
/// A GICv3 redistributor's two 64 KiB frames (control, then SGIs and PPIs).
const REDIST_STRIDE: u64 = 0x2_0000;
/// Cores that have taken a timer tick, each counted once (`TICK_COUNTED`).
static TICKED: AtomicUsize = AtomicUsize::new(0);
#[unsafe(link_section = ".percpu")]
// SAFETY: in `.percpu`.
static TICK_COUNTED: PerCpu<bool> = unsafe { PerCpu::new(false) };
/// `Board::start_timer` was called: a core that runs a task ticks.
static TICKS: AtomicBool = AtomicBool::new(false);
/// Cores whose GIC is set up, counted once each (core 0 at `start_cpus`).
static ONLINE: AtomicUsize = AtomicUsize::new(0);
/// `test=smp`: secondaries arm their timer once, so each takes a tick.
static SMP_TEST: AtomicBool = AtomicBool::new(false);
/// The DTB's cores; `start_cpus` starts them all or panics.
static CPUS: AtomicUsize = AtomicUsize::new(1);
/// The DT's PSCI conduit for SMCCC calls: 0 none, 1 `hvc`, 2 `smc`; stored before any secondary starts.
static CONDUIT: AtomicU8 = AtomicU8::new(0);
/// Set once `Board::disk` handed out the block device.
static DISK_TAKEN: AtomicBool = AtomicBool::new(false);
/// QEMU `virt`'s 32 virtio-mmio transports: the first one's base, and the stride between them.
const VIRTIO: PhysAddr = PhysAddr(0x0a00_0000);
const VIRTIO_STRIDE: u64 = 0x200;
const VIRTIO_COUNT: u64 = 32;
/// Threads, the boot context included. Interim, as `MAX_PROCESSES` and `MAX_PIPES` (64 each, room for `bench-smp`'s
/// 12 workers): step 31 removes all three.
const MAX_TASKS: usize = 64;
/// The kernel included; a process's index is its ASID (8 bits).
const MAX_PROCESSES: usize = 64;
const _: () = assert!(MAX_PROCESSES <= 256);
type Sched = Scheduler<MAX_TASKS, MAX_PROCESSES>;

/// `kernel::Clamp` over `arch::clamp`: user values that index kernel memory, bounded behind one `csdb`.
struct Nospec;

impl kernel::Clamp for Nospec {
    #[inline(always)]
    fn clamp<const N: usize>(values: [u64; N], maxes: [u64; N]) -> [u64; N] {
        arch::clamp(values, maxes)
    }

    #[inline(always)]
    fn mask(value: u64, mask: u64) -> u64 {
        arch::mask(value, mask)
    }
}
/// Kernel stack per task: 16 KiB.
const TASK_STACK_FRAMES: usize = 4;
const MAX_PIPES: usize = 64;
/// Mutexes: 8 processes' worth of handle tables, kept at its size before the interim 64 processes (each thread end
/// scans it), until step 27 deletes them.
const MAX_MUTEXES: usize = 8 * MAX_HANDLES;

#[global_allocator]
static HEAP: KernelHeap = KernelHeap(Lock::new(Heap::empty()));

struct KernelHeap(Lock<Heap, level::Leaf>);

// SAFETY: `Heap` hands out non-overlapping blocks of at least `layout` from the region `init_heap` gave it.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0
            .lock_leaf()
            .allocate_first_fit(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `GlobalAlloc` only passes pointers that `alloc` returned, which are non-null.
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        // SAFETY: `ptr` was allocated from this heap with `layout`.
        unsafe { self.0.lock_leaf().deallocate(ptr, layout) }
    }
}

/// The big lock over threads and processes, pipes, mutexes, console input, the file system and its open counts. A
/// trap hook that switches takes it and returns holding it, and the trap exit releases it (`board_unlock`); a call that
/// touches only its own process takes it not at all. File system calls do their disk I/O under it, so a `sync` holds
/// it for its flushes.
static KERNEL: Lock<Kernel, level::Kernel> = Lock::new(Kernel {
    sched: Scheduler::new(),
    pipes: Pipes::new(),
    mutexes: Mutexes::new(),
    line: Line::new(),
    fs: Fs::new(FsDisk(None)),
    mounted: false,
    opens: Opens::new(),
    deferred: false,
});

struct Kernel {
    sched: Sched,
    pipes: Pipes<MAX_PIPES>,
    mutexes: Mutexes<MAX_MUTEXES>,
    line: Line,
    fs: Fs<FsDisk>,
    /// `fs` is mounted: boot-spawned processes get its root as handle 3.
    mounted: bool,
    opens: Opens<{ MAX_PROCESSES * MAX_HANDLES }>,
    /// This hold left `trap::Deferred` work for the trap exit; taken by the hook's `Resume`.
    deferred: bool,
}

/// The free frames: one pass of the bitmap per `spawn` or `map`, which take every frame they need at once.
static FRAMES: Lock<FrameAllocator<FRAME_WORDS>, level::Frames> =
    Lock::new(FrameAllocator::empty());

#[unsafe(link_section = ".percpu")]
// SAFETY: in `.percpu`.
/// This core's current process index, written by its own switches only, so a syscall finds its table without `KERNEL`.
static CURRENT: PerCpu<usize> = unsafe { PerCpu::new(0) };
#[unsafe(link_section = ".percpu")]
// SAFETY: in `.percpu`.
/// Where a syscall copies user inputs and stages outputs: room for `rename`'s two paths. A hook never switches while it
/// uses it.
static BUF: PerCpu<[u8; 2 * MAX_BUFFER as usize]> =
    unsafe { PerCpu::new([0; 2 * MAX_BUFFER as usize]) };

/// Every console write and read on `UART0`, the last lock in the order, except panic output, which goes straight to
/// `UART0` so a panic under this lock still prints.
static CONSOLE: Lock<Uart, level::Console> = Lock::new(Uart::new(UART0));

/// `Board::console`: each formatted write holds `CONSOLE` for its whole line.
#[derive(Clone)]
struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // SAFETY: `Board::console`'s writer, which the kernel crate uses holding no lock.
        CONSOLE.lock(&mut unsafe { arch::root() }).write_str(s)
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        // SAFETY: as above.
        CONSOLE.lock(&mut unsafe { arch::root() }).write_fmt(args)
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
        arch::breakpoint_self_test()
    }

    fn read_unmapped(&mut self) {
        // SAFETY: the address is unmapped, so the read takes a data abort, which panics instead of returning.
        unsafe { (UNMAPPED.0 as *const u64).read_volatile() };
    }

    fn violate(&mut self, violation: Violation) {
        /// `ret`, in `.data`.
        static mut DATA_WORD: u32 = 0xd65f_03c0;
        let address = match violation {
            Violation::WriteText => kmain as *const () as usize,
            Violation::ExecuteData => &raw mut DATA_WORD as usize,
            Violation::OverflowStack => &raw const __boot_guard as usize,
        };
        let _ = writeln!(Uart::new(UART0), "wx: {address:#x}");
        match violation {
            // SAFETY: the text is mapped read-only, so the store takes a permission fault, which panics.
            Violation::WriteText => unsafe { (address as *mut u32).write_volatile(0) },
            Violation::ExecuteData => {
                // SAFETY: `.data` is mapped PXN, so the branch takes a permission fault, which panics.
                let data: extern "C" fn() = unsafe { core::mem::transmute(address) };
                data()
            }
            Violation::OverflowStack => _ = recurse(0),
        }
    }

    fn init_heap(&mut self, region: Range<PhysAddr>) {
        let size = (region.end.0 - region.start.0) as usize;
        let mut heap = HEAP.0.lock_leaf();
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
        // SAFETY: a `Board` method, which the kernel crate calls holding no lock.
        let mut root = unsafe { arch::root() };
        let mut kernel = KERNEL.lock(&mut root);
        let (kernel, mut w) = kernel.parts();
        let sched = &mut kernel.sched;
        sched.free_slot().ok_or(Full).and_then(|slot| {
            let stack = FRAMES.lock(&mut w).alloc_contiguous(TASK_STACK_FRAMES);
            let stack = stack.ok_or(Full)?;
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
        // SAFETY: a `Board` method, which the kernel crate calls holding no lock.
        let mut root = unsafe { arch::root() };
        KERNEL.lock(&mut root).sched.block(arch::cpu(), Event::Idle);
        arch::yield_now()
    }

    fn init_cpus(&mut self, frames: &mut FrameAllocator<FRAME_WORDS>) {
        let block = CPU_STACK + (arch::percpu_size() as u64).next_multiple_of(64);
        let bytes = CPUS.load(Relaxed) as u64 * block;
        let blocks = frames.alloc_contiguous(bytes.div_ceil(PAGE as u64) as usize);
        BLOCKS.store(
            blocks.expect("no room for the per-CPU blocks").start.0,
            Relaxed,
        );
        BLOCK.store(block, Relaxed);
        // SAFETY: block 0's area, in fresh frames only this core uses, 64-byte aligned, in RAM; IRQs are still masked and
        // nothing used `PerCpu` yet.
        unsafe { arch::enter_percpu(0, area(0) as usize) };
    }

    fn init_frames(&mut self, frames: FrameAllocator<FRAME_WORDS>) {
        // SAFETY: block 0's stack, which core 0 (on its boot stack) leaves to its idle context.
        let idle = unsafe { arch::new_task(area(0) as usize, idle, 0) };
        // SAFETY: a `Board` method, which the kernel crate calls holding no lock.
        let mut root = unsafe { arch::root() };
        *FRAMES.lock(&mut root) = frames;
        KERNEL
            .lock(&mut root)
            .sched
            .start_cores(CPUS.load(Relaxed), idle);
    }

    fn free_frames(&self) -> usize {
        // SAFETY: a `Board` method, which the kernel crate calls holding no lock.
        let mut root = unsafe { arch::root() };
        FRAMES.lock(&mut root).free_count()
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
            (code, core::iter::once(segment), entry),
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
        // SAFETY: a `Board` method, which the kernel crate calls holding no lock.
        let mut root = unsafe { arch::root() };
        KERNEL.lock(&mut root).sched.count() - net::STARTED.load(Relaxed) as usize
    }

    fn disk(&mut self) -> Option<VirtioBlk> {
        // SAFETY: a `Board` method, which the kernel crate calls holding no lock.
        let alloc = || FRAMES.lock(&mut unsafe { arch::root() }).alloc();
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
        // SAFETY: a `Board` method, which the kernel crate calls holding no lock.
        let mut root = unsafe { arch::root() };
        let mut kernel = KERNEL.lock(&mut root);
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

    fn round_trips(&mut self, n: u64, kind: RoundTrip) {
        static TICKET: Lock<(), level::Kernel> = Lock::new(());
        static TAS: AtomicBool = AtomicBool::new(false);
        #[unsafe(link_section = ".percpu")]
        // SAFETY: in `.percpu`.
        static COUNT: PerCpu<u64> = unsafe { PerCpu::new(0) };
        // SAFETY: a `Board` method, which the kernel crate calls holding no lock.
        let mut root = unsafe { arch::root() };
        for _ in 0..n {
            match kind {
                RoundTrip::Ticket => drop(TICKET.lock_masked(&mut root)),
                RoundTrip::TestAndSet => {
                    while TAS.swap(true, Acquire) {
                        spin_loop();
                    }
                    TAS.store(false, Release);
                }
                RoundTrip::Cpu => _ = black_box(arch::cpu()),
                RoundTrip::PerCpu => COUNT.with(|c| *c += 1),
            }
        }
    }

    fn ipi_round_trips(&mut self, n: u64) {
        // An SGI to a core that has not set up its GIC yet could be lost.
        while ONLINE.load(Acquire) < CPUS.load(Relaxed) {
            spin_loop();
        }
        for i in 1..=n {
            send(1, PING_SGI);
            while PONGS.load(Acquire) < i {
                arch::irq::window();
            }
        }
    }

    fn add_locked(&mut self, n: u64) -> u64 {
        static COUNT: Lock<u64, level::Kernel> = Lock::new(0);
        // SAFETY: a `Board` method, which the kernel crate calls holding no lock.
        let mut root = unsafe { arch::root() };
        for _ in 0..n {
            *COUNT.lock(&mut root) += 1;
        }
        *COUNT.lock(&mut root)
    }

    fn start_cpus(&mut self, smp_test: bool) {
        SMP_TEST.store(smp_test, Relaxed);
        ONLINE.store(1, Relaxed);
        // Core 1 starts the rest, as a tree, so boot pays one call.
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
        TICKED.load(Relaxed)
    }

    fn online_cpus(&self) -> usize {
        ONLINE.load(Acquire)
    }

    fn contended(&self) -> [u32; kernel::LOCK_LEVELS.len()] {
        let processes = PROCESSES.iter().map(|p| p.lock.contended());
        [
            processes.fold(0, u32::wrapping_add),
            KERNEL.contended(),
            net::contended(),
            FRAMES.contended(),
            CONSOLE.contended(),
            HEAP.0.contended(),
        ]
    }

    fn counter_us(&self) -> u64 {
        arch::uptime_us()
    }

    fn hold_kernel(&mut self, start_us: u64, waiters: u32) -> bool {
        // SAFETY: a `Board` method, which the kernel crate calls holding no lock.
        let mut root = unsafe { arch::root() };
        let _kernel = KERNEL.lock(&mut root);
        if arch::uptime_us() >= start_us {
            return false;
        }
        let from = KERNEL.contended();
        while KERNEL.contended().wrapping_sub(from) < waiters {
            spin_loop();
        }
        true
    }

    fn report_speculation(&mut self) {
        arch::install_vectors(conduit());
        records()[0].store(arch::record_speculation(conduit()), Release);
        let spec = loop {
            if let Some(spec) = arch::speculation(records()) {
                break spec;
            }
            spin_loop();
        };
        let _ = writeln!(self.console, "spec: {spec}");
    }
}

/// The conduit `kmain` stored in `CONDUIT`.
fn conduit() -> Option<Conduit> {
    match CONDUIT.load(Relaxed) {
        1 => Some(Conduit::Hvc),
        2 => Some(Conduit::Smc),
        _ => None,
    }
}

unsafe extern "C" {
    static __kernel_start: u8;
    static __text_end: u8;
    static __rodata_end: u8;
    static __boot_guard: u8;
    static __stacks: u8;
    static __kernel_end: u8;
}

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let entry_us = arch::uptime_us();
    // SAFETY: RAM base is RAM, read with the MMU off as Device memory; we only read the 8-byte FDT header there.
    let header = unsafe { slice::from_raw_parts(DTB.0 as *const u8, 8) };
    let size = dtb::total_size(header).expect("no DTB at RAM base");
    let stacks = PhysAddr(&raw const __stacks as u64);
    let dtb_range = DTB..PhysAddr(DTB.0 + size as u64);
    assert!(
        dtb_range.end.0 <= stacks.0,
        "the DTB reaches the boot stacks"
    );
    let map = KernelMap {
        device: PhysAddr(0),
        ram: PhysAddr(GIB),
        dtb: dtb_range.clone(),
        image: PhysAddr(&raw const __kernel_start as u64),
        text_end: PhysAddr(&raw const __text_end as u64),
        rodata_end: PhysAddr(&raw const __rodata_end as u64),
        guard: PhysAddr(&raw const __boot_guard as u64),
    };
    // SAFETY: core 0, MMU off, before any atomic RMW; MMIO is in GiB 0, the image, stacks and DTB in RAM in GiB 1.
    unsafe { arch::enable_mmu(&map) };

    // SAFETY: the magic matched, so QEMU loaded `size` bytes of DTB here and nothing writes them.
    let blob = unsafe { slice::from_raw_parts(DTB.0 as *const u8, size) };

    let dtb = Dtb::new(blob).expect("bad DTB");
    let method = match dtb.psci_method() {
        Some("hvc") => 1,
        Some("smc") => 2,
        _ => 0,
    };
    CONDUIT.store(method, Relaxed);
    // The choice waits for `report_speculation`, after the `boot:` line and before any EL0 code on this core.
    arch::install_boot_vectors();
    arch::timer::allow_user_counter();
    // The cores' table (`CPU_TABLE`), in one DTB walk for the cores: the boot core first.
    let table = &raw const __kernel_end as *mut u64;
    let boot = arch::mpidr();
    // SAFETY: RAM right after the image, reserved with it below and used by nothing else; a word per core and two per
    // redistributor region.
    unsafe { table.write(boot) };
    let mut len = 1;
    let cpus = dtb.cpus(|mpidr| {
        if mpidr != boot {
            // SAFETY: as above.
            unsafe { table.wrapping_add(len).write(mpidr) };
            len += 1;
        }
    });
    assert_eq!(len, cpus, "the boot core's MPIDR is not the DTB's once");
    let gic = dtb.gic().expect("no GICv3 in DTB");
    let dist = gic.distributor().expect("no GICv3 distributor");
    for (base, size) in gic.redistributors() {
        // SAFETY: as above.
        unsafe { (table.wrapping_add(len) as *mut [u64; 2]).write([base.0, size / REDIST_STRIDE]) };
        len += 2;
    }
    let records = len;
    for _ in 0..cpus.div_ceil(2) {
        // SAFETY: as above; then a zeroed `u32` per core for its speculation record (`records`).
        unsafe { table.wrapping_add(len).write(0) };
        len += 1;
    }
    CPU_TABLE.store(table as u64, Relaxed);
    CPUS.store(cpus, Relaxed);
    REDIST_REGIONS.store((records - cpus) / 2, Relaxed);
    GIC_DIST.store(dist.0, Relaxed);
    for gib in gic_gibs() {
        // SAFETY: core 0, before any secondary or process: a GiB of the DTB's GIC registers, not yet mapped.
        unsafe { arch::map_device_gib(PhysAddr(gib * GIB)) };
    }
    let gic_gib = gic.redistributors().map(|r| r.0.0 / GIB);
    let user_end = gic_gib
        .chain([dist.0 / GIB])
        .filter(|&g| g >= USER_BASE / GIB)
        .fold(511, u64::min);
    USER_END.store(user_end * GIB, Relaxed);
    // SAFETY: the DTB's GICv3 distributor, in the device-mapped GiB 0, enabled once, before any CPU interface.
    unsafe { arch::gic::enable(dist) };
    enable_gic_cpu();
    // SAFETY: as above; UART_IRQ is an SPI, routed to this core.
    unsafe { arch::gic::route(dist, UART_IRQ, boot) };
    // SAFETY: as above.
    unsafe { arch::gic::unmask(dist, UART_IRQ) };
    Uart::new(UART0).enable_rx_irq();

    // The cores' table follows the image, reserved with it.
    let image_end = PhysAddr(&raw const __kernel_end as u64 + len as u64 * 8);
    kernel::run(
        &mut QemuVirt {
            console: Console,
            entry_us,
        },
        dtb,
        &[dtb_range, stacks..image_end],
    )
}

/// The cores' table's redistributor regions: base and frame count each.
fn redist_regions() -> impl Iterator<Item = [u64; 2]> {
    let table = CPU_TABLE.load(Relaxed) as *const u64;
    let regions = table.wrapping_add(CPUS.load(Relaxed)) as *const [u64; 2];
    // SAFETY: `kmain` wrote `REDIST_REGIONS` pairs after the `CPUS` MPIDRs before any reader.
    (0..REDIST_REGIONS.load(Relaxed)).map(move |i| unsafe { regions.wrapping_add(i).read() })
}

/// The GiBs of the DTB's redistributor regions past `KERNEL_L1` (QEMU `virt` puts a second region at 256 GiB past
/// 123 cores): EL1-only Device blocks in the boot table and every address space.
fn gic_gibs() -> impl Iterator<Item = u64> {
    redist_regions().flat_map(|[base, frames]| {
        (base / GIB..(base + frames * REDIST_STRIDE).div_ceil(GIB))
            .filter(|&g| g >= KERNEL_ENTRIES as u64)
    })
}

/// The cores' speculation records (`arch::record_speculation`), one per core after the table's regions.
fn records() -> &'static [AtomicU32] {
    let table = CPU_TABLE.load(Relaxed) as *const u64;
    let at = table.wrapping_add(CPUS.load(Relaxed) + 2 * REDIST_REGIONS.load(Relaxed));
    // SAFETY: `kmain` zeroed a `u32` per core there before any reader, and only atomics reach them.
    unsafe { slice::from_raw_parts(at as *const AtomicU32, CPUS.load(Relaxed)) }
}

/// Core `cpu`'s per-CPU area, the top of its block's stack.
fn area(cpu: usize) -> u64 {
    BLOCKS.load(Relaxed) + cpu as u64 * BLOCK.load(Relaxed) + CPU_STACK
}

/// Core `cpu`'s MPIDR affinity, from the table `kmain` wrote before any secondary started.
fn mpidr(cpu: usize) -> u64 {
    let table = CPU_TABLE.load(Relaxed) as *const u64;
    // SAFETY: the table holds `CPUS` MPIDRs, and `cpu` is below `CPUS`.
    unsafe { table.wrapping_add(cpu).read() }
}

/// Core `cpu`'s GICv3 redistributor: frame `cpu` counting through the DTB's regions in order (QEMU `virt` gives
/// redistributors in core order), so no core walks every other core's.
fn redistributor(cpu: usize) -> PhysAddr {
    let mut frame = cpu as u64;
    for [base, frames] in redist_regions() {
        if frame < frames {
            return PhysAddr(base + frame * REDIST_STRIDE);
        }
        frame -= frames;
    }
    panic!("no redistributor for core {cpu}")
}

/// Starts core `cpu` (`mpidr(cpu)`) at `arch::secondary_entry` with its per-CPU area and index, without waiting for it.
fn start_cpu(cpu: usize) {
    let status: i64;
    // SAFETY: CPU_ON starts an off core on its own, unused block; `dsb ish` first completes the stores it reads.
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "hvc #0",
            inlateout("x0") PSCI_CPU_ON => status,
            in("x1") mpidr(cpu),
            in("x2") arch::secondary_entry(),
            in("x3") area(cpu) | (cpu as u64) << 48,
            clobber_abi("C"),
        )
    };
    assert_eq!(status, 0, "PSCI CPU_ON {cpu}");
}

/// A secondary core's first Rust code, from `arch::secondary_entry`: MMU on, on its own stack, IRQs masked. It turns on
/// its GIC CPU interface, timer PPI and reschedule SGI, and becomes its idle context.
#[unsafe(no_mangle)]
extern "C" fn kmain_secondary() -> ! {
    // Core k starts 2k and 2k + 1, so every core is up after about log2 N levels.
    let cpu = arch::cpu();
    (2 * cpu..(2 * cpu + 2).min(CPUS.load(Relaxed))).for_each(start_cpu);
    arch::install_vectors(conduit());
    records()[cpu].store(arch::record_speculation(conduit()), Release);
    arch::timer::allow_user_counter();
    enable_gic_cpu();
    // Its first reschedule: tasks made ready before its GIC was up signalled no one.
    send_sgi(cpu);
    ONLINE.fetch_add(1, Release);
    if SMP_TEST.load(Relaxed) {
        arch::timer::arm(TICK_US);
    }
    idle(0)
}

/// Turns on this core's redistributor and GIC CPU interface and unmasks its timer PPI and SGIs; panics if the
/// redistributor's affinity is not this core's MPIDR.
fn enable_gic_cpu() {
    let cpu = arch::cpu();
    let redist = redistributor(cpu);
    // SAFETY: a frame of the DTB's redistributor regions, device-mapped by `kmain` before any secondary started.
    let affinity = unsafe { arch::gic::affinity(redist) } as u64;
    let mpidr = arch::mpidr();
    assert_eq!(
        affinity,
        mpidr & 0xff_ffff | (mpidr >> 32) << 24,
        "core {cpu}'s redistributor"
    );
    // SAFETY: as above, and `affinity` showed it is this core's; `kmain` enabled the distributor first.
    unsafe { arch::gic::enable_cpu(redist) };
    let irqs = 1 << TIMER_IRQ | 1 << RESCHEDULE_SGI | 1 << PING_SGI;
    // SAFETY: as above.
    unsafe { arch::gic::unmask_local(redist, irqs) };
}

/// A core's idle context: sleeps until an IRQ, whose handler switches to a ready task, if any.
extern "C" fn idle(_: usize) -> ! {
    loop {
        arch::irq::wait();
    }
}

/// Sends `cpu` the reschedule SGI.
fn send_sgi(cpu: usize) {
    send(cpu, RESCHEDULE_SGI);
}

/// Sends core `cpu` SGI `sgi`.
fn send(cpu: usize, sgi: u32) {
    arch::gic::send_sgi(mpidr(cpu), sgi);
}

/// Signals an idle core for each task made ready since the last call, while one is left to signal.
#[inline(always)]
fn kick(sched: &mut Sched, cpu: usize) {
    let woken = sched.take_woken();
    if woken > 0 {
        signal(sched, cpu, woken);
    }
}

/// Signals up to `woken` idle cores other than `cpu`.
#[cold]
#[inline(never)]
fn signal(sched: &mut Sched, cpu: usize, woken: usize) {
    for _ in 0..woken {
        let Some(core) = sched.claim_idle(cpu) else {
            return;
        };
        send_sgi(core);
    }
}

/// Recurses without end, 512 bytes of stack a call.
fn recurse(depth: u64) -> u64 {
    let frame = core::hint::black_box([depth; 64]);
    if frame[0] == u64::MAX {
        return 0;
    }
    recurse(depth + 1) + frame[63]
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
