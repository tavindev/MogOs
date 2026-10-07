//! Trap hooks: switch, IRQ, syscall and user fault, and the thread and process ends and releases they run.

use core::fmt::Write;
use core::sync::atomic::Ordering::{Relaxed, Release};

use arch::Guard;
use kernel::Event;
use kernel::FRAME_WORDS;
use kernel::file;
use kernel::handle::{DUPLICATE, Object, READ, TRANSFER, WRITE};
use kernel::mutex::Mutexes;
use kernel::pipe::{self, End, Pipes};
use kernel::syscall::{Call, EBADF, EFAULT, ENFILE, ENOENT, ENOMEM, KILLED, MAX_BUFFER};
use mm::{FrameAllocator, PhysAddr};

use crate::net;
use crate::process::{free_stack, map, spawn, thread};
use crate::usermem::{UserIn, UserOut, copy_in};
use crate::{
    ARCHIVE, CONSOLE, KERNEL, Kernel, MAX_MUTEXES, MAX_PIPES, PING_SGI, PONGS, Sched, TICK_COUNTED,
    TICK_US, TICKED, TICKS, TIMER_IRQ, UART_IRQ, kick, send, send_sgi,
};

/// # Safety
/// Trap context (IRQs masked), and `frame` the current task's trap frame. Returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn task_switch(frame: usize) -> usize {
    let sched = &mut Guard::leak(KERNEL.lock_masked()).sched;
    let cpu = arch::cpu();
    // SAFETY: the caller masked IRQs, and `frame` came from the trap path.
    let next = unsafe { switch(sched, cpu, frame) };
    kick(sched, cpu);
    next
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

/// Saves `cpu`'s current `frame` and enters the next ready task, or its idle context (process 0); returns its frame.
/// SP_EL0 and TPIDR_EL0 move unless it is the same task or both are kernel tasks (each thread has its own); TTBR0
/// only if the process changed: the kernel's boot table keeps ASID 0, a process's level-1 table has ASID = its index.
///
/// # Safety
/// IRQs must be masked (trap context), and `frame` `cpu`'s current trap frame.
unsafe fn switch(sched: &mut Sched, cpu: usize, frame: usize) -> usize {
    let from = sched.process(cpu);
    let next = sched.switch(cpu, frame);
    let to = sched.process(cpu);
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

/// Ends the thread in `slot`, which no core but `cpu` runs, with `code`: frees the mutexes it owns, drops the boost it
/// lent, and refunds its kernel stack to its process. The caller switches away if it is current. The boot context may
/// wait for the task count to drop, so core 0 is signalled.
fn end_thread(
    sched: &mut Sched,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    mutexes: &mut Mutexes<MAX_MUTEXES>,
    (cpu, slot): (usize, usize),
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
    if cpu != 0 && sched.boot_waits() {
        send_sgi(0);
    }
}

/// Ends the process at `index` with `code` from `cpu`: releases its handles, ends every thread no other core runs, and
/// marks the others to end on their cores, signalling them. Returns its address space once no thread is left, which
/// the caller frees once no TTBR0 uses it; otherwise the last marked thread does. Its index is free from then, but no
/// core can take it before that free: that needs `KERNEL`, which this trap holds until it returns.
fn end_process(kernel: &mut Kernel, cpu: usize, index: usize, code: u64) -> Option<PhysAddr> {
    let Kernel {
        sched,
        frames,
        pipes,
        mutexes,
        ..
    } = kernel;
    for object in sched.take_handles(index).objects() {
        release(sched, cpu, frames, pipes, mutexes, (object, index));
    }
    while let Some(slot) = sched.thread_of(index, cpu) {
        end_thread(sched, frames, mutexes, (cpu, slot), code);
    }
    let elsewhere = sched.threads_elsewhere(index, cpu);
    for slot in (0..64).filter(|s| elsewhere & 1 << s != 0) {
        end_remote(sched, slot, code);
    }
    (elsewhere == 0).then(|| sched.space(index))
}

/// Ends the thread in `slot`, which another core runs, with `code` on that core: marks it and signals the core.
fn end_remote(sched: &mut Sched, slot: usize, code: u64) {
    sched.mark(slot, code);
    send_sgi(sched.core_of(slot).expect("runs elsewhere"));
}

/// Ends `cpu`'s current process with `code`, returns all its frames once no other core runs one of its threads, and
/// returns the next task's frame.
///
/// # Safety
/// IRQs must be masked (trap context), `cpu`'s current task must be a process's, and `frame` its trap frame.
unsafe fn exit_process(kernel: &mut Kernel, cpu: usize, frame: usize, code: u64) -> usize {
    let index = kernel.sched.process(cpu);
    // Before `switch` picks the next task, so a task this wakes can be it.
    let l1 = end_process(kernel, cpu, index, code);
    // SAFETY: the caller's contract.
    let next = unsafe { switch(&mut kernel.sched, cpu, frame) };
    if let Some(l1) = l1 {
        arch::flush_asid(index);
        // SAFETY: `switch` left `l1` and no other core runs a thread of it; its tables hold only its frames.
        unsafe { arch::free_space(l1, |f| kernel.frames.free(f)) };
    }
    next
}

/// Ends `cpu`'s current thread with `code`, or its process with its last thread; returns the next task's frame.
///
/// # Safety
/// As `exit_process`.
unsafe fn exit_thread(kernel: &mut Kernel, cpu: usize, frame: usize, code: u64) -> usize {
    let Kernel {
        sched,
        frames,
        mutexes,
        ..
    } = kernel;
    if sched.threads(sched.process(cpu)) == 1 {
        // SAFETY: the caller's contract.
        return unsafe { exit_process(kernel, cpu, frame, code) };
    }
    end_thread(sched, frames, mutexes, (cpu, sched.current(cpu).0), code);
    // SAFETY: the caller's contract.
    unsafe { switch(sched, cpu, frame) }
}

/// Ends the process at `index`, not `cpu`'s current one, as a fault would, and returns all its frames once no core runs
/// one of its threads.
fn kill(kernel: &mut Kernel, cpu: usize, index: usize) {
    if let Some(l1) = end_process(kernel, cpu, index, KILLED) {
        arch::flush_asid(index);
        // SAFETY: no core runs the process, so no TTBR0 is `l1`, and its tables hold only its frames.
        unsafe { arch::free_space(l1, |f| kernel.frames.free(f)) };
    }
}

/// Blocks `cpu`'s current process on `event` with its `svc` rewound, so the call runs again once woken; returns the
/// next task's frame. A thread marked to end ends instead.
///
/// # Safety
/// IRQs must be masked (trap context), and `frame` `cpu`'s current process's.
unsafe fn block(
    kernel: &mut Kernel,
    cpu: usize,
    frame: &mut arch::TrapFrame,
    event: Event,
) -> usize {
    let at = frame as *mut arch::TrapFrame as usize;
    if let Some(code) = kernel.sched.marked(cpu) {
        // SAFETY: the caller's contract.
        return unsafe { exit_thread(kernel, cpu, at, code) };
    }
    frame.restart();
    kernel.sched.block(cpu, event);
    // SAFETY: the caller masked IRQs, and `frame` is `cpu`'s current process's.
    unsafe { switch(&mut kernel.sched, cpu, at) }
}

/// Drops one handle to `object`: an ended process frees its index and, as `wait` does, moves its budget to `cpu`'s
/// current process; an ended thread frees its slot; a pipe wakes its waiters and, once no handle reaches it, frees its
/// page, refunding its creator if that still runs; the last handle to a mutex frees it.
fn release(
    sched: &mut Sched,
    cpu: usize,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    pipes: &mut Pipes<MAX_PIPES>,
    mutexes: &mut Mutexes<MAX_MUTEXES>,
    (object, holder): (Object, usize),
) {
    let end = match object {
        Object::Pipe(end) => end,
        Object::Mutex(mutex) => return mutexes.close(mutex),
        Object::Socket(sock) => return net::close(sched, cpu, sock, holder),
        Object::Process { index, generation } => {
            let limit = sched.close(index, generation);
            let held = pipes.charged_to((index, generation));
            let current = sched.process(cpu);
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

/// Creates a pipe whose page is charged to `cpu`'s current process, which gets a handle to each end (read, write).
fn new_pipe(
    sched: &mut Sched,
    cpu: usize,
    frames: &mut FrameAllocator<FRAME_WORDS>,
    pipes: &mut Pipes<MAX_PIPES>,
) -> Result<(u64, u64), i64> {
    let read = pipes.free().ok_or(ENFILE)?;
    let write = End {
        write: true,
        ..read
    };
    let mut handles = *sched.handles(cpu);
    let read_handle = handles.insert(Object::Pipe(read), READ | DUPLICATE | TRANSFER)?;
    let write_handle = handles.insert(Object::Pipe(write), WRITE | DUPLICATE | TRANSFER)?;
    let process = (sched.process(cpu), sched.generation(cpu));
    let page = sched.memory(process.0).budget.alloc(frames).ok_or(ENOMEM)?;
    pipes.create(read, page, process);
    *sched.handles(cpu) = handles;
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

/// The first of the special interrupt IDs `ack` may return (1023: none pending), which take no EOI.
const SPURIOUS: u32 = 1020;

/// # Safety
/// IRQs must be masked (trap context), as `task_switch` requires; returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_irq(frame: usize) -> usize {
    let kernel = Guard::leak(KERNEL.lock_masked());
    let cpu = arch::cpu();
    let idle = kernel.sched.idle(cpu);
    let irq = arch::gic::ack();
    let tick = irq == TIMER_IRQ;
    if tick {
        if !TICK_COUNTED.with(|counted| core::mem::replace(counted, true)) {
            TICKED.fetch_add(1, Relaxed);
        }
        net::tick(&mut kernel.sched);
    } else if irq == PING_SGI {
        match cpu {
            0 => _ = PONGS.fetch_add(1, Release),
            _ => send(0, PING_SGI),
        }
    } else if irq == net::IRQ.load(Relaxed) {
        net::interrupt(&mut kernel.sched);
    } else if irq == UART_IRQ {
        let mut uart = CONSOLE.lock_masked();
        while let Some(byte) = uart.get() {
            if kernel.line.push(byte, |echo| uart.write(echo)) {
                kernel.sched.wake(Event::Console);
            }
        }
    }
    if irq < SPURIOUS {
        arch::gic::eoi(irq);
    }
    let next = match kernel.sched.marked(cpu) {
        // SAFETY: the caller masked IRQs, and `frame` is `cpu`'s current thread's, which is marked.
        Some(code) => unsafe { exit_thread(kernel, cpu, frame, code) },
        // SAFETY: the caller masked IRQs, and `frame` came from the trap path.
        None if tick || idle => unsafe { switch(&mut kernel.sched, cpu, frame) },
        None => frame,
    };
    // The tick only preempts a running task: an idle core takes none, and starts again once it runs one.
    if (tick || idle) && TICKS.load(Relaxed) && !kernel.sched.idle(cpu) {
        arch::timer::arm(TICK_US);
    } else if tick {
        arch::timer::stop();
    }
    kick(&mut kernel.sched, cpu);
    next
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
    let cpu = arch::cpu();
    // SAFETY: the caller's contract.
    let next = unsafe { syscall(kernel, cpu, frame) };
    kick(&mut kernel.sched, cpu);
    next
}

/// Runs the syscall `cpu`'s current process made with `svc`; returns the frame to resume.
///
/// # Safety
/// Trap context (IRQs masked), and `frame` `cpu`'s current process's.
#[inline(always)]
unsafe fn syscall(kernel: &mut Kernel, cpu: usize, frame: &mut arch::TrapFrame) -> usize {
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
    frame.x[0] = match kernel::syscall::dispatch(frame.x[8], args, sched.handles(cpu)) {
        Ok(Call::Exit(code)) => {
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            return unsafe {
                exit_process(kernel, cpu, frame as *mut arch::TrapFrame as usize, code)
            };
        }
        Ok(Call::ThreadExit(code)) => {
            // SAFETY: as above.
            return unsafe {
                exit_thread(kernel, cpu, frame as *mut arch::TrapFrame as usize, code)
            };
        }
        Ok(Call::Thread {
            entry,
            stack,
            tls,
            arg,
        }) => thread(sched, cpu, frames, entry, (stack, tls), arg)
            .unwrap_or_else(|error| error as u64),
        Ok(Call::Write { ptr, len }) => match copy_in(ptr, len, buf) {
            Some(bytes) => {
                if !bytes.is_empty() {
                    CONSOLE.lock_masked().write(bytes);
                }
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
                None => return unsafe { block(kernel, cpu, frame, Event::Console) },
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
            None => return unsafe { block(kernel, cpu, frame, Event::Pipe(end.index as usize)) },
        },
        Ok(Call::NewPipe) => match new_pipe(sched, cpu, frames, pipes) {
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
                let current = sched.process(cpu);
                sched
                    .memory(current)
                    .budget
                    .grow(limit.saturating_sub(held));
                code
            }
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            Ok(None) => return unsafe { block(kernel, cpu, frame, Event::Exit(index)) },
            Err(error) => error as u64,
        },
        Ok(Call::Join { slot, generation }) => match sched.join(slot, generation) {
            Ok(Some(code)) => code,
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            Ok(None) => return unsafe { block(kernel, cpu, frame, Event::Join(slot)) },
            Err(error) => error as u64,
        },
        Ok(Call::Dup { handle, object }) => {
            match object {
                Object::Pipe(end) => pipes.open(end),
                Object::Mutex(mutex) => mutexes.open(mutex),
                Object::Socket(sock) => net::open(sock),
                object => sched.held(object),
            }
            handle
        }
        Ok(Call::Close(object)) => {
            let current = sched.process(cpu);
            release(sched, cpu, frames, pipes, mutexes, (object, current));
            0
        }
        Ok(Call::Map { pages }) => map(sched, cpu, frames, pages).unwrap_or(ENOMEM as u64),
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
        .and_then(|object| sched.handles(cpu).insert(object, rights))
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
            (sched, cpu),
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
                .handles(cpu)
                .insert(Object::Mutex(mutex), DUPLICATE | TRANSFER)
                .unwrap_or_else(|error| {
                    mutexes.close(mutex);
                    error as u64
                }),
            None => ENFILE as u64,
        },
        Ok(Call::Lock(mutex)) => match mutexes.lock(mutex, sched.current(cpu).0) {
            Ok(None) => 0,
            Ok(Some(owner)) => {
                sched.boost(cpu, owner);
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                return unsafe { block(kernel, cpu, frame, Event::Lock(mutex.index as usize)) };
            }
            Err(error) => error as u64,
        },
        Ok(Call::Unlock(mutex)) => {
            let (slot, _) = sched.current(cpu);
            match mutexes.unlock(mutex, slot) {
                Ok(()) => {
                    // With no waiter woken, the caller's boost is unchanged and nothing new is ready.
                    if sched.wake(Event::Lock(mutex.index as usize)) > 0 {
                        sched.unboost(
                            slot,
                            |e| matches!(e, Event::Lock(i) if mutexes.owner(i) == Some(slot)),
                        );
                        if sched.outranked(cpu) && sched.marked(cpu).is_none() {
                            frame.x[0] = 0;
                            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                            return unsafe {
                                switch(sched, cpu, frame as *mut arch::TrapFrame as usize)
                            };
                        }
                    }
                    0
                }
                Err(error) => error as u64,
            }
        }
        Ok(Call::Kill { index, generation }) => match sched.process_live(index, generation) {
            Ok(true) if index == sched.process(cpu) => {
                let frame = frame as *mut arch::TrapFrame as usize;
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                return unsafe { exit_process(kernel, cpu, frame, KILLED) };
            }
            Ok(true) => {
                kill(kernel, cpu, index);
                0
            }
            Ok(false) => 0,
            Err(error) => error as u64,
        },
        Ok(Call::KillThread { slot, generation }) => match sched.thread_live(slot, generation) {
            Ok(true) if slot == sched.current(cpu).0 => {
                let frame = frame as *mut arch::TrapFrame as usize;
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                return unsafe { exit_thread(kernel, cpu, frame, KILLED) };
            }
            // Not the current thread, so its last thread is in another process.
            Ok(true) if sched.threads(sched.process_of(slot)) == 1 => {
                let index = sched.process_of(slot);
                kill(kernel, cpu, index);
                0
            }
            Ok(true) if sched.core_of(slot).is_some() => {
                end_remote(sched, slot, KILLED);
                0
            }
            Ok(true) => {
                end_thread(sched, frames, mutexes, (cpu, slot), KILLED);
                0
            }
            Ok(false) => 0,
            Err(error) => error as u64,
        },
        Ok(Call::Net(call)) => {
            match net::syscall(sched, cpu, call, (&mut frame.x[1..3]).try_into().unwrap()) {
                Some(result) => result as u64,
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                None => return unsafe { block(kernel, cpu, frame, Event::NetIo) },
            }
        }
        Err(error) => error as u64,
    };
    frame as *mut arch::TrapFrame as usize
}

/// # Safety
/// Trap context (IRQs masked), and `frame` the current process's. Returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_user_fault(frame: usize, ec: u64, far: u64) -> usize {
    let kernel = Guard::leak(KERNEL.lock_masked());
    let cpu = arch::cpu();
    let index = kernel.sched.process(cpu);
    let _ = writeln!(
        CONSOLE.lock_masked(),
        "fault: {index} ec={ec:#x} far={far:#x}"
    );
    // SAFETY: the caller masked IRQs; `frame` is the current process's.
    let next = unsafe { exit_process(kernel, cpu, frame, KILLED) };
    kick(&mut kernel.sched, cpu);
    next
}
