//! Process construction: address spaces, ELF and asm programs, spawn and its arguments.

use core::arch::global_asm;
use core::mem::MaybeUninit;
use core::ops::{Deref, Range};
use core::ptr;
use core::slice;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::{Acquire, Relaxed};

use arch::{Lock, UserAccess, user_page};
use kernel::elf::{Elf, Segment};
use kernel::handle::{
    CONNECT, DUPLICATE, Handles, KILL, LISTEN, MAX_HANDLES, Object, READ, Rights, TRANSFER, Table,
    WAIT, WRITE,
};
use kernel::syscall::{EAGAIN, EFAULT, ENOBUFS, ENOEXEC, ENOMEM, MAX_BUFFER, MAX_MAP};
use kernel::{FRAME_WORDS, Process, Program};
use lock_order::{self as level, LockAfter, W};
use mm::{Budget, FrameAllocator, MAX_FRAMES, PhysAddr};
use mogfs::ROOT;

use crate::usermem::copy_in;
use crate::{
    ARCHIVE, FRAMES, IMAGE, KERNEL, KERNEL_ENTRIES, Kernel, MAP_BASE, MAX_PROCESSES, Nospec, PAGE,
    Sched, TASK_STACK_FRAMES, USER_BASE, USER_END, USER_STACK_TOP, gic_gibs,
};

/// What a process index has outside `KERNEL`: its lock, budget, live thread count and handle table, each starting a
/// 128-byte line (the M4 host's) so a sibling's charge or lookup does not bounce the lock's. An index's entry serves one
/// process from its spawn until its release frees the index.
#[repr(C)]
pub(crate) struct ProcessEntry {
    pub(crate) lock: Line<Lock<Process, level::Process>>,
    pub(crate) budget: Line<Budget>,
    /// Its live threads: raised only by its own `thread` call, lowered under `KERNEL` (release) when one ends.
    pub(crate) threads: Line<AtomicU32>,
    pub(crate) handles: Line<Table>,
}

impl ProcessEntry {
    /// Whether the calling thread is its process's only one. Then nothing else writes the process's table or reaches
    /// its lock's data (a sibling that ended released the lock before the end that lowered the count), so the call
    /// may skip that lock and the recheck of its lookups.
    #[inline(always)]
    pub(crate) fn alone(&self) -> bool {
        self.threads.load(Acquire) == 1
    }
}

#[repr(align(128))]
pub(crate) struct Line<T>(T);

impl<T> Deref for Line<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

pub(crate) static PROCESSES: [ProcessEntry; MAX_PROCESSES] = [const {
    ProcessEntry {
        lock: Line(Lock::new(Process { next: 0 })),
        budget: Line(Budget::new(0)),
        threads: Line(AtomicU32::new(0)),
        handles: Line(Table::new()),
    }
}; MAX_PROCESSES];

/// The process at `index` before its spawn publishes it, filled in under `KERNEL` by that spawn alone: its first `map`
/// at `next`, `budget` frames (nothing charged), and `handles`.
///
/// # Safety
/// `index` is allocated to the calling spawn (`free_process`, under `KERNEL`, which it holds) and not yet added, so no
/// handle names it and no thread runs in it.
unsafe fn start_entry(index: usize, next: u64, budget: usize, handles: &Handles) {
    let entry = &PROCESSES[index];
    // SAFETY: the caller's contract: nothing else reaches the unpublished process's data.
    let process = unsafe { entry.lock.unshared() };
    process.next = next;
    entry.threads.store(1, Relaxed);
    entry.budget.reset(budget);
    entry.handles.commit(process, handles);
}

/// Returns a thread's kernel stack at `stack` to `frames` (its process's budget refunded apart, under `KERNEL`).
pub(crate) fn free_stack(frames: &mut FrameAllocator<FRAME_WORDS>, stack: PhysAddr) {
    for i in 0..TASK_STACK_FRAMES {
        frames.free(PhysAddr(stack.0 + (i * PAGE) as u64));
    }
}

/// `page`, zeroed.
fn zeroed(page: PhysAddr) -> PhysAddr {
    // SAFETY: a fresh frame from the allocator: identity-mapped RAM that nothing else uses.
    unsafe { ptr::write_bytes(page.0 as *mut u8, 0, PAGE) };
    page
}

/// Maps `page` at `va` under `l1` with `access`, holding `bytes` at offset `at` (`at + bytes.len()` at most a page) and
/// zeroes elsewhere, any new table from `take`.
fn map_filled(
    (page, take): (PhysAddr, &mut impl FnMut() -> PhysAddr),
    (l1, va, access): (PhysAddr, u64, UserAccess),
    (at, bytes): (usize, &[u8]),
) {
    let base = page.0 as *mut u8;
    let end = at + bytes.len();
    if bytes.is_empty() {
        zeroed(page);
    } else {
        // SAFETY: a fresh frame from the allocator: identity-mapped RAM that nothing else uses, and `end <= PAGE`.
        unsafe { ptr::write_bytes(base, 0, at) };
        // SAFETY: as above; `bytes` is kernel memory, never this frame.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), base.wrapping_add(at), bytes.len()) };
        // SAFETY: as above.
        unsafe { ptr::write_bytes(base.wrapping_add(end), 0, PAGE - end) };
    }
    // SAFETY: `l1` is a process's table built from zeroed frames like these, and `va` is a user address it leaves
    // unmapped, from 4 GiB up to `USER_END`, below every GiB a kernel block maps.
    let mapped =
        unsafe { arch::map_page(l1, va, user_page(page, access), || Some(zeroed(take()))) };
    mapped.expect("no kernel block below USER_END");
}

/// Maps `pages` zeroed read-write pages at the current process's next map address, charged to its budget, taking the
/// frames and any new tables in one pass of the bitmap; returns their address, or `None` with nothing mapped if the
/// budget or the frames run out or the pages would reach `USER_END`. Under the process's lock only, unless the caller
/// is its only thread.
#[inline(never)]
pub(crate) fn map(
    entry: &ProcessEntry,
    root: &mut W<'_, level::Unlocked>,
    pages: usize,
) -> Option<u64> {
    if entry.alone() {
        // SAFETY: the caller is its process's only thread, so nothing else reaches the process's data (`alone`).
        return map_pages(entry, unsafe { entry.lock.unshared() }, root, pages);
    }
    let mut guard = entry.lock.lock_masked(root);
    let (process, mut w) = guard.parts();
    map_pages(entry, process, &mut w, pages)
}

/// `map` with the process's data in hand, `w` the witness of the locks held.
#[inline(always)]
fn map_pages<P>(
    entry: &ProcessEntry,
    process: &mut Process,
    w: &mut W<'_, P>,
    pages: usize,
) -> Option<u64>
where
    level::Frames: LockAfter<P>,
{
    let l1 = arch::user_table();
    let start = process.next;
    let end = start + (pages * PAGE) as u64;
    if end > USER_END.load(Relaxed) {
        return None;
    }
    // SAFETY: `l1` is the current process's table (TTBR0 during its syscall), changed only under its lock, and
    // `start..end` is page-aligned user space below `USER_END`.
    let count = pages + unsafe { arch::missing_tables(l1, start, end) };
    if !entry.budget.charge(count) {
        return None;
    }
    let mut taken = [MaybeUninit::uninit(); MAP_FRAMES];
    if !FRAMES
        .lock_masked(w)
        .alloc_many(count, |i, frame| _ = taken[i].write(frame))
    {
        entry.budget.refund(count);
        return None;
    }
    let mut taken = taken[..count].iter();
    // SAFETY: `alloc_many` wrote the first `count`.
    let mut take = || unsafe { taken.next().expect("counted").assume_init() };
    for va in (start..end).step_by(PAGE) {
        let page = take();
        map_filled((page, &mut take), (l1, va, UserAccess::ReadWrite), (0, &[]));
    }
    process.next = end;
    Some(start)
}

/// Frames a `map` takes at most: its pages, and a level-2 and level-3 table for each of the two GiBs and regions it
/// may touch.
const MAP_FRAMES: usize = (MAX_MAP / PAGE as u64) as usize + 4;

/// Builds a process from `file`'s `segments` (entered at `entry`): address space, pages and its first thread's kernel
/// stack, all charged with `sockets` more frames to a budget of `budget` frames and taken in one pass of the bitmap, its
/// first `map` at `next`,
/// and queues it at the free index `process`, its thread in the free `slot`, with `handles` at `priority`; on failure
/// (`ENOMEM`) takes nothing. With `args` (`argc` of them, at most a page), the top stack page holds them and the stack
/// gets a page below it.
fn spawn_process(
    (sched, w): (&mut Sched, &mut W<'_, level::Kernel>),
    (file, segments, entry): (&[u8], impl Iterator<Item = Segment> + Clone, u64),
    (budget, sockets, next): (usize, usize, u64),
    (process, slot, handles, priority): ((usize, u64), (usize, u64), &Handles, u8),
    (args, argc): (&[u8], usize),
) -> Result<(), i64> {
    let pages = segments
        .clone()
        .map(|s| s.size.div_ceil(PAGE as u64) as usize);
    // The level-1 table, one level-2 and one level-3 table (every user page is in one region), and the stack pages.
    let count = 3 + pages.sum::<usize>() + 1 + !args.is_empty() as usize;
    debug_assert!(count <= SPAWN_FRAMES);
    if count + TASK_STACK_FRAMES + sockets > budget {
        return Err(ENOMEM);
    }
    let mut frames = FRAMES.lock_masked(w);
    let stack = frames.alloc_contiguous(TASK_STACK_FRAMES).ok_or(ENOMEM)?;
    // The list of the other frames lies at the bottom of the new kernel stack, which nothing uses until the thread's
    // first frame is written at its top, below which the list ends.
    // SAFETY: fresh identity-mapped frames nothing else references; `SPAWN_FRAMES` fit below the first frame.
    let taken = unsafe { slice::from_raw_parts_mut(stack.start.0 as *mut PhysAddr, count) };
    if !frames.alloc_many(count, |i, frame| taken[i] = frame) {
        (stack.start.0..stack.end.0)
            .step_by(PAGE)
            .for_each(|f| frames.free(PhysAddr(f)));
        return Err(ENOMEM);
    }
    drop(frames);
    let mut taken = taken.iter().copied();
    let mut take = || taken.next().expect("counted");
    let l1 = zeroed(take());
    // SAFETY: `l1` is a fresh frame; the boot table's kernel entries stay fixed after `kmain`.
    unsafe {
        ptr::copy_nonoverlapping(
            arch::boot_table().0 as *const u64,
            l1.0 as *mut u64,
            KERNEL_ENTRIES,
        )
    };
    let boot = arch::boot_table().0 as *const u64;
    for gib in gic_gibs().map(|g| g as usize) {
        // SAFETY: a boot-table entry `kmain` added, below 512, fixed after `kmain`.
        let entry = unsafe { boot.wrapping_add(gib).read() };
        // SAFETY: as above, the fresh frame.
        unsafe { (l1.0 as *mut u64).wrapping_add(gib).write(entry) };
    }
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
            let page = take();
            map_filled((page, &mut take), (l1, va, access), (0, bytes));
            if !segment.writable {
                // SAFETY: `page` is identity-mapped RAM.
                unsafe { arch::sync_icache(page.0 as usize, PAGE) };
            }
        }
    }
    let stack_page = USER_STACK_TOP - PAGE as u64;
    let top = (l1, stack_page, UserAccess::ReadWrite);
    let page = take();
    map_filled((page, &mut take), top, (PAGE - args.len(), args));
    if !args.is_empty() {
        let below = (l1, stack_page - PAGE as u64, UserAccess::ReadWrite);
        let page = take();
        map_filled((page, &mut take), below, (0, &[]));
    }
    arch::icache_synced();
    let at = USER_STACK_TOP - args.len() as u64;
    let x = [argc as u64, at, args.len() as u64];
    // SAFETY: the kernel stack below `stack.end` is fresh and owned by the new process.
    let frame = unsafe { arch::new_user_task(stack.end.0 as usize, entry, (at & !15, 0), x) };
    // SAFETY: `process` came from `free_process` under the `KERNEL` this spawn holds, and is added only below.
    unsafe { start_entry(process.0, next, budget, handles) };
    assert!(
        PROCESSES[process.0]
            .budget
            .charge(count + TASK_STACK_FRAMES + sockets)
    );
    sched.add_process(process, l1);
    sched.add(slot, process.0, (frame, stack.start), priority);
    // Its one handle: the spawner's, or init's own.
    let (index, generation) = process;
    sched.held(Object::Process { index, generation });
    Ok(())
}

const _: () = assert!(
    USER_BASE >> 21 == (USER_STACK_TOP - 1) >> 21,
    "one level-3 table"
);

/// Frames a spawn takes besides its kernel stack, at most: three tables and every page from `USER_BASE` to the stack top.
const SPAWN_FRAMES: usize = 3 + ((USER_STACK_TOP - USER_BASE) / PAGE as u64) as usize;
const _: () = assert!(
    SPAWN_FRAMES * size_of::<PhysAddr>() + size_of::<arch::TrapFrame>() <= TASK_STACK_FRAMES * PAGE,
    "the spawn's frame list fits below its first trap frame"
);

/// The boot archive's executable at `file` (byte offsets), checked, as `spawn_process` takes it.
pub(crate) fn executable(
    file: Range<usize>,
) -> Result<(&'static [u8], impl Iterator<Item = Segment> + Clone, u64), i64> {
    let file = &ARCHIVE[file];
    let elf = Elf::parse(file, IMAGE).ok_or(ENOEXEC)?;
    let entry = elf.entry;
    Ok((file, elf.segments(), entry))
}

/// Queues `executable` from boot context with init's handles, a budget of `budget` frames, its first `map` at `next`,
/// `priority` and `args`.
pub(crate) fn spawn_init(
    executable: (&[u8], impl Iterator<Item = Segment> + Clone, u64),
    (budget, next): (usize, u64),
    priority: u8,
    archive: Rights,
    args: &[u8],
) -> Result<(), i64> {
    let argc = kernel::syscall::argc(args)?;
    // SAFETY: called from `Board` methods, which the kernel crate calls holding no lock.
    let mut root = unsafe { arch::root() };
    let mut kernel = KERNEL.lock(&mut root);
    let (kernel, mut w) = kernel.parts();
    let Kernel {
        sched,
        mounted,
        opens,
        ..
    } = kernel;
    let (process, slot) = sched.free_process().zip(sched.free_slot()).ok_or(EAGAIN)?;
    let mut handles = Handles::init(process.0, process.1, archive);
    if *mounted {
        handles.insert(Object::Dir(ROOT), READ | WRITE | DUPLICATE | TRANSFER)?;
    }
    if crate::net::STARTED.load(Relaxed) {
        let rights = CONNECT | LISTEN | DUPLICATE | TRANSFER;
        handles.insert(Object::NetStack, rights)?;
    }
    let init = (process, slot, &handles, priority);
    spawn_process(
        (sched, &mut w),
        executable,
        (budget, 0, next),
        init,
        (args, argc),
    )?;
    handles.objects().for_each(|o| opens.open(o));
    crate::kick(sched, arch::cpu());
    Ok(())
}

/// Spawns the boot archive's executable at `file` with the `len` handles at user address `ptr` and `budget` frames
/// moved from `cpu`'s current process, which gets a handle to the child, at `priority` capped at that process's own,
/// with the arguments at user address `args`, both copied in through `buf`; on failure nothing moves. The parent's
/// table (`table`, under its lock) changes last, once the child is complete.
pub(crate) fn spawn(
    (sched, w, cpu): (&mut Sched, &mut W<'_, level::Kernel>, usize),
    (table, parent): (&Table, &mut Process),
    buf: &mut [u8],
    file: Range<usize>,
    (ptr, len): (u64, usize),
    (budget, priority): (usize, u8),
    args: (u64, usize),
) -> Result<u64, i64> {
    let executable = executable(file)?;
    let (args_buf, list_buf) = buf.split_at_mut(MAX_BUFFER as usize);
    let args = copy_in(args.0, args.1, args_buf).ok_or(EFAULT)?;
    let argc = kernel::syscall::argc(args)?;
    let bytes = copy_in(ptr, len * 8, list_buf).ok_or(EFAULT)?;
    let mut list = [0; MAX_HANDLES];
    for (handle, bytes) in list.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *handle = u64::from_le_bytes(*bytes);
    }
    // `len` is at most `MAX_HANDLES` (`dispatch`); the modulo keeps the slice in bounds on a mispredicted path too.
    let (mut rest, child) =
        (table.snapshot(parent)).split::<Nospec>(&list[..len % (MAX_HANDLES + 1)])?;
    let (process, slot) = sched.free_process().zip(sched.free_slot()).ok_or(EAGAIN)?;
    let (index, generation) = process;
    let handle = rest.insert(Object::Process { index, generation }, WAIT | KILL)?;
    let priority = priority.min(sched.priority(cpu));
    let current = &PROCESSES[sched.process(cpu)].budget;
    if budget > MAX_FRAMES || !current.shrink(budget) {
        return Err(ENOMEM);
    }
    let sockets = crate::net::sockets_cost(&child, w);
    let child_entry = (process, slot, &child, priority);
    let spawned = match sockets > budget {
        true => Err(ENOBUFS),
        false => {
            let memory = (budget, sockets, MAP_BASE);
            spawn_process((sched, w), executable, memory, child_entry, (args, argc))
        }
    };
    if let Err(error) = spawned {
        current.grow(budget);
        return Err(error);
    }
    table.commit(parent, &rest);
    crate::net::spawned((sched, w), cpu, index, &child, &rest);
    Ok(handle)
}

/// Starts a thread of `cpu`'s current process at user address `entry` with SP_EL0 = `sp`, TPIDR_EL0 = `tls` and x0 =
/// `arg`, at the caller's priority, its kernel stack charged to the process's budget; returns a handle to it (wait,
/// kill, duplicate, transfer), written to `table` (under its lock) last. On failure nothing changes. A caller marked to
/// end gets `EAGAIN`, so a process being ended gains no thread.
pub(crate) fn thread(
    (sched, w, cpu): (&mut Sched, &mut W<'_, level::Kernel>, usize),
    (table, process): (&Table, &mut Process),
    entry: u64,
    (sp, tls): (u64, u64),
    arg: u64,
) -> Result<u64, i64> {
    if sched.marked(cpu).is_some() {
        return Err(EAGAIN);
    }
    let (slot, generation) = sched.free_slot().ok_or(EAGAIN)?;
    let [at] = table.reserve(process)?;
    let index = sched.process(cpu);
    let stack = (PROCESSES[index].budget)
        .alloc_contiguous(&mut FRAMES.lock_masked(w), TASK_STACK_FRAMES)
        .ok_or(ENOMEM)?;
    // SAFETY: the kernel stack below `stack.end` is fresh and owned by the new thread.
    let frame = unsafe { arch::new_user_task(stack.end.0 as usize, entry, (sp, tls), [arg, 0, 0]) };
    let priority = sched.priority(cpu);
    sched.add((slot, generation), index, (frame, stack.start), priority);
    PROCESSES[index].threads.fetch_add(1, Relaxed);
    let thread = Object::Thread { slot, generation };
    sched.held(thread);
    Ok(table.fill(process, at, thread, WAIT | KILL | DUPLICATE | TRANSFER))
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
    static user_map_end: u8;
    static user_map_end_end: u8;
}

/// A user program's code and the address it is mapped and starts at.
pub(crate) fn user_program(program: Program) -> (&'static [u8], u64) {
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
        Program::MapEnd => (
            &raw const user_map_end,
            &raw const user_map_end_end,
            USER_BASE,
        ),
    };
    // SAFETY: `user.s` places each program's bytes between its start and end labels in read-only data.
    let code = unsafe { slice::from_raw_parts(start, end as usize - start as usize) };
    (code, va)
}
