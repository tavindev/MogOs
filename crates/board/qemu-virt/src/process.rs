//! Process construction: address spaces, ELF and asm programs, spawn and its arguments.

use core::arch::global_asm;
use core::ops::Range;
use core::ptr;
use core::slice;

use arch::{UserAccess, user_page};
use kernel::elf::{Elf, Segment};
use kernel::handle::{
    DUPLICATE, Handles, KILL, MAX_HANDLES, Object, READ, Rights, TRANSFER, WAIT, WRITE,
};
use kernel::syscall::{EAGAIN, EFAULT, ENOEXEC, ENOMEM};
use kernel::{FRAME_WORDS, Memory, Program, Scheduler};
use mm::{Budget, FrameAllocator, PhysAddr};
use mogfs::ROOT;

use crate::usermem::user_bytes;
use crate::{
    ARCHIVE, IMAGE, KERNEL, KERNEL_L1, Kernel, MAP_BASE, MAX_TASKS, PAGE, TASK_STACK_FRAMES,
    USER_BASE, USER_STACK_TOP,
};

pub(crate) fn free_stack(frames: &mut FrameAllocator<FRAME_WORDS>, stack: PhysAddr) {
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
pub(crate) fn map(
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
pub(crate) unsafe fn enter(sched: &Scheduler<MAX_TASKS>, frame: usize, next: usize) {
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
pub(crate) fn executable(
    file: Range<usize>,
) -> Result<(&'static [u8], impl Iterator<Item = Segment>, u64), i64> {
    let file = &ARCHIVE[file];
    let elf = Elf::parse(file, IMAGE).ok_or(ENOEXEC)?;
    let entry = elf.entry;
    Ok((file, elf.segments(), entry))
}

/// Queues `executable` from boot context with init's handles, a budget of `budget` frames and `priority`.
pub(crate) fn spawn_init(
    executable: (&[u8], impl Iterator<Item = Segment>, u64),
    budget: usize,
    priority: u8,
    archive: Rights,
) -> Result<(), i64> {
    let mut kernel = KERNEL.lock();
    let Kernel {
        sched,
        frames,
        mounted,
        ..
    } = &mut *kernel;
    sched.free_slot().ok_or(EAGAIN).and_then(|slot| {
        let mut handles = Handles::init(slot.0, slot.1, archive);
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
pub(crate) fn spawn(
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
    };
    // SAFETY: `user.s` places each program's bytes between its start and end labels in read-only data.
    let code = unsafe { slice::from_raw_parts(start, end as usize - start as usize) };
    (code, va)
}
