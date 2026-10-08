//! Trap hooks: switch, IRQ, syscall and user fault, and the thread and process ends and releases they run.

use core::fmt::Write;
use core::sync::atomic::Ordering::{Relaxed, Release};

use arch::{Guard, Resume};
use kernel::handle::{
    DUPLICATE, Handle, Handles, Object, OnlyThread, READ, Seen, TRANSFER, Table, WRITE, Writer,
};
use kernel::pipe::{self, End, Pipes};
use kernel::syscall::{
    Call, EBADF, EFAULT, ENFILE, ENOENT, ENOMEM, IO, KILLED, MAX_BUFFER, NetCall, SHARED_CALLS,
    TABLE_CALLS, dispatch, dispatch_io,
};
use kernel::{Event, file};
use lock_order::{self as level, LockAfter, W};
use mm::PhysAddr;

use crate::TASK_STACK_FRAMES;
use crate::net;
use crate::process::{PROCESSES, ProcessEntry, free_stack, map, spawn, thread};
use crate::usermem::{UserIn, UserOut, copy_in};
use crate::{
    ARCHIVE, BUF, CONSOLE, CURRENT, FRAMES, KERNEL, Kernel, MAX_PIPES, Nospec, PING_SGI, PONGS,
    Sched, TICK_COUNTED, TICK_US, TICKED, TICKS, TIMER_IRQ, UART_IRQ, kick, send, send_sgi,
};

#[unsafe(link_section = ".percpu")]
// SAFETY: in `.percpu`.
/// The kernel stack of the thread this core ended while running on it (its process already refunded), freed once the
/// trap exit has left it (`board_unlock_work`).
static DEFERRED: arch::PerCpu<Option<PhysAddr>> = unsafe { arch::PerCpu::new(None) };

/// # Safety
/// Trap context (IRQs masked), and `frame` the current task's trap frame. Returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn task_switch(frame: usize) -> Resume {
    // SAFETY: a trap hook's entry, which holds no lock.
    let mut root = unsafe { arch::root() };
    let sched = &mut Guard::leak(KERNEL.lock_masked(&mut root)).0.sched;
    let cpu = arch::cpu();
    // SAFETY: the caller masked IRQs, and `frame` came from the trap path.
    let next = unsafe { switch(sched, cpu, frame) };
    kick(sched, cpu);
    Resume::locked(next, false)
}

/// Releases `KERNEL` for a hook that returned holding it, once the trap exit moved to the frame the hook returned, then
/// runs this core's deferred frees: no core frees the stack it runs on, and once `KERNEL` is free another core may take
/// frames.
///
/// # Safety
/// Trap exit only, after a hook that returned holding `KERNEL` through a leaked guard it no longer uses, with its
/// `Resume`'s `locked`.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_unlock() {
    // SAFETY: the caller's contract.
    unsafe { KERNEL.unlock() }
}

/// As `board_unlock`, after freeing the stack this core parked (`DEFERRED`), now that the trap exit left it.
///
/// # Safety
/// As `board_unlock`.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_unlock_work() {
    // SAFETY: trap context.
    if let Some(stack) = unsafe { DEFERRED.with_masked(Option::take) } {
        // Before `KERNEL` goes: core 0 cannot resume the boot context and count free frames until it is free.
        // SAFETY: the trap exit holds only `KERNEL`, and `FRAMES`, the one lock taken under this witness, comes after it.
        free_stack(&mut FRAMES.lock_masked(&mut unsafe { arch::root() }), stack);
    }
    // SAFETY: the caller's contract.
    unsafe { KERNEL.unlock() }
}

/// Releases the process whose last thread this hold ended (`Kernel::release`), once no core runs it: its handles, its
/// address space and ASID, and only then makes it reapable (`exited`), all in the hold that ended it, so nothing sees it
/// half released, `wait` returns after its frames are free and its index is not reused before. Then, from `next`, the
/// frame this core resumes, switches again if what the release woke should run instead: this core idles, or a ready
/// task outranks its own, which is not marked to end.
///
/// Returns the frame to resume and whether a kernel stack is parked for the trap exit.
///
/// # Safety
/// Trap context, holding `KERNEL` through `kernel`, every switch of this hook made, and `next` the frame it resumes.
#[inline(always)]
unsafe fn finish_release(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    next: usize,
) -> (usize, bool) {
    if !kernel.ended {
        return (next, false);
    }
    // SAFETY: the caller's contract.
    unsafe { after_end(kernel, w, cpu, next) }
}

/// `finish_release` once this hold ended a thread, out of line.
///
/// # Safety
/// As `finish_release`.
#[cold]
#[inline(never)]
unsafe fn after_end(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    mut next: usize,
) -> (usize, bool) {
    kernel.ended = false;
    if let Some(release) = kernel.release.take() {
        release_process(kernel, w, release);
        let sched = &kernel.sched;
        // A marked thread stays: the signal its killer sent ends it here.
        if sched.idle(cpu) || (sched.outranked(cpu) && sched.marked(cpu).is_none()) {
            // SAFETY: the caller's contract.
            next = unsafe { switch(&mut kernel.sched, cpu, next) };
        }
    }
    if core::mem::take(&mut kernel.signal_boot) {
        send_sgi(0);
    }
    // SAFETY: trap context.
    (next, unsafe {
        DEFERRED.with_masked(|parked| parked.is_some())
    })
}

/// `finish_release`'s release.
fn release_process(kernel: &mut Kernel, w: &mut W<'_, level::Kernel>, (index, code): (usize, u64)) {
    let entry = &PROCESSES[index];
    let only = entry.alone().expect("no thread left");
    // SAFETY: the process's last thread ended under the `KERNEL` this hold has, so no thread of it holds or takes its
    // lock, and no other process takes it.
    let mut process = unsafe { entry.unshared(only) };
    entry.handles.take(&mut process, |object| {
        release(kernel, w, (object, index), None)
    });
    let mut frames = FRAMES.lock_masked(w);
    let l1 = kernel.sched.space(index);
    arch::flush_asid(index);
    // SAFETY: no thread of the process is left, so no TTBR0 is `l1`, and its tables hold only its frames.
    unsafe { arch::free_space(l1, |f| frames.free(f)) };
    drop(frames);
    kernel.sched.exited(index, code);
    // The boot context may wait for the task count, which counted this release.
    kernel.signal_boot |= arch::cpu() != 0 && kernel.sched.boot_waits();
}

/// Saves `cpu`'s current `frame` and enters the next ready task, or its idle context (process 0); returns its frame.
/// SP_EL0 and TPIDR_EL0 move unless it is the same task or both are kernel tasks (each thread has its own); TTBR0
/// (and `CURRENT`) only if the process changed: the kernel's boot table keeps ASID 0, a process's level-1 table has
/// ASID = its index.
///
/// # Safety
/// IRQs must be masked (trap context), and `frame` `cpu`'s current trap frame.
unsafe fn switch(sched: &mut Sched, cpu: usize, frame: usize) -> usize {
    let target = sched.switch(cpu, frame);
    // SAFETY: the caller's contract.
    unsafe { enter(sched, frame, target) }
}

/// As `switch`, after ending threads: a core that ended a process's last thread goes to its idle context instead (off
/// the process's address space), so `finish_release` releases the process and then picks the task to run, which the
/// release may have woken, with no other core signalled for it.
///
/// # Safety
/// As `switch`.
unsafe fn switch_after_end(kernel: &mut Kernel, cpu: usize, frame: usize) -> usize {
    let target = match kernel.release {
        Some(_) => kernel.sched.to_idle(cpu, frame),
        None => kernel.sched.switch(cpu, frame),
    };
    // SAFETY: the caller's contract.
    unsafe { enter(&mut kernel.sched, frame, target) }
}

/// Moves this core from `frame` to `next`, from process `from` to `to`, as `Scheduler::switch` chose.
///
/// # Safety
/// As `switch`.
#[inline(always)]
unsafe fn enter(sched: &mut Sched, frame: usize, (next, from, to): (usize, usize, usize)) -> usize {
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
        // SAFETY: the caller masked IRQs.
        unsafe { CURRENT.with_masked(|current| *current = to) };
    }
    next
}

/// Ends the thread in `slot`, which no core but `cpu` runs, with `code`: frees the mutexes it owns, drops the boost it
/// lent, and refunds its kernel stack to its process, freeing its frames at once or, if `cpu` runs on it, once the trap
/// exit left it. Its process's last thread leaves the process's release to the hook's `finish_release`. The boot
/// context may wait for the task count to drop, so core 0 is signalled once, at the end of the hook (`after_end`),
/// however many threads the hold ended.
fn end_thread(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    (cpu, slot): (usize, usize),
    code: u64,
) {
    let Kernel {
        sched,
        mutexes,
        ended,
        release,
        signal_boot,
        ..
    } = kernel;
    let process = sched.process_of(slot);
    let (stack, blocked, last) = sched.end(slot, code);
    PROCESSES[process].threads.fetch_sub(1, Release);
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
    // The refund now, while `KERNEL` keeps the process from being reaped; the frames maybe later.
    PROCESSES[process].budget.refund(TASK_STACK_FRAMES);
    let on_it = slot == sched.current(cpu).0;
    if !on_it {
        free_stack(&mut FRAMES.lock_masked(w), stack);
    }
    *signal_boot |= cpu != 0 && sched.boot_waits();
    *ended |= on_it || last || *signal_boot;
    if on_it {
        // SAFETY: trap context, as for every caller.
        let parked = unsafe { DEFERRED.with_masked(|parked| parked.replace(stack)) };
        debug_assert!(parked.is_none(), "two stacks in one trap");
    }
    if last {
        debug_assert!(release.is_none(), "two releases in one hold");
        *release = Some((process, code));
    }
}

/// Ends the process at `index` with `code` from `cpu`: ends every thread no other core runs, and marks the others to
/// end on their cores, signalling them; whichever core ends its last thread releases it.
fn end_process(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    index: usize,
    code: u64,
) {
    while let Some(slot) = kernel.sched.thread_of(index, cpu) {
        end_thread(kernel, w, (cpu, slot), code);
    }
    let mut left = kernel.sched.threads_elsewhere(index, cpu);
    while left != 0 {
        end_remote(&mut kernel.sched, left.trailing_zeros() as usize, code);
        left &= left - 1;
    }
}

/// Ends the thread in `slot`, which another core runs, with `code` on that core: marks it and signals the core.
fn end_remote(sched: &mut Sched, slot: usize, code: u64) {
    sched.mark(slot, code);
    send_sgi(sched.core_of(slot).expect("runs elsewhere"));
}

/// Ends `cpu`'s current process with `code` and returns the next task's frame.
///
/// # Safety
/// IRQs must be masked (trap context), `cpu`'s current task must be a process's, and `frame` its trap frame.
unsafe fn exit_process(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    frame: usize,
    code: u64,
) -> usize {
    let index = kernel.sched.process(cpu);
    // Before `switch` picks the next task, so a task this wakes can be it.
    end_process(kernel, w, cpu, index, code);
    // SAFETY: the caller's contract.
    unsafe { switch_after_end(kernel, cpu, frame) }
}

/// Ends `cpu`'s current thread with `code`, or its process with its last thread; returns the next task's frame.
///
/// # Safety
/// As `exit_process`.
unsafe fn exit_thread(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    frame: usize,
    code: u64,
) -> usize {
    if kernel.sched.threads(kernel.sched.process(cpu)) == 1 {
        // SAFETY: the caller's contract.
        return unsafe { exit_process(kernel, w, cpu, frame, code) };
    }
    let slot = kernel.sched.current(cpu).0;
    end_thread(kernel, w, (cpu, slot), code);
    // SAFETY: the caller's contract.
    unsafe { switch_after_end(kernel, cpu, frame) }
}

/// Blocks `cpu`'s current process on `event` with its `svc` rewound, so the call runs again once woken; returns the
/// next task's frame. A thread marked to end ends instead.
///
/// # Safety
/// IRQs must be masked (trap context), and `frame` `cpu`'s current process's.
unsafe fn block(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    frame: &mut arch::TrapFrame,
    event: Event,
) -> usize {
    let at = frame as *mut arch::TrapFrame as usize;
    if let Some(code) = kernel.sched.marked(cpu) {
        // SAFETY: the caller's contract.
        return unsafe { exit_thread(kernel, w, cpu, at, code) };
    }
    frame.restart();
    kernel.sched.block(cpu, event);
    // SAFETY: the caller masked IRQs, and `frame` is `cpu`'s current process's.
    unsafe { switch(&mut kernel.sched, cpu, at) }
}

/// Drops one handle to `object` that the process at `holder` held, `rest` its table after (`None`: all of it is going):
/// an exited process frees its index and, as `wait` does, moves its budget to `holder`; an ended thread frees its slot;
/// a pipe wakes its waiters and, once no handle reaches it, frees its page, refunding its creator if that still runs;
/// the last handle to a mutex frees it; a directory or file stops counting it.
fn release(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    (object, holder): (Object, usize),
    rest: Option<&Handles>,
) {
    let Kernel {
        sched,
        pipes,
        mutexes,
        opens,
        ..
    } = kernel;
    let end = match object {
        Object::Pipe(end) => end,
        Object::Mutex(mutex) => return mutexes.close(mutex),
        Object::Socket(sock) => {
            let last = rest.is_none_or(|h| !h.objects().any(|o| o == object));
            return net::close((sched, w), sock, holder, last);
        }
        Object::Process { index, generation } => {
            if sched.close(index, generation) {
                reaped(pipes, (index, generation), holder);
            }
            return;
        }
        Object::Thread { slot, generation } => return sched.close_thread(slot, generation),
        Object::Dir(_) | Object::Node(_) => return opens.close(object),
        _ => return,
    };
    if let Some((page, (index, generation))) = pipes.close(end) {
        let mut frames = FRAMES.lock_masked(w);
        match sched.process_live(index, generation) {
            Ok(true) => PROCESSES[index].budget.free(&mut frames, page),
            _ => frames.free(page),
        }
    }
    sched.wake(Event::Pipe(end.index as usize));
}

/// The exited process `child` was freed (reaped, or its last handle closed): its budget's limit moves to `holder`, but
/// for the pipe pages still charged to it, which stay held until those pipes close.
#[inline(always)]
fn reaped(pipes: &mut Pipes<MAX_PIPES>, child: (usize, u64), holder: usize) {
    let limit = PROCESSES[child.0].budget.take();
    let held = pipes.charged_to(child);
    PROCESSES[holder].budget.grow(limit.saturating_sub(held));
}

/// Creates a pipe whose page is charged to `cpu`'s current process, which gets a handle to each end (read, write).
fn new_pipe(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    (table, process): (&Table, &mut impl Writer),
) -> Result<(u64, u64), i64> {
    let Kernel { sched, pipes, .. } = kernel;
    let read = pipes.free().ok_or(ENFILE)?;
    let write = End {
        write: true,
        ..read
    };
    let [r, w_at] = table.reserve(process)?;
    let creator = (sched.process(cpu), sched.generation(cpu));
    let budget = &PROCESSES[creator.0].budget;
    let page = budget.alloc(&mut FRAMES.lock_masked(w)).ok_or(ENOMEM)?;
    pipes.create(read, page, creator);
    let rights = DUPLICATE | TRANSFER;
    Ok((
        table.fill(process, r, Object::Pipe(read), READ | rights),
        table.fill(process, w_at, Object::Pipe(write), WRITE | rights),
    ))
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
unsafe extern "C" fn board_irq(frame: usize) -> Resume {
    // SAFETY: a trap hook's entry, which holds no lock.
    let mut root = unsafe { arch::root() };
    let (kernel, mut w) = Guard::leak(KERNEL.lock_masked(&mut root));
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
        net::interrupt(&mut kernel.sched, &mut w);
    } else if irq == UART_IRQ {
        let mut uart = CONSOLE.lock_masked(&mut w);
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
        Some(code) => unsafe { exit_thread(kernel, &mut w, cpu, frame, code) },
        // SAFETY: the caller masked IRQs, and `frame` came from the trap path.
        None if tick || idle => unsafe { switch(&mut kernel.sched, cpu, frame) },
        None => frame,
    };
    // SAFETY: trap context, and `next` the frame this hook resumes.
    let (next, parked) = unsafe { finish_release(kernel, &mut w, cpu, next) };
    // The tick only preempts a running task: an idle core takes none, and starts again once it runs one.
    if (tick || idle) && TICKS.load(Relaxed) && !kernel.sched.idle(cpu) {
        arch::timer::arm(TICK_US);
    } else if tick {
        arch::timer::stop();
    }
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, parked)
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

/// Runs the syscall the current process made with `svc`. A call on shared state alone (`SHARED_CALLS`), or a table call
/// (`TABLE_CALLS`) of a process's only thread, takes `KERNEL` first, as before the split, and returns holding it. The rest look their handles up without a lock: a call that
/// touches only its own process (a console write, `map`) takes no global lock, one that writes the handle table takes
/// the process's lock and then `KERNEL`, and the rest of `io` takes `KERNEL` and returns holding it. An object a
/// lock-free lookup reached is rechecked once the lock that keeps it alive is held: if its entry changed, the call runs
/// again (`restart`), after the change.
///
/// # Safety
/// Trap context (IRQs masked), and `frame` the current process's.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_syscall(frame: &mut arch::TrapFrame) -> Resume {
    // Each a function of its own: `io`'s path saves no register it does not use.
    match frame.x[8] {
        // SAFETY: the caller's contract.
        IO => unsafe { io_call(frame) },
        // SAFETY: the caller's contract.
        nr if nr < 64 && SHARED_CALLS >> nr & 1 != 0 => unsafe { shared_call(frame) },
        nr => {
            // SAFETY: the caller masked IRQs.
            let entry = &PROCESSES[unsafe { CURRENT.with_masked(|current| *current) }];
            // Read before `dispatch`, so a lookup it vouches for needs no recheck; only the caller's own `thread`
            // raises it.
            match entry.alone() {
                // SAFETY: the caller's contract.
                Some(only) if nr < 64 && TABLE_CALLS >> nr & 1 != 0 => unsafe {
                    alone_table_call(frame, entry, only)
                },
                // SAFETY: the caller's contract.
                alone => unsafe { other_call(frame, entry, alone) },
            }
        }
    }
}

/// A call on shared state alone (`SHARED_CALLS`): takes `KERNEL` before its lookups, so a sibling's `close` of an entry
/// they read releases the object only after the call, and returns holding it.
///
/// # Safety
/// As `board_syscall`.
#[inline(never)]
unsafe extern "C" fn shared_call(frame: &mut arch::TrapFrame) -> Resume {
    // SAFETY: a trap hook's call, which holds no lock.
    let mut root = unsafe { arch::root() };
    // SAFETY: the caller's contract.
    unsafe { kernel_first(&mut root, frame, None) }
}

/// A table call of a process's only thread (`entry`'s): `KERNEL` first, no process lock.
///
/// # Safety
/// As `board_syscall`, and the caller is its process's only thread.
#[inline(never)]
#[allow(improper_ctypes_definitions)] // `extern "C"` for the tail call, from Rust only.
unsafe extern "C" fn alone_table_call(
    frame: &mut arch::TrapFrame,
    entry: &ProcessEntry,
    only: OnlyThread,
) -> Resume {
    // SAFETY: a trap hook's call, which holds no lock.
    let mut root = unsafe { arch::root() };
    // SAFETY: the caller's contract.
    unsafe { kernel_first(&mut root, frame, Some((entry, only))) }
}

/// Every call but `io`, the shared ones and an only thread's table calls: the caller's own process's (`entry`), under
/// its lock or none (`alone`, read before any lookup).
///
/// # Safety
/// As `board_syscall`.
#[inline(never)]
#[allow(improper_ctypes_definitions)] // `extern "C"` for the tail call, from Rust only.
unsafe extern "C" fn other_call(
    frame: &mut arch::TrapFrame,
    entry: &ProcessEntry,
    alone: Option<OnlyThread>,
) -> Resume {
    // SAFETY: a trap hook's call, which holds no lock.
    let mut root = unsafe { arch::root() };
    let nr = frame.x[8];
    let mut seen = Seen::default();
    let args = frame.x.first_chunk().unwrap();
    let call = dispatch::<Nospec>(nr, args, &entry.handles, &mut seen);
    let at = frame as *mut arch::TrapFrame as usize;
    frame.x[0] = match call {
        Ok(Call::Map { pages }) => map((entry, alone), &mut root, pages).unwrap_or(ENOMEM as u64),
        // The caller alone: no lock, unless a closed handle's object needs `KERNEL`.
        Ok(Call::Dup {
            object:
                object @ (Object::Console | Object::Archive | Object::File { .. } | Object::NetStack),
            rights,
        }) if let Some(only) = alone => {
            // SAFETY: the caller is its process's only thread, so nothing else reaches the process's data.
            let mut process = unsafe { entry.unshared(only) };
            let handle = entry.handles.insert(&mut process, object, rights);
            handle.unwrap_or_else(|error| error as u64)
        }
        Ok(Call::Close(handle)) if let Some(only) = alone => {
            // SAFETY: as above.
            let mut process = unsafe { entry.unshared(only) };
            let table = (&*entry.handles, &mut process);
            close(&mut root, table, handle).unwrap_or_else(|error| error as u64)
        }
        Ok(
            ref call @ (Call::Dup { .. }
            | Call::Close(_)
            | Call::NewPipe
            | Call::NewMutex
            | Call::Open { .. }
            | Call::Spawn { .. }
            | Call::Thread { .. }
            | Call::Net(NetCall::Socket(_) | NetCall::IoWait)),
        ) => return table_call(&mut root, entry, (alone, &seen), frame, call),
        Ok(_) => unreachable!("a shared call or io"),
        Err(error) => error as u64,
    };
    Resume::unlocked(at)
}

/// `io`, a function of its own so its arms inline: a console write takes `CONSOLE` alone; the rest take `KERNEL`,
/// recheck their lookup and return holding it.
///
/// # Safety
/// As `board_syscall`.
#[inline(never)]
unsafe extern "C" fn io_call(frame: &mut arch::TrapFrame) -> Resume {
    // SAFETY: a trap hook's call, which holds no lock.
    let mut root = unsafe { arch::root() };
    // SAFETY: the caller masked IRQs.
    let entry = &PROCESSES[unsafe { CURRENT.with_masked(|current| *current) }];
    let mut seen = Seen::default();
    let args = frame.x.first_chunk().unwrap();
    let call = dispatch_io::<Nospec>(args, &entry.handles, &mut seen);
    let at = frame as *mut arch::TrapFrame as usize;
    // `Write` first and alone: the hot path tests one discriminant.
    frame.x[0] = match call {
        Ok(Call::Write { ptr, len }) => write(&mut root, ptr, len),
        Ok(Call::Pipe { end, ptr, len }) => {
            return pipe_call(&mut root, entry, &seen, frame, (end, ptr, len));
        }
        Ok(call) => return kernel_call(&mut root, entry, &seen, frame, call),
        Err(error) => error as u64,
    };
    Resume::unlocked(at)
}

/// Pipe I/O, as `kernel_call`, an arm of its own so the call goes straight from `dispatch` to it.
#[inline(always)]
fn pipe_call(
    root: &mut W<'_, level::Unlocked>,
    entry: &ProcessEntry,
    seen: &Seen,
    frame: &mut arch::TrapFrame,
    (end, ptr, len): (End, u64, usize),
) -> Resume {
    let (kernel, mut w) = Guard::leak(KERNEL.lock_masked(root));
    let at = frame as *mut arch::TrapFrame as usize;
    if !entry.handles.unchanged(seen) {
        frame.restart();
        return Resume::locked(at, false);
    }
    let cpu = arch::cpu();
    let next = match pipe_io(&mut kernel.pipes, end, ptr, len) {
        Some(moved) => {
            if moved > 0 {
                kernel.sched.wake(Event::Pipe(end.index as usize));
            }
            frame.x[0] = moved as u64;
            at
        }
        None => {
            // SAFETY: trap context, and `frame` the current process's (`board_syscall`'s contract).
            let next =
                unsafe { block(kernel, &mut w, cpu, frame, Event::Pipe(end.index as usize)) };
            // A marked thread ends instead of blocking, maybe its process's last.
            // SAFETY: trap context, and `next` the frame this hook resumes.
            let (next, parked) = unsafe { finish_release(kernel, &mut w, cpu, next) };
            kick(&mut kernel.sched, cpu);
            return Resume::locked(next, parked);
        }
    };
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, false)
}

/// A call on shared state alone, or a table call of a process's only thread: takes `KERNEL` before its lookups, so a
/// sibling's `close` of an entry they read releases the object only after the call (and the only thread needs no
/// process lock), and returns holding it. `entry` is the caller's process if the caller already has it.
///
/// # Safety
/// As `board_syscall`; for a table call (`TABLE_CALLS`), the caller is its process's only thread.
#[inline(always)]
unsafe fn kernel_first(
    root: &mut W<'_, level::Unlocked>,
    frame: &mut arch::TrapFrame,
    entry: Option<(&ProcessEntry, OnlyThread)>,
) -> Resume {
    let (kernel, mut w) = Guard::leak(KERNEL.lock_masked(root));
    // Before `dispatch`: nothing between it and the match keeps the two from fusing.
    let cpu = arch::cpu();
    let (entry, only) = match entry {
        Some((entry, only)) => (entry, Some(only)),
        None => (&PROCESSES[kernel.sched.process(cpu)], None),
    };
    let args = frame.x.first_chunk().unwrap();
    let call = dispatch::<Nospec>(frame.x[8], args, &entry.handles, &mut Seen::default());
    // SAFETY: the caller's contract.
    let next = unsafe { syscall(kernel, &mut w, (cpu, entry, only), frame, call) };
    // SAFETY: trap context, and `next` the frame this hook resumes.
    let (next, parked) = unsafe { finish_release(kernel, &mut w, cpu, next) };
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, parked)
}

/// `io`'s console read or file call: under `KERNEL`, after rechecking its lookup (`seen`), which it returns holding.
#[inline(always)]
fn kernel_call(
    root: &mut W<'_, level::Unlocked>,
    entry: &ProcessEntry,
    seen: &Seen,
    frame: &mut arch::TrapFrame,
    call: Call,
) -> Resume {
    let (kernel, mut w) = Guard::leak(KERNEL.lock_masked(root));
    if !entry.handles.unchanged(seen) {
        frame.restart();
        return Resume::locked(frame as *mut arch::TrapFrame as usize, false);
    }
    let cpu = arch::cpu();
    // SAFETY: trap context, and `frame` the current process's (`board_syscall`'s contract).
    let next = unsafe { io_locked(kernel, &mut w, cpu, frame, call) };
    // SAFETY: trap context, and `next` the frame this hook resumes.
    let (next, parked) = unsafe { finish_release(kernel, &mut w, cpu, next) };
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, parked)
}

/// A console write: copied in through this core's buffer, written under `CONSOLE` alone.
fn write(root: &mut W<'_, level::Unlocked>, ptr: u64, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    // SAFETY: trap context.
    unsafe {
        BUF.with_masked(|buf| match copy_in(ptr, len, buf) {
            Some(bytes) => {
                CONSOLE.lock_masked(root).write(bytes);
                len as u64
            }
            None => EFAULT as u64,
        })
    }
}

/// A call that writes the current process's handle table: under its lock (unless the caller is its only thread, which
/// then needs neither the lock nor the recheck of its lookups), then `KERNEL` (released before returning, as no table
/// call switches), each table write after every step that can fail. An `io_wait` that must block marks itself blocked
/// under both, then switches in a second hold of `KERNEL`, which it returns holding.
#[inline(never)]
fn table_call(
    root: &mut W<'_, level::Unlocked>,
    entry: &ProcessEntry,
    (alone, seen): (Option<OnlyThread>, &Seen),
    frame: &mut arch::TrapFrame,
    call: &Call,
) -> Resume {
    let at = frame as *mut arch::TrapFrame as usize;
    let table = &*entry.handles;
    let result = if let Some(only) = alone {
        // SAFETY: the caller is its process's only thread, so nothing else reaches the process's data.
        let mut process = unsafe { entry.unshared(only) };
        table_work(root, (table, &mut process), frame, call)
    } else {
        let mut guard = entry.lock.lock_masked(root);
        let (process, mut pw) = guard.parts();
        if !table.unchanged(seen) {
            frame.restart();
            return Resume::unlocked(at);
        }
        table_work(&mut pw, (table, process), frame, call)
    };
    match result {
        Some(result) => {
            frame.x[0] = result.unwrap_or_else(|error| error as u64);
            Resume::unlocked(at)
        }
        // SAFETY: trap context, and `frame` the current process's, marked blocked under the locks just released.
        None => unsafe { block_switch(root, frame) },
    }
}

/// A table call's work with the process's data in hand, `w` the witness of the locks held: its result, or `None` once
/// it marked itself blocked.
#[inline(always)]
fn table_work<P>(
    w: &mut W<'_, P>,
    (table, process): (&Table, &mut impl Writer),
    frame: &mut arch::TrapFrame,
    call: &Call,
) -> Option<Result<u64, i64>>
where
    level::Kernel: LockAfter<P>,
{
    Some(match *call {
        Call::Dup {
            object:
                object @ (Object::Console | Object::Archive | Object::File { .. } | Object::NetStack),
            rights,
        } => table.insert(process, object, rights),
        Call::Close(handle) => close(w, (table, process), handle),
        _ => {
            let mut kernel = KERNEL.lock_masked(w);
            let (kernel, mut w) = kernel.parts();
            let cpu = arch::cpu();
            let result = locked_table_call(kernel, &mut w, cpu, (table, process), frame, call);
            kick(&mut kernel.sched, cpu);
            return result;
        }
    })
}

/// Closes `handle` in `table`, then releases what it reached under `KERNEL` if that needs it; `w` the witness of the
/// locks held.
#[inline(always)]
fn close<P>(
    w: &mut W<'_, P>,
    (table, process): (&Table, &mut impl Writer),
    handle: Handle,
) -> Result<u64, i64>
where
    level::Kernel: LockAfter<P>,
{
    match table.close(process, handle)? {
        Object::Console | Object::Archive | Object::File { .. } | Object::NetStack => Ok(0),
        object => {
            // A socket's holder is refunded once no other handle of its reaches it.
            let rest = matches!(object, Object::Socket(_)).then(|| table.snapshot(process));
            let mut kernel = KERNEL.lock_masked(w);
            let (kernel, mut w) = kernel.parts();
            let cpu = arch::cpu();
            let holder = kernel.sched.process(cpu);
            release(kernel, &mut w, (object, holder), rest.as_ref());
            kick(&mut kernel.sched, cpu);
            Ok(0)
        }
    }
}

/// The second hold of a blocking table call: ends the thread if it was marked meanwhile, else switches away.
///
/// # Safety
/// Trap context, and `frame` the current process's, blocked with its `svc` rewound.
unsafe fn block_switch(root: &mut W<'_, level::Unlocked>, frame: &mut arch::TrapFrame) -> Resume {
    let (kernel, mut w) = Guard::leak(KERNEL.lock_masked(root));
    let cpu = arch::cpu();
    let at = frame as *mut arch::TrapFrame as usize;
    let next = match kernel.sched.marked(cpu) {
        // SAFETY: the caller's contract.
        Some(code) => unsafe { exit_thread(kernel, &mut w, cpu, at, code) },
        // SAFETY: the caller's contract.
        None => unsafe { switch(&mut kernel.sched, cpu, at) },
    };
    // SAFETY: trap context, and `next` the frame this hook resumes.
    let (next, parked) = unsafe { finish_release(kernel, &mut w, cpu, next) };
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, parked)
}

/// A table call's work under the process lock and `KERNEL`: its result, or `None` once it marked itself blocked.
#[inline(always)]
fn locked_table_call(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    (table, process): (&Table, &mut impl Writer),
    frame: &mut arch::TrapFrame,
    call: &Call,
) -> Option<Result<u64, i64>> {
    Some(match *call {
        // Its count after the entry: the process lock keeps any sibling from closing it before.
        Call::Dup { object, rights } => table.insert(process, object, rights).inspect(|_| {
            let Kernel {
                sched,
                pipes,
                mutexes,
                opens,
                ..
            } = kernel;
            match object {
                Object::Pipe(end) => pipes.open(end),
                Object::Mutex(mutex) => mutexes.open(mutex),
                Object::Socket(sock) => net::open(sock, w),
                Object::Dir(_) | Object::Node(_) => opens.open(object),
                object => sched.held(object),
            }
        }),
        Call::NewPipe => new_pipe(kernel, w, cpu, (table, process)).map(|(read, write)| {
            frame.x[1] = write;
            read
        }),
        Call::NewMutex => {
            let mutexes = &mut kernel.mutexes;
            match mutexes.create() {
                Some(mutex) => table
                    .insert(process, Object::Mutex(mutex), DUPLICATE | TRANSFER)
                    .inspect_err(|_| mutexes.close(mutex)),
                None => Err(ENFILE),
            }
        }
        Call::Open {
            dir,
            ptr,
            len,
            flags,
            rights,
        } => {
            let Kernel { fs, opens, .. } = kernel;
            // SAFETY: trap context, and nothing here switches.
            let opened = unsafe {
                BUF.with_masked(|buf| {
                    with_input((ptr, len), buf, |path| match dir {
                        Object::Dir(dir) => file::open(fs, dir, path, flags),
                        _ => kernel::cpio::find(ARCHIVE, path)
                            .map(|file| Object::File {
                                start: file.start,
                                end: file.end,
                            })
                            .ok_or(ENOENT),
                    })
                })
            };
            match opened {
                Ok(object) => {
                    let handle = table.insert(process, object, rights);
                    if handle.is_ok() {
                        opens.open(object);
                    }
                    handle
                }
                Err(error) => Err(error),
            }
        }
        Call::Spawn {
            ref file,
            ptr,
            len,
            budget,
            priority,
            args,
            args_len,
        } => {
            let sched = &mut kernel.sched;
            // SAFETY: trap context, and nothing here switches.
            unsafe {
                BUF.with_masked(|buf| {
                    spawn(
                        (sched, w, cpu),
                        (table, process),
                        buf,
                        file.clone(),
                        (ptr, len.into()),
                        (budget, priority),
                        (args, args_len.into()),
                    )
                })
            }
        }
        Call::Thread {
            entry,
            stack,
            tls,
            arg,
        } => thread(
            (&mut kernel.sched, w, cpu),
            (table, process),
            entry,
            (stack, tls),
            arg,
        ),
        Call::Net(NetCall::Socket(allowed)) => {
            Ok(net::socket((&mut kernel.sched, w), cpu, (table, process), allowed) as u64)
        }
        Call::Net(NetCall::IoWait) => {
            let sched = &mut kernel.sched;
            let out = (&mut frame.x[1..3]).try_into().unwrap();
            match net::io_wait((sched, w), cpu, (table, process), out) {
                Some(result) => Ok(result as u64),
                None => {
                    frame.restart();
                    sched.block(cpu, Event::NetIo);
                    return None;
                }
            }
        }
        _ => unreachable!("not a table call"),
    })
}

/// Runs `io`'s console read or file call of `cpu`'s current process under `KERNEL`; returns the frame to resume.
///
/// # Safety
/// Trap context (IRQs masked), and `frame` `cpu`'s current process's.
#[inline(always)]
unsafe fn io_locked(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    frame: &mut arch::TrapFrame,
    call: Call,
) -> usize {
    let at = frame as *mut arch::TrapFrame as usize;
    let Kernel { line, fs, .. } = kernel;
    frame.x[0] = match call {
        // SAFETY: trap context, and nothing here switches.
        Call::Read { ptr, len } => match unsafe { read_line(line, ptr, len) } {
            Some(n) => n,
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            None => return unsafe { block(kernel, w, cpu, frame, Event::Console) },
        },
        Call::File {
            inode,
            write: true,
            offset,
            ptr,
            len,
        } => {
            // SAFETY: trap context, and nothing here switches.
            let written = unsafe {
                BUF.with_masked(|buf| {
                    with_input((ptr, len), buf, |data| {
                        fs.write(inode, offset, data).map_err(file::errno)
                    })
                })
            };
            written.map_or_else(|error| error as u64, |()| len as u64)
        }
        Call::File {
            inode,
            offset,
            ptr,
            len,
            ..
        } => {
            // SAFETY: trap context, and nothing here switches.
            unsafe {
                BUF.with_masked(|buf| {
                    with_output((ptr, len), buf, |out| {
                        fs.read(inode, offset, out).map_err(file::errno)
                    })
                })
            }
        }
        _ => unreachable!("not io under KERNEL"),
    };
    at
}

/// Runs a syscall of `cpu`'s current process (`entry`) that needs `KERNEL`, or returns `dispatch`'s error; returns the
/// frame to resume.
///
/// # Safety
/// Trap context (IRQs masked), and `frame` `cpu`'s current process's; for a table call (`TABLE_CALLS`), the caller is
/// its process's only thread.
#[inline(always)]
unsafe fn syscall(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    (cpu, entry, only): (usize, &ProcessEntry, Option<OnlyThread>),
    frame: &mut arch::TrapFrame,
    call: Result<Call, i64>,
) -> usize {
    let at = frame as *mut arch::TrapFrame as usize;
    let Kernel {
        sched,
        pipes,
        mutexes,
        fs,
        opens,
        ..
    } = kernel;
    let ok = |result: Result<(), i64>| result.map_or_else(|error| error as u64, |()| 0);
    let call = match call {
        Ok(call) => call,
        Err(error) => {
            frame.x[0] = error as u64;
            return at;
        }
    };
    frame.x[0] = match call {
        Call::Exit(code) => {
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            return unsafe { exit_process(kernel, w, cpu, at, code) };
        }
        Call::ThreadExit(code) => {
            // SAFETY: as above.
            return unsafe { exit_thread(kernel, w, cpu, at, code) };
        }
        Call::Wait { index, generation } => match sched.reap(index, generation) {
            Ok(Some(code)) => {
                reaped(pipes, (index, generation), sched.process(cpu));
                code
            }
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            Ok(None) => return unsafe { block(kernel, w, cpu, frame, Event::Exit(index)) },
            Err(error) => error as u64,
        },
        Call::Join { slot, generation } => match sched.join(slot, generation) {
            Ok(Some(code)) => code,
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            Ok(None) => return unsafe { block(kernel, w, cpu, frame, Event::Join(slot)) },
            Err(error) => error as u64,
        },
        Call::Mkdir { dir, ptr, len } => {
            // SAFETY: trap context, and nothing here switches.
            ok(unsafe {
                BUF.with_masked(|buf| {
                    with_input((ptr, len), buf, |path| file::mkdir(fs, dir, path))
                })
            })
        }
        Call::Readdir {
            dir,
            ptr,
            len,
            cursor,
        } => {
            frame.x[1] = u64::MAX;
            // SAFETY: trap context, and nothing here switches.
            unsafe {
                BUF.with_masked(|buf| {
                    with_output((ptr, len), buf, |out| {
                        let (n, next) = match dir {
                            Object::Dir(dir) => file::readdir(fs, dir, cursor, out),
                            _ => file::list_archive(ARCHIVE, cursor, out),
                        }?;
                        frame.x[1] = next;
                        Ok(n)
                    })
                })
            }
        }
        Call::Unlink { dir, ptr, len } => {
            // SAFETY: trap context, and nothing here switches.
            ok(unsafe {
                BUF.with_masked(|buf| {
                    with_input((ptr, len), buf, |path| {
                        file::unlink(fs, dir, path, |i| opens.held(i))
                    })
                })
            })
        }
        Call::Rename { from, to } => {
            // SAFETY: trap context, and nothing here switches.
            ok(unsafe {
                BUF.with_masked(|buf| {
                    let (from_buf, to_buf) = buf.split_at_mut(MAX_BUFFER as usize);
                    with_input((from.1, from.2), from_buf, |f| {
                        with_input((to.1, to.2), to_buf, |t| {
                            file::rename(fs, (from.0, f), (to.0, t))
                        })
                    })
                })
            })
        }
        Call::Sync => ok(fs.commit().map_err(file::errno)),
        Call::Lock(mutex) => match mutexes.lock(mutex, sched.current(cpu).0) {
            Ok(None) => 0,
            Ok(Some(owner)) => {
                sched.boost(cpu, owner);
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                return unsafe { block(kernel, w, cpu, frame, Event::Lock(mutex.index as usize)) };
            }
            Err(error) => error as u64,
        },
        Call::Unlock(mutex) => {
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
                            return unsafe { switch(sched, cpu, at) };
                        }
                    }
                    0
                }
                Err(error) => error as u64,
            }
        }
        Call::Kill { index, generation } => match sched.process_live(index, generation) {
            Ok(true) if index == sched.process(cpu) => {
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                return unsafe { exit_process(kernel, w, cpu, at, KILLED) };
            }
            Ok(true) => {
                end_process(kernel, w, cpu, index, KILLED);
                0
            }
            Ok(false) => 0,
            Err(error) => error as u64,
        },
        Call::KillThread { slot, generation } => match sched.thread_live(slot, generation) {
            Ok(true) if slot == sched.current(cpu).0 => {
                // SAFETY: the caller masked IRQs, and `frame` is the current process's.
                return unsafe { exit_thread(kernel, w, cpu, at, KILLED) };
            }
            // Not the current thread, so its last thread is in another process.
            Ok(true) if sched.threads(sched.process_of(slot)) == 1 => {
                let index = sched.process_of(slot);
                end_process(kernel, w, cpu, index, KILLED);
                0
            }
            Ok(true) if sched.core_of(slot).is_some() => {
                end_remote(sched, slot, KILLED);
                0
            }
            Ok(true) => {
                end_thread(kernel, w, (cpu, slot), KILLED);
                0
            }
            Ok(false) => 0,
            Err(error) => error as u64,
        },
        call @ (Call::NewPipe
        | Call::NewMutex
        | Call::Open { .. }
        | Call::Spawn { .. }
        | Call::Thread { .. }
        | Call::Net(NetCall::Socket(_) | NetCall::IoWait)) => {
            let only = only.expect("a table call by its process's only thread");
            // SAFETY: the caller is its process's only thread, so nothing else reaches the process's data.
            let mut process = unsafe { entry.unshared(only) };
            let table = (&*entry.handles, &mut process);
            match locked_table_call(kernel, w, cpu, table, frame, &call) {
                Some(result) => result.unwrap_or_else(|error| error as u64),
                // Marked blocked (`io_wait`): a thread marked to end ends instead.
                None => {
                    return match kernel.sched.marked(cpu) {
                        // SAFETY: the caller's contract.
                        Some(code) => unsafe { exit_thread(kernel, w, cpu, at, code) },
                        // SAFETY: the caller's contract.
                        None => unsafe { switch(&mut kernel.sched, cpu, at) },
                    };
                }
            }
        }
        Call::Net(call) => match net::syscall((sched, w), cpu, call) {
            Some(result) => result as u64,
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            None => return unsafe { block(kernel, w, cpu, frame, Event::NetIo) },
        },
        _ => unreachable!("taken without KERNEL"),
    };
    at
}

/// A console line into the user buffer `ptr..ptr + len`, staged in this core's buffer: its length, or `None` until
/// one is entered.
///
/// # Safety
/// Trap context.
#[inline(always)]
unsafe fn read_line(line: &mut kernel::console::Line, ptr: u64, len: usize) -> Option<u64> {
    if len == 0 {
        return Some(0);
    }
    let Some(out) = UserOut::new(ptr, len) else {
        return Some(EFAULT as u64);
    };
    // SAFETY: the caller's contract; nothing here switches.
    unsafe {
        BUF.with_masked(|buf| {
            let n = line.read(&mut buf[..len])?;
            out.write(0, &buf[..n]);
            Some(n as u64)
        })
    }
}

/// # Safety
/// Trap context (IRQs masked), and `frame` the current process's. Returns holding `KERNEL`.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_user_fault(frame: usize, ec: u64, far: u64) -> Resume {
    // SAFETY: a trap hook's entry, which holds no lock.
    let mut root = unsafe { arch::root() };
    let (kernel, mut w) = Guard::leak(KERNEL.lock_masked(&mut root));
    let cpu = arch::cpu();
    let index = kernel.sched.process(cpu);
    let _ = writeln!(
        CONSOLE.lock_masked(&mut w),
        "fault: {index} ec={ec:#x} far={far:#x}"
    );
    // SAFETY: the caller masked IRQs; `frame` is the current process's.
    let next = unsafe { exit_process(kernel, &mut w, cpu, frame, KILLED) };
    // SAFETY: trap context, and `next` the frame this hook resumes.
    let (next, parked) = unsafe { finish_release(kernel, &mut w, cpu, next) };
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, parked)
}
