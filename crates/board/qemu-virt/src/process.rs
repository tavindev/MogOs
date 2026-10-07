//! Process construction: address spaces, ELF and asm programs, spawn and its arguments.

use core::arch::global_asm;
use core::ops::Range;
use core::ptr;
use core::slice;

use arch::{UserAccess, user_page};
use kernel::elf::{Elf, Segment};
use kernel::handle::{
    CONNECT, DUPLICATE, Handles, KILL, LISTEN, MAX_HANDLES, Object, READ, Rights, TRANSFER, WAIT,
    WRITE,
};
use kernel::syscall::{EAGAIN, EFAULT, ENOEXEC, ENOMEM, MAX_BUFFER};
use kernel::{FRAME_WORDS, Memory, Program};
use mm::{Budget, FrameAllocator, PhysAddr};
use mogfs::ROOT;

use crate::usermem::copy_in;
use crate::{
    ARCHIVE, IMAGE, KERNEL, KERNEL_ENTRIES, Kernel, MAP_BASE, Nospec, PAGE, Sched,
    TASK_STACK_FRAMES, USER_BASE, USER_STACK_TOP,
};

/// Returns a thread's kernel stack at `stack` to `frames`, refunding `budget`.
pub(crate) fn free_stack(
    frames: &mut FrameAllocator<FRAME_WORDS>,
    budget: &mut Budget,
    stack: PhysAddr,
) {
    for i in 0..TASK_STACK_FRAMES {
        budget.free(frames, PhysAddr(stack.0 + (i * PAGE) as u64));
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
    // SAFETY: as above; `bytes` is kernel memory, never this frame.
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
    sched: &mut Sched,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    pages: usize,
) -> Option<u64> {
    let asid = sched.process();
    let l1 = sched.space(asid);
    let memory = sched.memory(asid);
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

/// Builds a process from `file`'s `segments` (entered at `entry`): address space, pages and its first thread's kernel
/// stack, all charged to `budget`, and queues it at the free index `process`, its thread in the free `slot`, with
/// `handles` at `priority`; on failure (`ENOMEM`) returns every frame it took. With `args` (`argc` of them, at most a
/// page), the top stack page holds them and the stack gets a page below it.
fn spawn_process(
    sched: &mut Sched,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    (file, segments, entry): (&[u8], impl Iterator<Item = Segment>, u64),
    mut budget: Budget,
    (process, slot, handles, priority): ((usize, u64), (usize, u64), Handles, u8),
    (args, argc): (&[u8], usize),
) -> Result<(), i64> {
    let l1 = zeroed(frames, &mut budget).ok_or(ENOMEM)?;
    // SAFETY: `l1` is a fresh frame; the boot table's kernel entries stay fixed after `kmain`.
    unsafe {
        ptr::copy_nonoverlapping(
            arch::boot_table().0 as *const u64,
            l1.0 as *mut u64,
            KERNEL_ENTRIES,
        )
    };
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
    let frame = unsafe { arch::new_user_task(stack.end.0 as usize, entry, (at & !15, 0), x) };
    let memory = Memory {
        budget,
        next: MAP_BASE,
    };
    sched.add_process(process, l1, memory, handles);
    sched.add(slot, process.0, (frame, stack.start), priority);
    // Its one handle: the spawner's, or init's own.
    let (index, generation) = process;
    sched.held(Object::Process { index, generation });
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

/// Queues `executable` from boot context with init's handles, a budget of `budget` frames, `priority` and `args`.
pub(crate) fn spawn_init(
    executable: (&[u8], impl Iterator<Item = Segment>, u64),
    budget: usize,
    priority: u8,
    archive: Rights,
    args: &[u8],
) -> Result<(), i64> {
    let argc = kernel::syscall::argc(args)?;
    let mut kernel = KERNEL.lock();
    let Kernel {
        sched,
        frames,
        mounted,
        ..
    } = &mut *kernel;
    let ids = sched.free_process().zip(sched.free_slot()).ok_or(EAGAIN);
    ids.and_then(|(process, slot)| {
        let mut handles = Handles::init(process.0, process.1, archive);
        if *mounted {
            handles.insert(Object::Dir(ROOT), READ | WRITE | DUPLICATE | TRANSFER)?;
        }
        if crate::net::STARTED.load(core::sync::atomic::Ordering::Relaxed) {
            let rights = CONNECT | LISTEN | DUPLICATE | TRANSFER;
            handles.insert(Object::NetStack, rights)?;
        }
        let init = (process, slot, handles, priority);
        spawn_process(
            sched,
            frames,
            executable,
            Budget::new(budget),
            init,
            (args, argc),
        )
    })
}

/// Spawns the boot archive's executable at `file` with the `len` handles at user address `ptr` and `budget` frames
/// moved from the current process, which gets a handle to the child, at `priority` capped at the current process's
/// own, with the arguments at user address `args`, both copied in through `buf`; on failure nothing moves.
pub(crate) fn spawn(
    sched: &mut Sched,
    frames: &mut FrameAllocator<FRAME_WORDS>,
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
    let (mut parent, child) = sched
        .handles()
        .split::<Nospec>(&list[..len % (MAX_HANDLES + 1)])?;
    let current = sched.process();
    if budget > sched.memory(current).budget.remaining() {
        return Err(ENOMEM);
    }
    let (process, slot) = sched.free_process().zip(sched.free_slot()).ok_or(EAGAIN)?;
    let (index, generation) = process;
    let handle = parent.insert(Object::Process { index, generation }, WAIT | KILL)?;
    let priority = priority.min(sched.priority());
    let mut child_budget = Budget::new(budget);
    crate::net::spawn_charge(&child, &mut child_budget)?;
    let moved = child;
    let child = (process, slot, child, priority);
    spawn_process(sched, frames, executable, child_budget, child, (args, argc))?;
    sched.memory(current).budget.shrink(budget);
    *sched.handles() = parent;
    crate::net::spawned(sched, index, &moved);
    Ok(handle)
}

/// Starts a thread of the current process at user address `entry` with SP_EL0 = `sp`, TPIDR_EL0 = `tls` and x0 =
/// `arg`, at the caller's priority, its kernel stack charged to the process's budget; returns a handle to it (wait,
/// kill, duplicate, transfer). On failure nothing changes.
pub(crate) fn thread(
    sched: &mut Sched,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    entry: u64,
    (sp, tls): (u64, u64),
    arg: u64,
) -> Result<u64, i64> {
    let (slot, generation) = sched.free_slot().ok_or(EAGAIN)?;
    let mut handles = *sched.handles();
    let thread = Object::Thread { slot, generation };
    let handle = handles.insert(thread, WAIT | KILL | DUPLICATE | TRANSFER)?;
    let index = sched.process();
    let budget = &mut sched.memory(index).budget;
    let stack = budget
        .alloc_contiguous(frames, TASK_STACK_FRAMES)
        .ok_or(ENOMEM)?;
    // SAFETY: the kernel stack below `stack.end` is fresh and owned by the new thread.
    let frame = unsafe { arch::new_user_task(stack.end.0 as usize, entry, (sp, tls), [arg, 0, 0]) };
    let priority = sched.priority();
    sched.add((slot, generation), index, (frame, stack.start), priority);
    sched.held(thread);
    *sched.handles() = handles;
    Ok(handle)
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
