//! Trap hooks: switch, IRQ, syscall and user fault, and the thread and process ends and releases they run.

use core::fmt::Write;
use core::sync::atomic::Ordering::Relaxed;

use arch::Guard;
use kernel::Event;
use kernel::FRAME_WORDS;
use kernel::file;
use kernel::handle::{DUPLICATE, Object, READ, TRANSFER, WRITE};
use kernel::mutex::Mutexes;
use kernel::pipe::{self, End, Pipes};
use kernel::syscall::{Call, EBADF, EFAULT, ENFILE, ENOENT, ENOMEM, KILLED, MAX_BUFFER};
use mm::{FrameAllocator, PhysAddr};

use crate::process::{free_stack, map, spawn, thread};
use crate::uart::Uart;
use crate::usermem::{UserIn, UserOut, copy_in};
use crate::{
    ARCHIVE, GIC_CPU, KERNEL, Kernel, MAX_MUTEXES, MAX_PIPES, Sched, TICK_US, TICKED, TIMER_IRQ,
    UART_IRQ, UART0,
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

/// Saves the current task's `frame` and enters the next ready one; returns its frame. SP_EL0 and TPIDR_EL0 move unless
/// it is the same task or both are kernel tasks (each thread has its own); TTBR0 only if the process changed: the
/// kernel's boot table keeps ASID 0, a process's level-1 table has ASID = its index.
///
/// # Safety
/// IRQs must be masked (trap context), and `frame` the current task's trap frame.
unsafe fn switch(sched: &mut Sched, frame: usize) -> usize {
    let from = sched.process();
    let next = sched.switch(frame);
    let to = sched.process();
    if to != from || (to != 0 && next != frame) {
        // SAFETY: `frame` came from the trap path and `next` from the scheduler.
        unsafe { arch::switch_el0_regs(frame, next) };
    }
    if to != from {
        let table = match to {
            0 => arch::boot_table(),
            to => sched.space(to),
        };
        // SAFETY: every space's table holds the kernel blocks, and ASID `to` is used only by the process at that index.
        unsafe { arch::set_ttbr0(table, to) };
    }
    next
}

/// Ends the thread in `slot` with `code`: frees the mutexes it owns, drops the boost it lent, and refunds its kernel
/// stack to its process. The caller switches away if it is current.
fn end_thread(
    sched: &mut Sched,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    mutexes: &mut Mutexes<MAX_MUTEXES>,
    slot: usize,
    code: u64,
) {
    let (stack, blocked) = sched.end(slot, code);
    let owner = match blocked {
        Some(Event::Lock(index)) => mutexes.owner(index),
        _ => None,
    };
    for index in mutexes.release(slot) {
        sched.wake(Event::Lock(index));
    }
    if let Some(owner) = owner {
        sched.unboost(
            owner,
            |e| matches!(e, Event::Lock(i) if mutexes.owner(i) == Some(owner)),
        );
    }
    // This may run on that stack: reused only after the trap exit leaves it and releases `KERNEL`.
    free_stack(
        frames,
        &mut sched.memory(sched.process_of(slot)).budget,
        stack,
    );
}

/// Ends the process at `index` with `code`: releases its handles, then ends every thread; returns its address space,
/// which the caller frees once no TTBR0 uses it. Its index is free from here, but no core can take it before that
/// free: that needs `KERNEL`, which this trap holds until it returns.
fn end_process(kernel: &mut Kernel, index: usize, code: u64) -> PhysAddr {
    let Kernel {
        sched,
        frames,
        pipes,
        mutexes,
        ..
    } = kernel;
    for object in sched.take_handles(index).objects() {
        release(sched, frames, pipes, mutexes, object);
    }
    while let Some(slot) = sched.thread_of(index) {
        end_thread(sched, frames, mutexes, slot, code);
    }
    sched.space(index)
}

/// Ends the current process with `code`, returns all its frames, and returns the next task's frame.
///
/// # Safety
/// IRQs must be masked (trap context), the current task must be a process's, and `frame` its trap frame.
unsafe fn exit_process(kernel: &mut Kernel, frame: usize, code: u64) -> usize {
    let index = kernel.sched.process();
    // Before `switch` picks the next task, so a task this wakes can be it.
    let l1 = end_process(kernel, index, code);
    // SAFETY: the caller's contract.
    let next = unsafe { switch(&mut kernel.sched, frame) };
    arch::flush_asid(index);
    // SAFETY: `switch` left `l1`, as none of its threads is left to run, and its tables hold only its frames.
    unsafe { arch::free_space(l1, |f| kernel.frames.free(f)) };
    next
}

/// Ends the current thread with `code`, or its process with its last thread; returns the next task's frame.
///
/// # Safety
/// As `exit_process`.
unsafe fn exit_thread(kernel: &mut Kernel, frame: usize, code: u64) -> usize {
    let Kernel {
        sched,
        frames,
        mutexes,
        ..
    } = kernel;
    if sched.threads(sched.process()) == 1 {
        // SAFETY: the caller's contract.
        return unsafe { exit_process(kernel, frame, code) };
    }
    end_thread(sched, frames, mutexes, sched.current().0, code);
    // SAFETY: the caller's contract.
    unsafe { switch(sched, frame) }
}

/// Ends the process at `index`, not the current one, as a fault would, and returns all its frames.
fn kill(kernel: &mut Kernel, index: usize) {
    let l1 = end_process(kernel, index, KILLED);
    arch::flush_asid(index);
    // SAFETY: the process is not current, so TTBR0 is not `l1`, and its tables hold only its frames.
    unsafe { arch::free_space(l1, |f| kernel.frames.free(f)) };
}

/// Blocks the current process on `event` with its `svc` rewound, so the call runs again once woken; returns the next
/// task's frame.
///
/// # Safety
/// IRQs must be masked (trap context), and `frame` the current process's.
unsafe fn block(sched: &mut Sched, frame: &mut arch::TrapFrame, event: Event) -> usize {
    frame.restart();
    sched.block(event);
    // SAFETY: the caller masked IRQs, and `frame` is the current process's.
    unsafe { switch(sched, frame as *mut arch::TrapFrame as usize) }
}

/// Drops one handle to `object`: an ended process frees its index and, as `wait` does, moves its budget to the
/// current process; an ended thread frees its slot; a pipe wakes its waiters and, once no handle reaches it, frees its
/// page, refunding its creator if that still runs; the last handle to a mutex frees it.
fn release(
    sched: &mut Sched,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    pipes: &mut Pipes<MAX_PIPES>,
    mutexes: &mut Mutexes<MAX_MUTEXES>,
    object: Object,
) {
    let end = match object {
        Object::Pipe(end) => end,
        Object::Mutex(mutex) => return mutexes.close(mutex),
        Object::Process { index, generation } => {
            let limit = sched.close(index, generation);
            let held = pipes.charged_to((index, generation));
            let current = sched.process();
            return sched
                .memory(current)
                .budget
                .grow(limit.saturating_sub(held));
        }
        Object::Thread { slot, generation } => return sched.close_thread(slot, generation),
        _ => return,
    };
    if let Some((page, (index, generation))) = pipes.close(end) {
        match sched.budget(index, generation) {
            Some(budget) => budget.free(frames, page),
            None => frames.free(page),
        }
    }
    sched.wake(Event::Pipe(end.index as usize));
}

/// Creates a pipe whose page is charged to the current process, which gets a handle to each end (read, write).
fn new_pipe(
    sched: &mut Sched,
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
    let process = (sched.process(), sched.generation());
    let page = sched.memory(process.0).budget.alloc(frames).ok_or(ENOMEM)?;
    pipes.create(read, page, process);
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
        true => UserIn::new(ptr, len).map_or(Some(EFAULT), |data| {
            pipe.write(page, len, |chunk, at| data.read(at, chunk))
        }),
        // Before the probe, which costs more than the rest of the call (hvf), so a read that waits skips it.
        false if pipe.read_waits(len) => None,
        false => UserOut::new(ptr, len).map_or(Some(EFAULT), |out| {
            pipe.read(page, len, |chunk, at| out.write(at, chunk))
        }),
    }
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

/// Copies the user buffer at `ptr` into `buf` and runs `f` on the copy; `EFAULT` if EL0 may not read it.
fn with_input<T>(
    (ptr, len): (u64, usize),
    buf: &mut [u8],
    f: impl FnOnce(&[u8]) -> Result<T, i64>,
) -> Result<T, i64> {
    copy_in(ptr, len, buf).ok_or(EFAULT).and_then(f)
}

/// Fills the first `len` bytes of `buf` with `f`, which returns the count, and copies them to the user buffer at
/// `ptr`; `EFAULT` (before `f` runs) if EL0 may not write it.
fn with_output(
    (ptr, len): (u64, usize),
    buf: &mut [u8],
    f: impl FnOnce(&mut [u8]) -> Result<usize, i64>,
) -> u64 {
    let Some(out) = UserOut::new(ptr, len) else {
        return EFAULT as u64;
    };
    match f(&mut buf[..len]) {
        Ok(n) => {
            out.write(0, &buf[..n]);
            n as u64
        }
        Err(error) => error as u64,
    }
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
        buf,
        ..
    } = kernel;
    let args = frame.x.first_chunk().unwrap();
    let ok = |result: Result<(), i64>| result.map_or_else(|error| error as u64, |()| 0);
    frame.x[0] = match kernel::syscall::dispatch(frame.x[8], args, sched.handles()) {
        Ok(Call::Exit(code)) => {
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            return unsafe { exit_process(kernel, frame as *mut arch::TrapFrame as usize, code) };
        }
        Ok(Call::ThreadExit(code)) => {
            // SAFETY: as above.
            return unsafe { exit_thread(kernel, frame as *mut arch::TrapFrame as usize, code) };
        }
        Ok(Call::Thread {
            entry,
            stack,
            tls,
            arg,
        }) => thread(sched, frames, entry, (stack, tls), arg).unwrap_or_else(|error| error as u64),
        Ok(Call::Write { ptr, len }) => match copy_in(ptr, len, buf) {
            Some(bytes) => {
                Uart::new(UART0).write(bytes);
                len as u64
            }
            None => EFAULT as u64,
        },
        Ok(Call::Read { ptr, len }) => match UserOut::new(ptr, len) {
            None => EFAULT as u64,
            Some(out) => match line.read(&mut buf[..len]) {
                Some(n) => {
                    out.write(0, &buf[..n]);
                    n as u64
                }
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                None => return unsafe { block(sched, frame, Event::Console) },
            },
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
        Ok(Call::Wait { index, generation }) => match sched.reap(index, generation) {
            Ok(Some((code, limit))) => {
                // Pipes it created that are still open keep their page; a repeated `wait` gets a limit of 0.
                let held = pipes.charged_to((index, generation));
                let current = sched.process();
                sched
                    .memory(current)
                    .budget
                    .grow(limit.saturating_sub(held));
                code
            }
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            Ok(None) => return unsafe { block(sched, frame, Event::Exit(index)) },
            Err(error) => error as u64,
        },
        Ok(Call::Join { slot, generation }) => match sched.join(slot, generation) {
            Ok(Some(code)) => code,
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            Ok(None) => return unsafe { block(sched, frame, Event::Join(slot)) },
            Err(error) => error as u64,
        },
        Ok(Call::Dup { handle, object }) => {
            match object {
                Object::Pipe(end) => pipes.open(end),
                Object::Mutex(mutex) => mutexes.open(mutex),
                object => sched.held(object),
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
        }) => with_input((ptr, len), buf, |data| {
            fs.write(inode, offset, data).map_err(file::errno)
        })
        .map_or_else(|error| error as u64, |()| len as u64),
        Ok(Call::File {
            inode,
            offset,
            ptr,
            len,
            ..
        }) => with_output((ptr, len), buf, |out| {
            fs.read(inode, offset, out).map_err(file::errno)
        }),
        Ok(Call::Open {
            dir,
            ptr,
            len,
            flags,
            rights,
        }) => with_input((ptr, len), buf, |path| match dir {
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
        Ok(Call::Mkdir { dir, ptr, len }) => ok(with_input((ptr, len), buf, |path| {
            file::mkdir(fs, dir, path)
        })),
        Ok(Call::Readdir {
            dir,
            ptr,
            len,
            start,
        }) => with_output((ptr, len), buf, |out| match dir {
            Object::Dir(dir) => file::readdir(fs, dir, start, out),
            _ => file::list_archive(ARCHIVE, start, out),
        }),
        Ok(Call::Unlink { dir, ptr, len }) => ok(with_input((ptr, len), buf, |path| {
            let held = |i| sched.holds(|o| o == Object::Dir(i) || o == Object::Node(i));
            file::unlink(fs, dir, path, held)
        })),
        Ok(Call::Rename { from, to }) => {
            let (from_buf, to_buf) = buf.split_at_mut(MAX_BUFFER as usize);
            ok(with_input((from.1, from.2), from_buf, |f| {
                with_input((to.1, to.2), to_buf, |t| {
                    file::rename(fs, (from.0, f), (to.0, t))
                })
            }))
        }
        Ok(Call::Sync) => ok(fs.commit().map_err(file::errno)),
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
            buf,
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
            let (slot, _) = sched.current();
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
        Ok(Call::Kill { index, generation }) => match sched.process_live(index, generation) {
            Ok(true) if index == sched.process() => {
                let frame = frame as *mut arch::TrapFrame as usize;
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                return unsafe { exit_process(kernel, frame, KILLED) };
            }
            Ok(true) => {
                kill(kernel, index);
                0
            }
            Ok(false) => 0,
            Err(error) => error as u64,
        },
        Ok(Call::KillThread { slot, generation }) => match sched.thread_live(slot, generation) {
            Ok(true) if slot == sched.current().0 => {
                let frame = frame as *mut arch::TrapFrame as usize;
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                return unsafe { exit_thread(kernel, frame, KILLED) };
            }
            // Not the current thread, so its last thread is in another process.
            Ok(true) if sched.threads(sched.process_of(slot)) == 1 => {
                let index = sched.process_of(slot);
                kill(kernel, index);
                0
            }
            Ok(true) => {
                end_thread(sched, frames, mutexes, slot, KILLED);
                0
            }
            Ok(false) => 0,
            Err(error) => error as u64,
        },
        Err(error) => error as u64,
    };
    frame as *mut arch::TrapFrame as usize
}

/// # Safety
/// Trap context (IRQs masked), and `frame` the current process's. Returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_user_fault(frame: usize, ec: u64, far: u64) -> usize {
    let kernel = Guard::leak(KERNEL.lock_masked());
    let index = kernel.sched.process();
    let _ = writeln!(Uart::new(UART0), "fault: {index} ec={ec:#x} far={far:#x}");
    // SAFETY: the caller masked IRQs; `frame` is the current process's.
    unsafe { exit_process(kernel, frame, KILLED) }
}
