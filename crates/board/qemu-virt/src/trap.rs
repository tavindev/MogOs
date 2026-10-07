//! Trap hooks: switch, IRQ, syscall and user fault, and the task exit and release they run.

use core::fmt::Write;
use core::sync::atomic::Ordering::Relaxed;

use arch::Guard;
use kernel::file;
use kernel::handle::{DUPLICATE, Object, READ, TRANSFER, WRITE};
use kernel::mutex::Mutexes;
use kernel::pipe::{self, End, Pipes};
use kernel::syscall::{Call, EBADF, EFAULT, ENFILE, ENOENT, ENOMEM, KILLED};
use kernel::{Event, FRAME_WORDS, Scheduler};
use mm::{FrameAllocator, PhysAddr};

use crate::process::{enter, free_stack, map, spawn};
use crate::uart::Uart;
use crate::usermem::{user_bytes, user_bytes_mut};
use crate::{
    ARCHIVE, GIC_CPU, KERNEL, Kernel, MAX_MUTEXES, MAX_PIPES, MAX_TASKS, TICK_US, TICKED,
    TIMER_IRQ, UART_IRQ, UART0,
};

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

/// # Safety
/// IRQs must be masked (trap context), as `task_switch` requires; returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_irq(frame: usize) -> usize {
    let Kernel { sched, line, .. } = Guard::leak(KERNEL.lock_masked());
    let cpu = PhysAddr(GIC_CPU.load(Relaxed));
    // SAFETY: IRQs are delivered only after `kmain` stored the DTB's GIC CPU interface.
    let iar = unsafe { arch::gic::ack(cpu) };
    let irq = iar & 0x3ff;
    let tick = irq == TIMER_IRQ;
    if tick {
        arch::timer::arm(TICK_US);
        TICKED.fetch_or(1 << arch::cpu(), Relaxed);
    } else if irq == UART_IRQ {
        let mut uart = Uart::new(UART0);
        while let Some(byte) = uart.get() {
            if line.push(byte, |echo| uart.write(echo)) {
                sched.wake(Event::Console);
            }
        }
    }
    // SAFETY: as above.
    unsafe { arch::gic::eoi(cpu, iar) };
    // Only core 0 runs tasks: the scheduler's one `current` is core 0's.
    if !tick || arch::cpu() != 0 {
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
