#![no_std]
#![no_main]

extern crate alloc;

mod uart;
mod virtio_blk;

use core::alloc::{GlobalAlloc, Layout};
use core::arch::global_asm;
use core::fmt::{self, Write};
use core::hint::spin_loop;
use core::ops::Range;
use core::panic::PanicInfo;
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use core::sync::atomic::{AtomicBool, AtomicU64};

use arch::{Guard, Lock, MemoryType, UserAccess, l1_block, user_page};
use dtb::Dtb;
use kernel::console::Line;
use kernel::elf::{Elf, Segment};
use kernel::file;
use kernel::handle::{DUPLICATE, Handles, KILL, MAX_HANDLES, Object, READ, TRANSFER, WAIT, WRITE};
use kernel::mutex::Mutexes;
use kernel::pipe::{self, End, Pipes};
use kernel::syscall::{Call, EAGAIN, EBADF, EFAULT, ENFILE, ENOENT, ENOEXEC, ENOMEM, KILLED};
use kernel::{BLOCK_SIZE, Disk, Event, FRAME_WORDS, Full, Memory, PRIORITIES, Program, Scheduler};
use linked_list_allocator::Heap;
use mm::{Budget, FrameAllocator, PhysAddr};
use mogfs::{Error, Fs, ROOT};
use uart::Uart;
use virtio_blk::VirtioBlk;

/// Panic- and trap-path console; normal output uses the PL011 from the DTB.
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
/// Where an executable's segments may go: below the two stack pages and an unmapped guard page, in one level-3 table.
const IMAGE: Range<u64> = USER_BASE..USER_STACK_TOP - 3 * PAGE as u64;
/// The boot archive (cpio, newc), built by `build.rs` from `crates/user`.
static ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/boot.cpio"));
/// EL1 virtual timer PPI.
const TIMER_IRQ: u32 = 27;
/// PL011 SPI 1 on QEMU `virt`.
const UART_IRQ: u32 = 33;
const TICK_US: u64 = 10_000;

/// GIC CPU interface base, set before the first IRQ can be delivered.
static GIC_CPU: AtomicU64 = AtomicU64::new(0);
/// Set once `Board::disk` handed out the block device.
static DISK_TAKEN: AtomicBool = AtomicBool::new(false);
/// QEMU `virt`'s 32 virtio-mmio transports: the first one's base, and the stride between them.
const VIRTIO: PhysAddr = PhysAddr(0x0a00_0000);
const VIRTIO_STRIDE: u64 = 0x200;
const VIRTIO_COUNT: u64 = 32;
/// Boot context included; a task's slot is its ASID (8 bits).
const MAX_TASKS: usize = 8;
const _: () = assert!(MAX_TASKS <= 256);
/// Kernel stack per task: 16 KiB.
const TASK_STACK_FRAMES: usize = 4;
const MAX_PIPES: usize = 16;
/// Each live mutex has a handle, so the handle tables are the per-process quota.
const MAX_MUTEXES: usize = MAX_TASKS * MAX_HANDLES;

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

/// The big lock over task contexts, free frames, pipes, mutexes, console input and the file system. Every trap hook
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
});

struct Kernel {
    sched: Scheduler<MAX_TASKS>,
    frames: FrameAllocator<FRAME_WORDS>,
    pipes: Pipes<MAX_PIPES>,
    mutexes: Mutexes<MAX_MUTEXES>,
    line: Line,
    fs: Fs<FsDisk>,
    /// `fs` is mounted: boot-spawned processes get its root as handle 3.
    mounted: bool,
}

/// `Board::console` output, a leaf lock; the DTB's PL011 from `enable_mmu` on. Panic, fault, echo and user `write`
/// output go straight to `UART0`, so a panic under this lock still prints.
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

/// `KERNEL.fs`'s disk: `None` until `Board::mount` puts the device in, so the const `Fs::new` builds the static before
/// the device exists; `Io` while there is none.
struct FsDisk(Option<VirtioBlk>);

impl Disk for FsDisk {
    fn read(&mut self, block: u64, bufs: &mut [[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        self.0.as_mut().ok_or(Error::Io)?.read(block, bufs)
    }

    fn write(&mut self, block: u64, bufs: &[[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        self.0.as_mut().ok_or(Error::Io)?.write(block, bufs)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.0.as_mut().ok_or(Error::Io)?.flush()
    }

    fn blocks(&self) -> u64 {
        self.0.as_ref().map_or(0, VirtioBlk::blocks)
    }
}

/// # Safety
/// Trap context (IRQs masked), and `frame` the current task's trap frame. Returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn task_switch(frame: usize) -> usize {
    let sched = &mut Guard::leak(KERNEL.lock_masked()).sched;
    // SAFETY: the caller masked IRQs, and `frame` came from the trap path.
    unsafe { switch(sched, frame) }
}

/// Releases `KERNEL`, once per trap, after the trap exit moved to the frame the hook returned.
///
/// # Safety
/// Trap exit only: every trap hook returns holding `KERNEL` through a leaked guard it no longer uses.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_unlock() {
    // SAFETY: the caller's contract.
    unsafe { KERNEL.unlock() }
}

/// Saves the current task's `frame` and enters the next ready one; returns its frame.
///
/// # Safety
/// IRQs must be masked (trap context), and `frame` the current task's trap frame.
unsafe fn switch(sched: &mut Scheduler<MAX_TASKS>, frame: usize) -> usize {
    let (_, from) = sched.current();
    let next = sched.switch(frame);
    if sched.current().1 != from {
        // SAFETY: `frame` came from the trap path and `next` from the scheduler.
        unsafe { enter(sched, frame, next) };
    }
    next
}

/// Ends the current process with `code`, returns all its frames, and returns the next task's frame.
///
/// # Safety
/// IRQs must be masked (trap context), the current task must be a process, and `frame` its trap frame.
unsafe fn task_exit(kernel: &mut Kernel, frame: usize, code: u64) -> usize {
    let Kernel {
        sched,
        frames,
        pipes,
        mutexes,
        ..
    } = kernel;
    // Before `exit` picks the next task, so a reader or locker this wakes can be it.
    for index in mutexes.release(sched.current().0) {
        sched.wake(Event::Lock(index));
    }
    for object in core::mem::take(sched.handles()).objects() {
        release(sched, frames, pipes, mutexes, object);
    }
    let (asid, l1) = sched.current();
    let (next, stack) = sched.exit(code);
    // SAFETY: `frame` is the exiting process's trap frame and `next` came from the scheduler.
    unsafe { enter(sched, frame, next) };
    arch::flush_asid(asid);
    // Frees the kernel stack this runs on: no core can allocate it until the trap exit has left it and released `KERNEL`.
    // SAFETY: TTBR0 left `l1` above, and its tables hold only this process's frames.
    unsafe { arch::free_space(l1, |f| frames.free(f)) };
    free_stack(frames, stack);
    next
}

/// Blocks the current process on `event` with its `svc` rewound, so the call runs again once woken; returns the next
/// task's frame.
///
/// # Safety
/// IRQs must be masked (trap context), and `frame` the current process's.
unsafe fn block(
    sched: &mut Scheduler<MAX_TASKS>,
    frame: &mut arch::TrapFrame,
    event: Event,
) -> usize {
    frame.restart();
    sched.block(event);
    // SAFETY: the caller masked IRQs, and `frame` is the current process's.
    unsafe { switch(sched, frame as *mut arch::TrapFrame as usize) }
}

/// Drops one handle to `object`: an exited process frees its slot and, as `wait` does, moves its budget to the
/// current task; a pipe wakes its waiters and, once no handle reaches it, frees its page, refunding its creator if that
/// still runs; the last handle to a mutex frees it.
fn release(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    pipes: &mut Pipes<MAX_PIPES>,
    mutexes: &mut Mutexes<MAX_MUTEXES>,
    object: Object,
) {
    let end = match object {
        Object::Pipe(end) => end,
        Object::Mutex(mutex) => return mutexes.close(mutex),
        Object::Process { slot, generation } => {
            let limit = sched.close(slot, generation);
            let held = pipes.charged_to((slot, generation));
            return sched.memory().budget.grow(limit.saturating_sub(held));
        }
        _ => return,
    };
    if let Some((page, (slot, generation))) = pipes.close(end) {
        match sched.budget(slot, generation) {
            Some(budget) => budget.free(frames, page),
            None => frames.free(page),
        }
    }
    sched.wake(Event::Pipe(end.index as usize));
}

/// Creates a pipe whose page is charged to the current process, which gets a handle to each end (read, write).
fn new_pipe(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    pipes: &mut Pipes<MAX_PIPES>,
) -> Result<(u64, u64), i64> {
    let read = pipes.free().ok_or(ENFILE)?;
    let write = End {
        write: true,
        ..read
    };
    let mut handles = *sched.handles();
    let read_handle = handles.insert(Object::Pipe(read), READ | DUPLICATE | TRANSFER)?;
    let write_handle = handles.insert(Object::Pipe(write), WRITE | DUPLICATE | TRANSFER)?;
    let page = sched.memory().budget.alloc(frames).ok_or(ENOMEM)?;
    pipes.create(read, page, (sched.current().0, sched.generation()));
    *sched.handles() = handles;
    Ok((read_handle, write_handle))
}

/// Moves bytes between the user buffer at `ptr` and the pipe `end` reaches; `None` if the caller must wait.
fn pipe_io(pipes: &mut Pipes<MAX_PIPES>, end: End, ptr: u64, len: usize) -> Option<i64> {
    let Some(pipe) = pipes.get(end) else {
        return Some(EBADF);
    };
    // SAFETY: an open pipe's page is identity-mapped RAM that only it uses, never mapped to user space.
    let page = unsafe { &mut *(pipe.page.0 as *mut [u8; pipe::SIZE]) };
    match end.write {
        true => user_bytes(ptr, len).map_or(Some(EFAULT), |data| pipe.write(page, data)),
        false => user_bytes_mut(ptr, len).map_or(Some(EFAULT), |out| pipe.read(page, out)),
    }
}

/// Ends the process in `slot` with `generation`, not the current one, as a fault would, and returns all its frames;
/// 0, or `EBADF` once a newer task took the slot.
fn kill(
    Kernel {
        sched,
        frames,
        pipes,
        mutexes,
        ..
    }: &mut Kernel,
    slot: usize,
    generation: u64,
) -> i64 {
    let (handles, l1, stack, blocked) = match sched.kill(slot, generation) {
        Ok(Some(ended)) => ended,
        Ok(None) => return 0,
        Err(error) => return error,
    };
    let owner = match blocked {
        Some(Event::Lock(index)) => mutexes.owner(index),
        _ => None,
    };
    for index in mutexes.release(slot) {
        sched.wake(Event::Lock(index));
    }
    for object in handles.objects() {
        release(sched, frames, pipes, mutexes, object);
    }
    if let Some(owner) = owner {
        sched.unboost(
            owner,
            |e| matches!(e, Event::Lock(i) if mutexes.owner(i) == Some(owner)),
        );
    }
    arch::flush_asid(slot);
    // SAFETY: the process is not current, so TTBR0 is not `l1`, and its tables hold only its frames.
    unsafe { arch::free_space(l1, |f| frames.free(f)) };
    free_stack(frames, stack);
    0
}

fn free_stack(frames: &mut FrameAllocator<FRAME_WORDS>, stack: PhysAddr) {
    for i in 0..TASK_STACK_FRAMES {
        frames.free(PhysAddr(stack.0 + (i * PAGE) as u64));
    }
}

/// A zeroed frame charged to `budget`.
fn zeroed(frames: &mut FrameAllocator<FRAME_WORDS>, budget: &mut Budget) -> Option<PhysAddr> {
    let page = budget.alloc(frames)?;
    // SAFETY: a fresh frame from the allocator: identity-mapped RAM that nothing else uses.
    unsafe { ptr::write_bytes(page.0 as *mut u8, 0, PAGE) };
    Some(page)
}

/// Maps a zeroed frame at `va` under `l1` with `access`, the frame and any new table charged to `budget`; `None`
/// (the frame refunded) if either is out of budget or frames.
fn map_zeroed(
    frames: &mut FrameAllocator<FRAME_WORDS>,
    budget: &mut Budget,
    l1: PhysAddr,
    va: u64,
    access: UserAccess,
) -> Option<PhysAddr> {
    map_filled(frames, budget, (l1, va, access), (0, &[]))
}

/// As `map_zeroed`, with `bytes` at offset `at` of the frame (`at + bytes.len()` at most a page); only the rest is
/// zeroed.
fn map_filled(
    frames: &mut FrameAllocator<FRAME_WORDS>,
    budget: &mut Budget,
    (l1, va, access): (PhysAddr, u64, UserAccess),
    (at, bytes): (usize, &[u8]),
) -> Option<PhysAddr> {
    let page = budget.alloc(frames)?;
    let base = page.0 as *mut u8;
    let end = at + bytes.len();
    // SAFETY: a fresh frame from the allocator: identity-mapped RAM that nothing else uses, and `end <= PAGE`.
    unsafe { ptr::write_bytes(base, 0, at) };
    // SAFETY: as above; `bytes` is kernel or user memory, never this frame.
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), base.wrapping_add(at), bytes.len()) };
    // SAFETY: as above.
    unsafe { ptr::write_bytes(base.wrapping_add(end), 0, PAGE - end) };
    let leaf = user_page(page, access);
    // SAFETY: `l1` is a process's table built from zeroed frames like these, and `va` is a user address it leaves unmapped;
    // map's `next` only grows, by at most the budget, and budgets stay within RAM, so `va` stays far below 512 GiB.
    if unsafe { arch::map_page(l1, va, leaf, || zeroed(frames, budget)) }.is_none() {
        budget.free(frames, page);
        return None;
    }
    Some(page)
}

/// Maps `pages` zeroed read-write pages at the current process's next map address, charged to its budget; returns
/// their address, or `None` with nothing mapped if the budget or the frames run out.
fn map(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    pages: usize,
) -> Option<u64> {
    let (asid, l1) = sched.current();
    let memory = sched.memory();
    if pages > memory.budget.remaining() {
        return None;
    }
    let start = memory.next;
    let end = start + (pages * PAGE) as u64;
    for va in (start..end).step_by(PAGE) {
        if map_zeroed(frames, &mut memory.budget, l1, va, UserAccess::ReadWrite).is_none() {
            for va in (start..va).step_by(PAGE) {
                // SAFETY: this call mapped `va` under `l1` above.
                let page = unsafe { arch::unmap_page(l1, va) };
                memory.budget.free(frames, page);
            }
            arch::flush_asid(asid);
            return None;
        }
    }
    memory.next = end;
    Some(start)
}

/// Moves SP_EL0, TPIDR_EL0 and TTBR0 from the task that saved `frame` to the scheduler's current task, whose frame is
/// `next`: the boot table keeps ASID 0, a process's level-1 table has ASID = its slot.
///
/// # Safety
/// `frame` and `next` must be trap frames.
unsafe fn enter(sched: &Scheduler<MAX_TASKS>, frame: usize, next: usize) {
    let (slot, space) = sched.current();
    // SAFETY: the caller guarantees both are trap frames.
    unsafe { arch::switch_el0_regs(frame, next) };
    let (table, asid) = match space {
        PhysAddr(0) => (arch::boot_table(), 0),
        table => (table, slot),
    };
    // SAFETY: every space's table holds the kernel blocks, and ASID `slot` is used only by the task in that slot.
    unsafe { arch::set_ttbr0(table, asid) }
}

/// Builds a process from `file`'s `segments` (entered at `entry`): address space, pages and kernel stack, all charged
/// to `budget`, and queues it in the free `slot` with `handles` at `priority`; on failure (`ENOMEM`) returns every
/// frame it took. With `args` (`argc` of them, at most a page), the top stack page holds them and the stack gets a
/// page below it.
fn spawn_process(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    (file, segments, entry): (&[u8], impl Iterator<Item = Segment>, u64),
    mut budget: Budget,
    (slot, handles, priority): ((usize, u64), Handles, u8),
    (args, argc): (&[u8], usize),
) -> Result<(), i64> {
    let l1 = zeroed(frames, &mut budget).ok_or(ENOMEM)?;
    // SAFETY: `l1` is a fresh, zeroed frame.
    unsafe { (l1.0 as *mut [u64; 2]).write(KERNEL_L1) };
    let stack = (|| {
        for segment in segments {
            let data = &file[segment.data];
            // No read-only non-executable access kind yet, so a read-only segment (flags R) maps executable.
            let access = match segment.writable {
                true => UserAccess::ReadWrite,
                false => UserAccess::ReadExecute,
            };
            for offset in (0..segment.size as usize).step_by(PAGE) {
                let va = segment.vaddr + offset as u64;
                let bytes = data.get(offset..).unwrap_or_default();
                let bytes = &bytes[..bytes.len().min(PAGE)];
                let page = map_filled(frames, &mut budget, (l1, va, access), (0, bytes))?;
                if !segment.writable {
                    // SAFETY: `page` is identity-mapped RAM.
                    unsafe { arch::clean_dcache(page.0 as usize, PAGE) };
                }
            }
        }
        let stack_page = USER_STACK_TOP - PAGE as u64;
        let top = (l1, stack_page, UserAccess::ReadWrite);
        map_filled(frames, &mut budget, top, (PAGE - args.len(), args))?;
        if !args.is_empty() {
            map_zeroed(
                frames,
                &mut budget,
                l1,
                stack_page - PAGE as u64,
                UserAccess::ReadWrite,
            )?;
        }
        budget.alloc_contiguous(frames, TASK_STACK_FRAMES)
    })();
    let Some(stack) = stack else {
        // SAFETY: no TTBR0 ever used `l1`, and its tables hold only frames taken above.
        unsafe { arch::free_space(l1, |f| frames.free(f)) };
        return Err(ENOMEM);
    };
    arch::invalidate_icache();
    let at = USER_STACK_TOP - args.len() as u64;
    let x = [argc as u64, at, args.len() as u64];
    // SAFETY: the kernel stack below `stack.end` is fresh and owned by the new process.
    let frame = unsafe { arch::new_user_task(stack.end.0 as usize, entry, at & !15, x) };
    let memory = Memory {
        stack: stack.start,
        budget,
        next: MAP_BASE,
    };
    sched.add(slot, frame, l1, memory, handles, priority);
    Ok(())
}

/// The boot archive's executable at `file` (byte offsets), checked, as `spawn_process` takes it.
fn executable(
    file: Range<usize>,
) -> Result<(&'static [u8], impl Iterator<Item = Segment>, u64), i64> {
    let file = &ARCHIVE[file];
    let elf = Elf::parse(file, IMAGE).ok_or(ENOEXEC)?;
    let entry = elf.entry;
    Ok((file, elf.segments(), entry))
}

/// Queues `executable` from boot context with init's handles, a budget of `budget` frames and `priority`.
fn spawn_init(
    executable: (&[u8], impl Iterator<Item = Segment>, u64),
    budget: usize,
    priority: u8,
) -> Result<(), i64> {
    let mut kernel = KERNEL.lock();
    let Kernel {
        sched,
        frames,
        mounted,
        ..
    } = &mut *kernel;
    sched.free_slot().ok_or(EAGAIN).and_then(|slot| {
        let mut handles = Handles::init(slot.0, slot.1);
        if *mounted {
            handles.insert(Object::Dir(ROOT), READ | WRITE | DUPLICATE | TRANSFER)?;
        }
        let init = (slot, handles, priority);
        spawn_process(
            sched,
            frames,
            executable,
            Budget::new(budget),
            init,
            (&[], 0),
        )
    })
}

/// Spawns the boot archive's executable at `file` with the `len` handles at user address `ptr` and `budget` frames
/// moved from the current process, which gets a handle to the child, at `priority` capped at the current process's
/// own, with the arguments at user address `args`; on failure nothing moves.
fn spawn(
    sched: &mut Scheduler<MAX_TASKS>,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    file: Range<usize>,
    (ptr, len): (u64, usize),
    (budget, priority): (usize, u8),
    args: (u64, usize),
) -> Result<u64, i64> {
    let executable = executable(file)?;
    let args = user_bytes(args.0, args.1).ok_or(EFAULT)?;
    let argc = kernel::syscall::argc(args)?;
    let bytes = user_bytes(ptr, len * 8).ok_or(EFAULT)?;
    let mut list = [0; MAX_HANDLES];
    for (handle, bytes) in list.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *handle = u64::from_le_bytes(*bytes);
    }
    let (mut parent, child) = sched.handles().split(&list[..len])?;
    if budget > sched.memory().budget.remaining() {
        return Err(ENOMEM);
    }
    let (slot, generation) = sched.free_slot().ok_or(EAGAIN)?;
    let process = parent.insert(Object::Process { slot, generation }, WAIT | KILL)?;
    let priority = priority.min(sched.priority());
    let child = ((slot, generation), child, priority);
    spawn_process(
        sched,
        frames,
        executable,
        Budget::new(budget),
        child,
        (args, argc),
    )?;
    sched.memory().budget.shrink(budget);
    *sched.handles() = parent;
    Ok(process)
}

global_asm!(include_str!("user.s"), USER_BASE = const USER_BASE);

unsafe extern "C" {
    static user_counter: u8;
    static user_counter_end: u8;
    static user_intruder: u8;
    static user_intruder_end: u8;
    static user_kernel_reader: u8;
    static user_kernel_reader_end: u8;
    static user_bench: u8;
    static user_bench_end: u8;
    static user_handles: u8;
    static user_handles_end: u8;
    static user_budget: u8;
    static user_budget_end: u8;
}

/// A user program's code and the address it is mapped and starts at.
fn user_program(program: Program) -> (&'static [u8], u64) {
    let (start, end, va) = match program {
        Program::Counter => (
            &raw const user_counter,
            &raw const user_counter_end,
            USER_BASE,
        ),
        Program::Intruder => (
            &raw const user_intruder,
            &raw const user_intruder_end,
            USER_BASE + PAGE as u64,
        ),
        Program::KernelReader => (
            &raw const user_kernel_reader,
            &raw const user_kernel_reader_end,
            USER_BASE + PAGE as u64,
        ),
        Program::SyscallBench => (&raw const user_bench, &raw const user_bench_end, USER_BASE),
        Program::Handles => (
            &raw const user_handles,
            &raw const user_handles_end,
            USER_BASE,
        ),
        Program::Budget => (
            &raw const user_budget,
            &raw const user_budget_end,
            USER_BASE,
        ),
    };
    // SAFETY: `user.s` places each program's bytes between its start and end labels in read-only data.
    let code = unsafe { slice::from_raw_parts(start, end as usize - start as usize) };
    (code, va)
}

/// Whether `allowed` holds for each page of the `len` bytes at `ptr`.
fn user_pages(ptr: u64, len: usize, allowed: fn(u64) -> bool) -> bool {
    let first_page = ptr & !(PAGE as u64 - 1);
    (first_page..ptr + len as u64).step_by(PAGE).all(allowed)
}

/// The `len` bytes at user address `ptr` (in user space unless `len` is 0, checked by `dispatch`) if EL0 may read all
/// of them; valid only until the trap returns.
fn user_bytes<'a>(ptr: u64, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if !user_pages(ptr, len, arch::user_readable) {
        return None;
    }
    // SAFETY: EL0 may read every page of the range, so it is mapped in the current address space, which
    // stays loaded and unchanged until the trap returns (this core runs it and holds `KERNEL`).
    Some(unsafe { slice::from_raw_parts(ptr as *const u8, len) })
}

/// As `user_bytes`, if EL0 may write all of them.
fn user_bytes_mut<'a>(ptr: u64, len: usize) -> Option<&'a mut [u8]> {
    if len == 0 {
        return Some(&mut []);
    }
    if !user_pages(ptr, len, arch::user_writable) {
        return None;
    }
    // SAFETY: as in `user_bytes`; the pages are user memory, which no kernel reference aliases.
    Some(unsafe { slice::from_raw_parts_mut(ptr as *mut u8, len) })
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
    /// The DTB's PL011, `CONSOLE` once the MMU is on.
    uart: Uart,
    console: Console,
    /// GICv2 distributor and CPU interface.
    gic: (PhysAddr, PhysAddr),
    entry_us: u64,
}

impl kernel::Board for QemuVirt {
    type Console = Console;
    type Disk = VirtioBlk;

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

    fn enable_mmu(&mut self) {
        // SAFETY: called at boot with the MMU off, before any atomic RMW; MMIO is in GiB 0, and the image, stack and DTB are in RAM in GiB 1.
        unsafe { arch::enable_mmu(&KERNEL_L1, arch::MAIR) }
        *CONSOLE.lock() = self.uart.clone();
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
        // SAFETY: the DTB's GICv2 distributor, in the device-mapped GiB 0.
        unsafe { arch::gic::unmask(self.gic.0, TIMER_IRQ) };
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
            let memory = Memory {
                stack: stack.start,
                budget: Budget::new(0),
                next: 0,
            };
            sched.add(slot, frame, PhysAddr(0), memory, Handles::new(), 0);
            Ok(())
        })
    }

    fn yield_now(&mut self) {
        arch::yield_now()
    }

    fn run_others(&mut self) {
        KERNEL.lock().sched.block(Event::Idle);
        arch::yield_now()
    }

    fn init_frames(&mut self, frames: FrameAllocator<FRAME_WORDS>) {
        KERNEL.lock().frames = frames;
    }

    fn free_frames(&self) -> usize {
        KERNEL.lock().frames.free_count()
    }

    fn spawn_user(&mut self, program: Program, budget: usize) -> Result<(), i64> {
        let (code, entry) = user_program(program);
        let segment = Segment {
            vaddr: entry,
            data: 0..code.len(),
            size: code.len() as u64,
            writable: false,
        };
        spawn_init((code, [segment].into_iter(), entry), budget, 0)
    }

    fn spawn_archived(&mut self, name: &str, budget: usize) -> Result<(), i64> {
        let file = kernel::cpio::find(ARCHIVE, name.as_bytes()).ok_or(ENOENT)?;
        spawn_init(executable(file)?, budget, PRIORITIES - 1)
    }

    fn tasks(&self) -> usize {
        KERNEL.lock().sched.count()
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
}

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let entry_us = arch::uptime_us();
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
    let uart = dtb.uart().expect("no PL011 in DTB");
    let gic = dtb.gic().expect("no GICv2 in DTB");
    GIC_CPU.store(gic.1.0, Relaxed);
    // SAFETY: the DTB's GICv2 registers, in the device-mapped GiB 0.
    unsafe { arch::gic::enable(gic.0, gic.1) };
    // SAFETY: as above.
    unsafe { arch::gic::unmask(gic.0, UART_IRQ) };
    Uart::new(UART0).enable_rx_irq();

    kernel::run(
        &mut QemuVirt {
            uart: Uart::new(uart),
            console: Console,
            gic,
            entry_us,
        },
        dtb,
        &[image, dtb_range],
    )
}

/// # Safety
/// IRQs must be masked (trap context), as `task_switch` requires; returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_irq(frame: usize) -> usize {
    let Kernel { sched, line, .. } = Guard::leak(KERNEL.lock_masked());
    let cpu = PhysAddr(GIC_CPU.load(Relaxed));
    // SAFETY: IRQs are delivered only after `kmain` stored the DTB's GIC CPU interface.
    let iar = unsafe { arch::gic::ack(cpu) };
    let tick = iar == TIMER_IRQ;
    if tick {
        arch::timer::arm(TICK_US);
    } else if iar == UART_IRQ {
        let mut uart = Uart::new(UART0);
        while let Some(byte) = uart.get() {
            if line.push(byte, |echo| uart.write(echo)) {
                sched.wake(Event::Console);
            }
        }
    }
    // SAFETY: as above.
    unsafe { arch::gic::eoi(cpu, iar) };
    if !tick {
        return frame;
    }
    // SAFETY: the caller masked IRQs, and `frame` came from the trap path.
    unsafe { switch(sched, frame) }
}

/// # Safety
/// Trap context (IRQs masked), and `frame` the current process's. Returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_syscall(frame: &mut arch::TrapFrame) -> usize {
    let kernel = Guard::leak(KERNEL.lock_masked());
    let Kernel {
        sched,
        frames,
        pipes,
        mutexes,
        line,
        fs,
        ..
    } = kernel;
    let args = frame.x.first_chunk().unwrap();
    frame.x[0] = match kernel::syscall::dispatch(frame.x[8], args, sched.handles()) {
        Ok(Call::Exit(code)) => {
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            return unsafe { task_exit(kernel, frame as *mut arch::TrapFrame as usize, code) };
        }
        Ok(Call::Write { ptr, len }) => match user_bytes(ptr, len) {
            Some(bytes) => {
                Uart::new(UART0).write(bytes);
                len as u64
            }
            None => EFAULT as u64,
        },
        Ok(Call::Read { ptr, len }) => match user_bytes_mut(ptr, len).map(|out| line.read(out)) {
            None => EFAULT as u64,
            Some(Some(n)) => n as u64,
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            Some(None) => return unsafe { block(sched, frame, Event::Console) },
        },
        Ok(Call::Pipe { end, ptr, len }) => match pipe_io(pipes, end, ptr, len) {
            Some(moved) => {
                if moved > 0 {
                    sched.wake(Event::Pipe(end.index as usize));
                }
                moved as u64
            }
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            None => return unsafe { block(sched, frame, Event::Pipe(end.index as usize)) },
        },
        Ok(Call::NewPipe) => match new_pipe(sched, frames, pipes) {
            Ok((read, write)) => {
                frame.x[1] = write;
                read
            }
            Err(error) => error as u64,
        },
        Ok(Call::Wait { slot, generation }) => match sched.reap(slot, generation) {
            Ok(Some((code, limit))) => {
                // Pipes it created that are still open keep their page; a repeated `wait` gets a limit of 0.
                let held = pipes.charged_to((slot, generation));
                sched.memory().budget.grow(limit.saturating_sub(held));
                code
            }
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            Ok(None) => return unsafe { block(sched, frame, Event::Exit(slot)) },
            Err(error) => error as u64,
        },
        Ok(Call::Dup { handle, object }) => {
            match object {
                Object::Pipe(end) => pipes.open(end),
                Object::Mutex(mutex) => mutexes.open(mutex),
                _ => {}
            }
            handle
        }
        Ok(Call::Close(object)) => {
            release(sched, frames, pipes, mutexes, object);
            0
        }
        Ok(Call::Map { pages }) => map(sched, frames, pages).unwrap_or(ENOMEM as u64),
        Ok(Call::File {
            inode,
            write: true,
            offset,
            ptr,
            len,
        }) => user_bytes(ptr, len)
            .ok_or(EFAULT)
            .and_then(|data| fs.write(inode, offset, data).map_err(file::errno))
            .map_or_else(|error| error as u64, |()| len as u64),
        Ok(Call::File {
            inode,
            offset,
            ptr,
            len,
            ..
        }) => user_bytes_mut(ptr, len)
            .ok_or(EFAULT)
            .and_then(|buf| fs.read(inode, offset, buf).map_err(file::errno))
            .map_or_else(|error| error as u64, |n| n as u64),
        Ok(Call::Open {
            dir,
            ptr,
            len,
            flags,
            rights,
        }) => user_bytes(ptr, len)
            .ok_or(EFAULT)
            .and_then(|path| match dir {
                Object::Dir(dir) => file::open(fs, dir, path, flags),
                _ => kernel::cpio::find(ARCHIVE, path)
                    .map(|file| Object::File {
                        start: file.start,
                        end: file.end,
                    })
                    .ok_or(ENOENT),
            })
            .and_then(|object| sched.handles().insert(object, rights))
            .unwrap_or_else(|error| error as u64),
        Ok(Call::Mkdir { dir, ptr, len }) => user_bytes(ptr, len)
            .ok_or(EFAULT)
            .and_then(|path| file::mkdir(fs, dir, path))
            .map_or_else(|error| error as u64, |()| 0),
        Ok(Call::Readdir {
            dir,
            ptr,
            len,
            start,
        }) => user_bytes_mut(ptr, len)
            .ok_or(EFAULT)
            .and_then(|out| match dir {
                Object::Dir(dir) => file::readdir(fs, dir, start, out),
                _ => file::list_archive(ARCHIVE, start, out),
            })
            .map_or_else(|error| error as u64, |n| n as u64),
        Ok(Call::Unlink { dir, ptr, len }) => user_bytes(ptr, len)
            .ok_or(EFAULT)
            .and_then(|path| {
                let held = |i| sched.holds(|o| o == Object::Dir(i) || o == Object::Node(i));
                file::unlink(fs, dir, path, held)
            })
            .map_or_else(|error| error as u64, |()| 0),
        Ok(Call::Rename { from, to }) => user_bytes(from.1, from.2)
            .zip(user_bytes(to.1, to.2))
            .ok_or(EFAULT)
            .and_then(|(f, t)| file::rename(fs, (from.0, f), (to.0, t)))
            .map_or_else(|error| error as u64, |()| 0),
        Ok(Call::Sync) => fs
            .commit()
            .map_or_else(|error| file::errno(error) as u64, |()| 0),
        Ok(Call::Spawn {
            file,
            ptr,
            len,
            budget,
            priority,
            args,
            args_len,
        }) => spawn(
            sched,
            frames,
            file,
            (ptr, len.into()),
            (budget, priority),
            (args, args_len.into()),
        )
        .unwrap_or_else(|error| error as u64),
        Ok(Call::NewMutex) => match mutexes.create() {
            Some(mutex) => sched
                .handles()
                .insert(Object::Mutex(mutex), DUPLICATE | TRANSFER)
                .unwrap_or_else(|error| {
                    mutexes.close(mutex);
                    error as u64
                }),
            None => ENFILE as u64,
        },
        Ok(Call::Lock(mutex)) => match mutexes.lock(mutex, sched.current().0) {
            Ok(None) => 0,
            Ok(Some(owner)) => {
                sched.boost(owner);
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                return unsafe { block(sched, frame, Event::Lock(mutex.index as usize)) };
            }
            Err(error) => error as u64,
        },
        Ok(Call::Unlock(mutex)) => {
            let slot = sched.current().0;
            match mutexes.unlock(mutex, slot) {
                Ok(()) => {
                    // With no waiter woken, the caller's boost is unchanged and nothing new is ready.
                    if sched.wake(Event::Lock(mutex.index as usize)) {
                        sched.unboost(
                            slot,
                            |e| matches!(e, Event::Lock(i) if mutexes.owner(i) == Some(slot)),
                        );
                        if sched.outranked() {
                            frame.x[0] = 0;
                            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                            return unsafe {
                                switch(sched, frame as *mut arch::TrapFrame as usize)
                            };
                        }
                    }
                    0
                }
                Err(error) => error as u64,
            }
        }
        Ok(Call::Kill { slot, generation })
            if (slot, generation) == (sched.current().0, sched.generation()) =>
        {
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            return unsafe { task_exit(kernel, frame as *mut arch::TrapFrame as usize, KILLED) };
        }
        Ok(Call::Kill { slot, generation }) => kill(kernel, slot, generation) as u64,
        Err(error) => error as u64,
    };
    frame as *mut arch::TrapFrame as usize
}

/// # Safety
/// Trap context (IRQs masked), and `frame` the current process's. Returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_user_fault(frame: usize, ec: u64, far: u64) -> usize {
    let kernel = Guard::leak(KERNEL.lock_masked());
    let (slot, _) = kernel.sched.current();
    let _ = writeln!(Uart::new(UART0), "fault: {slot} ec={ec:#x} far={far:#x}");
    // SAFETY: the caller masked IRQs; `frame` is the current process's.
    unsafe { task_exit(kernel, frame, KILLED) }
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
