//! Trap hooks: switch, IRQ, syscall and user fault, and the thread and process ends and releases they run.

use core::fmt::Write;
use core::sync::atomic::Ordering::{Relaxed, Release};

use arch::{Guard, Resume};
use kernel::handle::{DUPLICATE, Handles, Object, READ, Seen, TRANSFER, Table, WRITE};
use kernel::pipe::{self, End, Pipes};
use kernel::syscall::{
    Call, EBADF, EFAULT, EMFILE, ENFILE, ENOENT, ENOMEM, KILLED, MAX_BUFFER, NetCall, dispatch,
};
use kernel::{Event, Process, file};
use lock_order::{self as level, W};
use mm::PhysAddr;

use crate::TASK_STACK_FRAMES;
use crate::net;
use crate::process::{PROCESSES, ProcessEntry, free_stack, map, spawn, thread};
use crate::usermem::{UserIn, UserOut, copy_in};
use crate::{
    ARCHIVE, BUF, CONSOLE, CURRENT, FRAMES, KERNEL, Kernel, MAX_PIPES, Nospec, PING_SGI, PONGS,
    Sched, TICK_COUNTED, TICK_US, TICKED, TICKS, TIMER_IRQ, UART_IRQ, kick, send, send_sgi,
};

/// Frees a core does once off the stack it trapped on (`board_unlock`), from a hook that holds `KERNEL`.
#[derive(Default)]
pub(crate) struct Deferred {
    /// The kernel stack of the thread this core ended while running on it (its process already refunded).
    stack: Option<PhysAddr>,
    /// A process whose last thread ended, and its code: its release (handles, then memory) ends with `exited`.
    release: Option<(usize, u64)>,
}

#[unsafe(link_section = ".percpu")]
// SAFETY: in `.percpu`.
static DEFERRED: arch::PerCpu<Deferred> = unsafe {
    arch::PerCpu::new(Deferred {
        stack: None,
        release: None,
    })
};

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

/// As `board_unlock`, then this core's `Deferred` work, now off the stack it trapped on; a core that idled for a
/// release (`switch_after_end`) then picks a task from `frame`, its idle context's, as a hook does.
///
/// # Safety
/// As `board_unlock`, with `frame` the frame the trap exit moved to.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_unlock_work(frame: usize) -> Resume {
    // SAFETY: trap context.
    let deferred = unsafe { DEFERRED.with_masked(core::mem::take) };
    if let Some(stack) = deferred.stack {
        // Before `KERNEL` goes: core 0 cannot resume the boot context and count free frames until it is free.
        // SAFETY: the trap exit holds only `KERNEL`, and `FRAMES`, the one lock taken under this witness, comes after it.
        free_stack(&mut FRAMES.lock_masked(&mut unsafe { arch::root() }), stack);
    }
    // SAFETY: the caller's contract.
    unsafe { KERNEL.unlock() };
    let Some((index, code)) = deferred.release else {
        return Resume::unlocked(frame);
    };
    // SAFETY: the trap exit, with `KERNEL` released above and no other lock held.
    let mut root = unsafe { arch::root() };
    // The handles first, under the process's lock, which comes before `KERNEL`.
    let entry = &PROCESSES[index];
    let handles = entry.handles.take(&mut entry.lock.lock_masked(&mut root));
    let (kernel, mut w) = Guard::leak(KERNEL.lock_masked(&mut root));
    let cpu = arch::cpu();
    release_process(kernel, &mut w, (index, code), &handles);
    let next = match kernel.sched.idle(cpu) {
        // SAFETY: trap context, and `frame` this core's idle context's.
        true => unsafe { switch(&mut kernel.sched, cpu, frame) },
        false => frame,
    };
    // `board_irq` stopped the tick when this core went idle for the release.
    if TICKS.load(Relaxed) && !kernel.sched.idle(cpu) {
        arch::timer::arm(TICK_US);
    }
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, core::mem::take(&mut kernel.deferred))
}

/// Releases the ended process at `index`, its last thread gone and its `handles` taken from its table: those handles,
/// then its address space and ASID, and only then makes it reapable (`exited`), so
/// `wait` returns after its frames are free and its index is not reused before.
fn release_process(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    (index, code): (usize, u64),
    handles: &Handles,
) {
    for object in handles.objects() {
        release(kernel, w, (object, index), None);
    }
    let mut frames = FRAMES.lock_masked(w);
    let l1 = kernel.sched.space(index);
    arch::flush_asid(index);
    // SAFETY: no thread of the process is left, so no TTBR0 is `l1`, and its tables hold only its frames.
    unsafe { arch::free_space(l1, |f| frames.free(f)) };
    drop(frames);
    kernel.sched.exited(index, code);
    // The boot context may wait for the task count, which counted this release.
    if arch::cpu() != 0 && kernel.sched.boot_waits() {
        send_sgi(0);
    }
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

/// As `switch`, but a core that ended a process's last thread goes to its idle context: it releases the process at the
/// trap exit, which may wake a task better than any it would pick now, and then reschedules (`release_process`).
///
/// # Safety
/// As `switch`.
unsafe fn switch_after_end(sched: &mut Sched, cpu: usize, frame: usize) -> usize {
    // SAFETY: the caller masked IRQs.
    let releasing = unsafe { DEFERRED.with_masked(|deferred| deferred.release.is_some()) };
    let target = match releasing {
        true => sched.to_idle(cpu, frame),
        false => sched.switch(cpu, frame),
    };
    // SAFETY: the caller's contract.
    unsafe { enter(sched, frame, target) }
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
/// lent, and refunds its kernel stack to its process: at once, or, if `cpu` runs on it, once the trap exit left it. Its
/// process's last thread leaves the process's release to the trap exit too. The boot context may wait for the task
/// count to drop, so core 0 is signalled.
fn end_thread(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    (cpu, slot): (usize, usize),
    code: u64,
) {
    let Kernel {
        sched,
        mutexes,
        deferred,
        ..
    } = kernel;
    let process = sched.process_of(slot);
    let (stack, blocked, last) = sched.end(slot, code);
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
    if on_it || last {
        *deferred = true;
        // SAFETY: trap context, as for every caller.
        unsafe {
            DEFERRED.with_masked(|deferred| {
                if on_it {
                    debug_assert!(deferred.stack.is_none(), "two stacks in one trap");
                    deferred.stack = Some(stack);
                }
                if last {
                    debug_assert!(deferred.release.is_none(), "two releases in one trap");
                    deferred.release = Some((process, code));
                }
            })
        };
    }
    if cpu != 0 && sched.boot_waits() {
        send_sgi(0);
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
    unsafe { switch_after_end(&mut kernel.sched, cpu, frame) }
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
    unsafe { switch_after_end(&mut kernel.sched, cpu, frame) }
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
    (table, process): (&Table, &mut Process),
) -> Result<(u64, u64), i64> {
    let Kernel { sched, pipes, .. } = kernel;
    let read = pipes.free().ok_or(ENFILE)?;
    let write = End {
        write: true,
        ..read
    };
    if table.vacant(process) < 2 {
        return Err(EMFILE);
    }
    let creator = (sched.process(cpu), sched.generation(cpu));
    let budget = &PROCESSES[creator.0].budget;
    let page = budget.alloc(&mut FRAMES.lock_masked(w)).ok_or(ENOMEM)?;
    pipes.create(read, page, creator);
    let mut insert =
        |end, rights| table.insert(process, Object::Pipe(end), rights | DUPLICATE | TRANSFER);
    Ok((
        insert(read, READ).expect("vacant"),
        insert(write, WRITE).expect("vacant"),
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
    // The tick only preempts a running task: an idle core takes none, and starts again once it runs one.
    if (tick || idle) && TICKS.load(Relaxed) && !kernel.sched.idle(cpu) {
        arch::timer::arm(TICK_US);
    } else if tick {
        arch::timer::stop();
    }
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, core::mem::take(&mut kernel.deferred))
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

/// Runs the syscall the current process made with `svc`. Its handles are looked up without a lock; a call that touches
/// only its own process (a console write, `map`) takes no global lock, one that writes the handle table takes the
/// process's lock and then `KERNEL`, and the rest take `KERNEL` and return holding it. An object a lookup reached is
/// rechecked once the lock that keeps it alive is held: if its entry changed, the call runs again (`restart`), after
/// the change.
///
/// # Safety
/// Trap context (IRQs masked), and `frame` the current process's.
#[unsafe(no_mangle)]
unsafe extern "C" fn board_syscall(frame: &mut arch::TrapFrame) -> Resume {
    // SAFETY: a trap hook's entry, which holds no lock.
    let mut root = unsafe { arch::root() };
    // SAFETY: the caller masked IRQs.
    let entry = &PROCESSES[unsafe { CURRENT.with_masked(|current| *current) }];
    let mut seen = Seen::default();
    let args = frame.x.first_chunk().unwrap();
    let call = dispatch::<Nospec>(frame.x[8], args, &entry.handles, &mut seen);
    let at = frame as *mut arch::TrapFrame as usize;
    // `Write` first and alone: the hot path tests one discriminant.
    frame.x[0] = match call {
        Ok(Call::Write { ptr, len }) => write(&mut root, ptr, len),
        Ok(Call::Map { pages }) => map(entry, &mut root, pages).unwrap_or(ENOMEM as u64),
        Ok(
            ref call @ (Call::Dup { .. }
            | Call::Close(_)
            | Call::NewPipe
            | Call::NewMutex
            | Call::Open { .. }
            | Call::Spawn { .. }
            | Call::Thread { .. }
            | Call::Net(NetCall::Socket(_) | NetCall::IoWait)),
        ) => return table_call(&mut root, entry, &seen, frame, call),
        Ok(Call::Pipe { end, ptr, len }) => {
            return pipe_call(&mut root, entry, &seen, frame, (end, ptr, len));
        }
        Ok(ref call) => return kernel_call(&mut root, entry, &seen, frame, call),
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
        // SAFETY: trap context, and `frame` the current process's (`board_syscall`'s contract).
        None => unsafe { block(kernel, &mut w, cpu, frame, Event::Pipe(end.index as usize)) },
    };
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, core::mem::take(&mut kernel.deferred))
}

/// A call under `KERNEL` alone, which returns holding it.
#[inline(always)]
fn kernel_call(
    root: &mut W<'_, level::Unlocked>,
    entry: &ProcessEntry,
    seen: &Seen,
    frame: &mut arch::TrapFrame,
    call: &Call,
) -> Resume {
    let (kernel, mut w) = Guard::leak(KERNEL.lock_masked(root));
    if !entry.handles.unchanged(seen) {
        frame.restart();
        return Resume::locked(frame as *mut arch::TrapFrame as usize, false);
    }
    let cpu = arch::cpu();
    // SAFETY: trap context, and `frame` the current process's (`board_syscall`'s contract).
    let next = unsafe { syscall(kernel, &mut w, cpu, frame, call) };
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, core::mem::take(&mut kernel.deferred))
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

/// A call that writes the current process's handle table: under its lock, then `KERNEL` (released before returning,
/// as no table call switches), each table write after every step that can fail. An `io_wait` that must block marks
/// itself blocked under both, then switches in a second hold of `KERNEL`, which it returns holding.
#[inline(never)]
fn table_call(
    root: &mut W<'_, level::Unlocked>,
    entry: &ProcessEntry,
    seen: &Seen,
    frame: &mut arch::TrapFrame,
    call: &Call,
) -> Resume {
    let at = frame as *mut arch::TrapFrame as usize;
    let table = &*entry.handles;
    let mut guard = entry.lock.lock_masked(root);
    let (process, mut pw) = guard.parts();
    if !table.unchanged(seen) {
        frame.restart();
        return Resume::unlocked(at);
    }
    let result = match *call {
        Call::Dup {
            object:
                object @ (Object::Console | Object::Archive | Object::File { .. } | Object::NetStack),
            rights,
        } => table.insert(process, object, rights),
        Call::Close(handle) => match table.close(process, handle) {
            Ok(Object::Console | Object::Archive | Object::File { .. } | Object::NetStack) => Ok(0),
            Ok(object) => {
                let rest = table.snapshot(process);
                let mut kernel = KERNEL.lock_masked(&mut pw);
                let (kernel, mut w) = kernel.parts();
                let cpu = arch::cpu();
                let holder = kernel.sched.process(cpu);
                release(kernel, &mut w, (object, holder), Some(&rest));
                kick(&mut kernel.sched, cpu);
                Ok(0)
            }
            Err(error) => Err(error),
        },
        _ => {
            let result = {
                let mut kernel = KERNEL.lock_masked(&mut pw);
                let (kernel, mut w) = kernel.parts();
                let cpu = arch::cpu();
                let result = locked_table_call(kernel, &mut w, cpu, (table, process), frame, call);
                kick(&mut kernel.sched, cpu);
                result
            };
            match result {
                Some(result) => result,
                None => {
                    drop(guard);
                    // SAFETY: trap context, and `frame` the current process's, marked blocked above.
                    return unsafe { block_switch(root, frame) };
                }
            }
        }
    };
    frame.x[0] = result.unwrap_or_else(|error| error as u64);
    Resume::unlocked(at)
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
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, core::mem::take(&mut kernel.deferred))
}

/// A table call's work under the process lock and `KERNEL`: its result, or `None` once it marked itself blocked.
fn locked_table_call(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    (table, process): (&Table, &mut Process),
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
            opened.and_then(|object| {
                let handle = table.insert(process, object, rights)?;
                opens.open(object);
                Ok(handle)
            })
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

/// Runs a syscall of `cpu`'s current process that needs `KERNEL` but writes no handle table; returns the frame to
/// resume.
///
/// # Safety
/// Trap context (IRQs masked), and `frame` `cpu`'s current process's.
#[inline(always)]
unsafe fn syscall(
    kernel: &mut Kernel,
    w: &mut W<'_, level::Kernel>,
    cpu: usize,
    frame: &mut arch::TrapFrame,
    call: &Call,
) -> usize {
    let at = frame as *mut arch::TrapFrame as usize;
    let Kernel {
        sched,
        pipes,
        mutexes,
        line,
        fs,
        opens,
        ..
    } = kernel;
    let ok = |result: Result<(), i64>| result.map_or_else(|error| error as u64, |()| 0);
    frame.x[0] = match *call {
        Call::Exit(code) => {
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            return unsafe { exit_process(kernel, w, cpu, at, code) };
        }
        Call::ThreadExit(code) => {
            // SAFETY: as above.
            return unsafe { exit_thread(kernel, w, cpu, at, code) };
        }
        // SAFETY: trap context, and nothing here switches.
        Call::Read { ptr, len } => match unsafe { read_line(line, ptr, len) } {
            Some(n) => n,
            // SAFETY: the caller masked IRQs, and `frame` is the current process's.
            None => return unsafe { block(kernel, w, cpu, frame, Event::Console) },
        },
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
            start,
        } => {
            // SAFETY: trap context, and nothing here switches.
            unsafe {
                BUF.with_masked(|buf| {
                    with_output((ptr, len), buf, |out| match dir {
                        Object::Dir(dir) => file::readdir(fs, dir, start, out),
                        _ => file::list_archive(ARCHIVE, start, out),
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
unsafe fn read_line(line: &mut kernel::console::Line, ptr: u64, len: usize) -> Option<u64> {
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
    kick(&mut kernel.sched, cpu);
    Resume::locked(next, core::mem::take(&mut kernel.deferred))
}
