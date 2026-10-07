use mm::{Budget, PhysAddr};

use crate::handle::{Handles, Object};
use crate::syscall::EBADF;

/// Priority levels: 0 (lowest; the boot context and kernel tasks) to `PRIORITIES - 1`.
pub const PRIORITIES: u8 = 4;

/// No room for the task: the run queue is full or its memory could not be allocated.
#[derive(Debug)]
pub struct Full;

/// A process's budget, which pays for its threads' kernel stacks too, and where its next `map` goes. Handle tables
/// are fixed arrays in the process table, so they are not charged.
pub struct Memory {
    pub budget: Budget,
    pub next: u64,
}

const NO_MEMORY: Memory = Memory {
    budget: Budget::new(0),
    next: 0,
};

/// What a blocked task waits for; `wake` makes every task waiting for it ready.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Event {
    /// Data, room or a closed end in the pipe at this table index.
    Pipe(usize),
    /// The process at this index ending.
    Exit(usize),
    /// The thread in this slot ending.
    Join(usize),
    /// The mutex at this table index being unlocked.
    Lock(usize),
    /// A console line being entered.
    Console,
    /// Work for the net task: a frame, a passed deadline or a socket submit.
    Net,
    /// A net task pass, after which a socket op may finish.
    NetIo,
    /// Nothing: the boot context, which runs only once no other task is ready.
    Idle,
}

/// A thread's state; a process is `Ready` while it has threads, and never `Blocked`.
#[derive(Clone, Copy, PartialEq)]
enum State {
    Ready,
    Blocked(Event),
    /// Free; what was last here ended with this code.
    Exited(u64),
    /// Ended with this code, kept until it is reaped (`reap`, `join`) or the last handle to it closes.
    Zombie(u64),
}

/// The lifecycle of a table's entries (thread slots or process indices): state, generation, the handles that reach
/// each, and which are free, all kept in step so no call scans for them. Entry 0 never ends.
struct Entries<const M: usize> {
    state: [State; M],
    /// Bumped by each `start`, so a handle reaches one occupant, never a later one in the same place.
    generation: [u64; M],
    /// Open handles to the occupant of this generation: it stays a zombie while any is left.
    holders: [u32; M],
    /// A bit per `Exited` entry.
    free: u64,
}

impl<const M: usize> Entries<M> {
    const fn new() -> Self {
        assert!(M <= 64, "the free set is one word");
        let mut state = [State::Exited(0); M];
        state[0] = State::Ready;
        Self {
            state,
            generation: [0; M],
            holders: [0; M],
            free: (u64::MAX >> (64 - M)) & !1,
        }
    }

    /// The first free entry and the generation its next occupant gets.
    fn free(&self) -> Option<(usize, u64)> {
        let i = self.free.trailing_zeros() as usize;
        (i < M).then(|| (i, self.generation[i] + 1))
    }

    fn start(&mut self, (i, generation): (usize, u64)) {
        self.state[i] = State::Ready;
        self.generation[i] = generation;
        self.holders[i] = 0;
        self.free &= !(1 << i);
    }

    /// Ends entry `i` with `code`: a zombie while a handle reaches it, else free.
    fn end(&mut self, i: usize, code: u64) {
        match self.holders[i] {
            0 => self.exit(i, code),
            _ => self.state[i] = State::Zombie(code),
        }
    }

    fn exit(&mut self, i: usize, code: u64) {
        self.state[i] = State::Exited(code);
        self.free |= 1 << i;
    }

    /// For entry `i` reached with `generation`: whether it runs; `EBADF` once a newer occupant took it.
    fn live(&self, i: usize, generation: u64) -> Result<bool, i64> {
        if self.generation[i] != generation {
            return Err(EBADF);
        }
        Ok(matches!(self.state[i], State::Ready | State::Blocked(_)))
    }

    /// As `live`, but once the entry ended, frees it and returns its code.
    fn reap(&mut self, i: usize, generation: u64) -> Result<Option<u64>, i64> {
        if self.generation[i] != generation {
            return Err(EBADF);
        }
        let (State::Exited(code) | State::Zombie(code)) = self.state[i] else {
            return Ok(None);
        };
        self.exit(i, code);
        Ok(Some(code))
    }

    /// A handle to entry `i` with `generation` was opened.
    fn held(&mut self, i: usize, generation: u64) {
        if self.generation[i] == generation {
            self.holders[i] += 1;
        }
    }

    /// A handle to entry `i` with `generation` was closed: whether that was the last one to a zombie.
    fn dropped(&mut self, i: usize, generation: u64) -> bool {
        if self.generation[i] != generation {
            return false;
        }
        self.holders[i] -= 1;
        self.holders[i] == 0 && matches!(self.state[i], State::Zombie(_))
    }
}

/// The process table: per index, an address space (its level-1 table; `PhysAddr(0)` is the boot table), handles,
/// memory and live threads. Index 0 is the kernel: the boot context and kernel tasks, in the boot table.
pub struct Processes<const P: usize> {
    space: [PhysAddr; P],
    handles: [Handles; P],
    /// An ended process's budget stays here until `reap`.
    memory: [Memory; P],
    /// A bit per slot of its live threads.
    threads: [u64; P],
    entries: Entries<P>,
}

/// A core's current slot while it runs its idle context.
const IDLE: usize = usize::MAX;
/// The bit of slot `i` (below 64) in a slot mask; no overflow check on the hot paths.
#[inline]
fn bit(i: usize) -> u64 {
    1u64.wrapping_shl(i as u32)
}

/// `Scheduler::woken`'s flag for core 0's signal.
const RESUME_BOOT: usize = 1 << (usize::BITS - 1);

/// What one core runs: a slot (`IDLE` for its idle context), that slot's process (0, the boot table, while idle), the
/// idle context's saved frame, and whether it was signalled since it last went idle.
#[derive(Clone, Copy)]
struct Core {
    current: usize,
    process: usize,
    idle: usize,
    kicked: bool,
}

/// Run queue of up to `N` threads of up to `P` processes, each thread known by its saved trap frame address, process,
/// kernel stack and priority, shared by every core. A core runs the highest-priority ready thread no other core runs,
/// round robin within a level; blocked ones are skipped; with none, its idle context, but core 0 runs the boot
/// context (slot 0, never on another core) once no core runs a thread.
pub struct Scheduler<const N: usize, const P: usize> {
    frame: [usize; N],
    process: [usize; N],
    /// First frame of the kernel stack, charged to the thread's process.
    stack: [PhysAddr; N],
    slots: Entries<N>,
    /// Own priority per slot.
    priority: [u8; N],
    /// Own priority, raised while a higher-priority task waits for a mutex the slot owns.
    effective: [u8; N],
    /// One past the highest slot ever used.
    end: usize,
    /// Per core, sized by `start_cores` and never freed (a slice, so indexing it is inline); a core's `process` caches
    /// `process[current]`, which the syscall, switch and exit paths read on every call.
    cores: &'static mut [Core],
    /// A bit per slot some core runs.
    running: u64,
    /// A bit per slot to end with its `code` on its own core, which runs it, at its next switch or IRQ.
    marked: u64,
    code: [u64; N],
    /// Tasks made ready (`add`, `wake`) since `take_woken`, as many idle cores may be signalled; `RESUME_BOOT` set once
    /// every core went idle while the boot context waits, so core 0 is to be signalled. One word: one test when 0.
    woken: usize,
    processes: Processes<P>,
}

impl<const N: usize, const P: usize> Scheduler<N, P> {
    /// Slot 0 is the boot context, the kernel process's first thread; its frame is recorded on its first switch.
    pub const fn new() -> Self {
        let mut threads = [0; P];
        threads[0] = 1;
        Self {
            frame: [0; N],
            process: [0; N],
            stack: [PhysAddr(0); N],
            slots: Entries::new(),
            priority: [0; N],
            effective: [0; N],
            end: 1,
            cores: &mut [],
            running: 1,
            marked: 0,
            code: [0; N],
            woken: 0,
            processes: Processes {
                space: [PhysAddr(0); P],
                handles: [Handles::new(); P],
                memory: [NO_MEMORY; P],
                threads,
                entries: Entries::new(),
            },
        }
    }

    /// Gives the scheduler `cpus` cores: core 0 runs the boot context, its idle context's first frame at `idle_frame`;
    /// the others start idle and counted as signalled (an SGI before a core's interrupt controller is up is lost), so
    /// each must reschedule once it is up; their idle frame is saved by that first switch.
    pub fn start_cores(&mut self, cpus: usize, idle_frame: usize) {
        let idle = Core {
            current: IDLE,
            process: 0,
            idle: 0,
            kicked: true,
        };
        self.cores = alloc::vec![idle; cpus].leak();
        self.cores[0] = Core {
            current: 0,
            idle: idle_frame,
            kicked: false,
            ..idle
        };
    }

    /// Starts a process with no threads yet at `index` with `generation` (from `free_process`), in address space
    /// `space`, with `memory` and `handles`.
    pub fn add_process(
        &mut self,
        (index, generation): (usize, u64),
        space: PhysAddr,
        memory: Memory,
        handles: Handles,
    ) {
        let p = &mut self.processes;
        p.space[index] = space;
        p.handles[index] = handles;
        p.memory[index] = memory;
        p.threads[index] = 0;
        p.entries.start((index, generation));
    }

    /// Queues a new thread of the live `process` in `slot` with `generation` (from `free_slot`), its first frame at
    /// `frame`, its kernel stack at `stack`, at `priority` (below `PRIORITIES`).
    pub fn add(
        &mut self,
        (slot, generation): (usize, u64),
        process: usize,
        (frame, stack): (usize, PhysAddr),
        priority: u8,
    ) {
        self.frame[slot] = frame;
        self.process[slot] = process;
        self.stack[slot] = stack;
        self.slots.start((slot, generation));
        self.priority[slot] = priority;
        self.effective[slot] = priority;
        self.end = self.end.max(slot + 1);
        self.processes.threads[process] |= 1 << slot;
        self.woken += 1;
    }

    /// The slot the next `add` takes, if any is free, and the generation it gives the thread there.
    pub fn free_slot(&self) -> Option<(usize, u64)> {
        self.slots.free()
    }

    /// The index the next `add_process` takes, if any is free, and the generation it gives the process there.
    pub fn free_process(&self) -> Option<(usize, u64)> {
        self.processes.entries.free()
    }

    /// Saves `cpu`'s current `frame` (a task's or its idle context's) and returns the next one it runs.
    #[inline]
    pub fn switch(&mut self, cpu: usize, frame: usize) -> usize {
        let core = &mut self.cores[cpu];
        match core.current {
            IDLE => core.idle = frame,
            current => self.frame[current] = frame,
        }
        self.advance(cpu)
    }

    /// Marks `cpu`'s current task blocked until `wake(event)`; the caller switches away.
    pub fn block(&mut self, cpu: usize, event: Event) {
        self.slots.state[self.cores[cpu].current] = State::Blocked(event);
    }

    /// Makes every task waiting for `event` ready; returns how many.
    pub fn wake(&mut self, event: Event) -> usize {
        let mut woke = 0;
        for state in &mut self.slots.state[..self.end] {
            if *state == State::Blocked(event) {
                *state = State::Ready;
                woke += 1;
            }
        }
        self.woken += woke;
        woke
    }

    /// The cores to signal: the tasks made ready since the last call that are still ready and run nowhere (the caller
    /// may have switched to one), if any core is left to signal, and core 0 if the boot context may resume.
    #[inline]
    pub fn take_woken(&mut self) -> usize {
        if self.woken == 0 {
            return 0;
        }
        let woken = core::mem::take(&mut self.woken);
        let boot = (woken & RESUME_BOOT != 0) as usize;
        let woken = woken & !RESUME_BOOT;
        if woken == 0 || !self.cores.iter().any(|c| c.current == IDLE && !c.kicked) {
            return boot;
        }
        let waiting = (1..self.end)
            .filter(|&s| self.slots.state[s] == State::Ready && self.running & 1 << s == 0)
            .count();
        woken.min(waiting) + boot
    }

    /// An idle core other than `cpu`, not signalled since it went idle, now marked signalled: the caller sends it the
    /// reschedule SGI. `cpu` needs none: an idle core reschedules at the end of every trap.
    pub fn claim_idle(&mut self, cpu: usize) -> Option<usize> {
        let (i, core) = (self.cores.iter_mut().enumerate())
            .find(|(i, c)| c.current == IDLE && !c.kicked && *i != cpu)?;
        core.kicked = true;
        Some(i)
    }

    /// Whether `cpu` runs its idle context.
    #[inline(always)]
    pub fn idle(&self, cpu: usize) -> bool {
        self.cores[cpu].current == IDLE
    }

    /// Whether the boot context may be waiting for `count()` to drop: core 0 idles or runs it.
    pub fn boot_waits(&self) -> bool {
        matches!(self.cores[0].current, 0 | IDLE)
    }

    /// The core that runs the thread in `slot`, if any.
    pub fn core_of(&self, slot: usize) -> Option<usize> {
        if self.running & 1 << slot == 0 {
            return None;
        }
        self.cores.iter().position(|c| c.current == slot)
    }

    /// Marks the thread in `slot`, which another core runs, to end there with `code`.
    pub fn mark(&mut self, slot: usize, code: u64) {
        self.marked |= 1 << slot;
        self.code[slot] = code;
    }

    /// The code `cpu`'s current thread is marked to end with, if it is.
    #[inline(always)]
    pub fn marked(&self, cpu: usize) -> Option<u64> {
        let current = self.cores[cpu].current;
        (current != IDLE && self.marked & bit(current) != 0).then(|| self.code[current])
    }

    /// Ends the live thread in `slot` (never slot 0) with `code`, a zombie while a handle reaches it, and wakes its
    /// joiners; returns its kernel stack and the event it was blocked on. With its last thread its process ends alike,
    /// so its own handles must already be released (`take_handles`).
    pub fn end(&mut self, slot: usize, code: u64) -> (PhysAddr, Option<Event>) {
        assert!(slot != 0, "the boot context cannot exit");
        self.marked &= !(1 << slot);
        let blocked = match self.slots.state[slot] {
            State::Blocked(event) => Some(event),
            _ => None,
        };
        self.slots.end(slot, code);
        self.wake(Event::Join(slot));
        let index = self.process[slot];
        debug_assert!(
            self.processes.threads[index] & 1 << slot != 0,
            "slot {slot} ended twice"
        );
        self.processes.threads[index] &= !(1 << slot);
        if self.processes.threads[index] == 0 {
            self.processes.entries.end(index, code);
            self.wake(Event::Exit(index));
        }
        (self.stack[slot], blocked)
    }

    /// Whether any process's handle table holds a handle to an object `f` matches.
    pub fn holds(&self, f: impl Fn(Object) -> bool) -> bool {
        self.processes.handles.iter().any(|h| h.objects().any(&f))
    }

    /// A new handle reaches `object`: a process or thread it names stays a zombie, once it ends, until it closes.
    #[inline]
    pub fn held(&mut self, object: Object) {
        match object {
            Object::Process { index, generation } => self.processes.entries.held(index, generation),
            Object::Thread { slot, generation } => self.slots.held(slot, generation),
            _ => {}
        }
    }

    /// For the process at `index` with `generation`: `None` while it runs; once it ended, its code and its budget's
    /// limit, which only the first call gets (later ones get 0), and its index is freed; `EBADF` once a newer process
    /// took the index.
    pub fn reap(&mut self, index: usize, generation: u64) -> Result<Option<(u64, usize)>, i64> {
        let p = &mut self.processes;
        let Some(code) = p.entries.reap(index, generation)? else {
            return Ok(None);
        };
        let budget = core::mem::replace(&mut p.memory[index].budget, Budget::new(0));
        Ok(Some((code, budget.limit())))
    }

    /// For the thread in `slot` with `generation`: `None` while it runs; once it ended, its code, and its slot is
    /// freed; `EBADF` once a newer thread took the slot.
    pub fn join(&mut self, slot: usize, generation: u64) -> Result<Option<u64>, i64> {
        self.slots.reap(slot, generation)
    }

    /// A handle to the process at `index` with `generation` was closed: if it was the last one to the ended process,
    /// frees its index and returns its budget's limit, as `reap` would; otherwise 0.
    #[inline]
    pub fn close(&mut self, index: usize, generation: u64) -> usize {
        if !self.processes.entries.dropped(index, generation) {
            return 0;
        }
        match self.reap(index, generation) {
            Ok(Some((_, limit))) => limit,
            _ => 0,
        }
    }

    /// A handle to the thread in `slot` with `generation` was closed: if it was the last one to the ended thread,
    /// frees its slot.
    pub fn close_thread(&mut self, slot: usize, generation: u64) {
        if self.slots.dropped(slot, generation) {
            let _ = self.join(slot, generation);
        }
    }

    /// Whether the process at `index` with `generation` still runs; `EBADF` once a newer process took the index.
    pub fn process_live(&self, index: usize, generation: u64) -> Result<bool, i64> {
        self.processes.entries.live(index, generation)
    }

    /// Whether the thread in `slot` with `generation` still runs; `EBADF` once a newer thread took the slot.
    pub fn thread_live(&self, slot: usize, generation: u64) -> Result<bool, i64> {
        self.slots.live(slot, generation)
    }

    /// The budget of the process at `index` with `generation`, unless it ended.
    pub fn budget(&mut self, index: usize, generation: u64) -> Option<&mut Budget> {
        let live = self.process_live(index, generation) == Ok(true);
        live.then_some(&mut self.processes.memory[index].budget)
    }

    /// `cpu`'s current task's slot and generation.
    #[inline]
    pub fn current(&self, cpu: usize) -> (usize, u64) {
        let current = self.cores[cpu].current;
        (current, self.slots.generation[current])
    }

    /// `cpu`'s current task's process index (and ASID); 0 while it idles.
    #[inline(always)]
    pub fn process(&self, cpu: usize) -> usize {
        self.cores[cpu].process
    }

    /// `cpu`'s current task's process generation.
    #[inline]
    pub fn generation(&self, cpu: usize) -> u64 {
        self.processes.entries.generation[self.process(cpu)]
    }

    /// The address space of the process at `index`.
    pub fn space(&self, index: usize) -> PhysAddr {
        self.processes.space[index]
    }

    /// The process the thread in `slot` belongs to.
    pub fn process_of(&self, slot: usize) -> usize {
        self.process[slot]
    }

    /// The live threads of the process at `index`.
    pub fn threads(&self, index: usize) -> usize {
        self.processes.threads[index].count_ones() as usize
    }

    /// A live thread of the process at `index` that no core but `cpu` runs, if it has one.
    pub fn thread_of(&self, index: usize, cpu: usize) -> Option<usize> {
        let threads = self.processes.threads[index] & !self.elsewhere(cpu);
        (threads != 0).then(|| threads.trailing_zeros() as usize)
    }

    /// The live threads of the process at `index` that cores other than `cpu` run, a bit per slot.
    pub fn threads_elsewhere(&self, index: usize, cpu: usize) -> u64 {
        self.processes.threads[index] & self.elsewhere(cpu)
    }

    /// The slots cores other than `cpu` run.
    fn elsewhere(&self, cpu: usize) -> u64 {
        match self.cores[cpu].current {
            IDLE => self.running,
            current => self.running & !(1 << current),
        }
    }

    /// `cpu`'s current task's own priority.
    #[inline]
    pub fn priority(&self, cpu: usize) -> u8 {
        self.priority[self.cores[cpu].current]
    }

    /// `cpu`'s current task waits for a mutex `slot` owns: `slot` runs at least at that task's priority.
    pub fn boost(&mut self, cpu: usize, slot: usize) {
        let current = self.cores[cpu].current;
        self.effective[slot] = self.effective[slot].max(self.effective[current]);
    }

    /// `slot` lost a waiter (unlock or kill): it drops back to its own priority, raised by tasks still waiting for a
    /// mutex it owns (`owns(event)`). One level: a waiter's boost does not pass on to the owner of a mutex that owner
    /// waits for.
    pub fn unboost(&mut self, slot: usize, owns: impl Fn(Event) -> bool) {
        let mut priority = self.priority[slot];
        for (state, &effective) in self.slots.state[..self.end].iter().zip(&self.effective) {
            if let State::Blocked(event) = *state
                && owns(event)
            {
                priority = priority.max(effective);
            }
        }
        self.effective[slot] = priority;
    }

    /// A ready task no core runs beats `cpu`'s current task's effective priority.
    pub fn outranked(&self, cpu: usize) -> bool {
        let current = self.effective[self.cores[cpu].current];
        (0..self.end).any(|s| {
            self.slots.state[s] == State::Ready
                && self.running & 1 << s == 0
                && self.effective[s] > current
        })
    }

    /// The memory of the process at `index`.
    #[inline]
    pub fn memory(&mut self, index: usize) -> &mut Memory {
        &mut self.processes.memory[index]
    }

    /// `cpu`'s current process's handles.
    #[inline(always)]
    pub fn handles(&mut self, cpu: usize) -> &mut Handles {
        &mut self.processes.handles[self.cores[cpu].process]
    }

    /// Empties the handle table of the process at `index`; returns what it held.
    pub fn take_handles(&mut self, index: usize) -> Handles {
        core::mem::take(&mut self.processes.handles[index])
    }

    /// Threads in the queue, the boot context included.
    pub fn count(&self) -> usize {
        let queued = |s: &&State| matches!(s, State::Ready | State::Blocked(_));
        self.slots.state[..self.end].iter().filter(queued).count()
    }

    /// Moves `cpu` to the highest-priority ready task no other core runs, the first after its current one (itself
    /// last) among equals, slot 0 only on core 0; with none, core 0 to the boot context if no core runs a task, else
    /// to its idle context.
    fn advance(&mut self, cpu: usize) -> usize {
        let mut slot = match self.cores[cpu].current {
            IDLE => self.end - 1,
            current => {
                self.running &= !bit(current);
                current
            }
        };
        // Slot 0 is the boot context's, pinned to core 0.
        let free = !self.running & !((cpu != 0) as u64);
        let mut next = None;
        for _ in 0..self.end {
            slot = if slot + 1 == self.end { 0 } else { slot + 1 };
            if matches!(self.slots.state[slot], State::Ready)
                && free & bit(slot) != 0
                && next.is_none_or(|n: usize| self.effective[slot] > self.effective[n])
            {
                next = Some(slot);
            }
        }
        let next = match next {
            Some(slot) => slot,
            None if cpu == 0 && self.running == 0 => 0,
            None => {
                // The boot context waits for every core to idle: signal core 0 (first in `claim_idle`'s order).
                let boot = self.cores[0];
                if cpu != 0 && self.running == 0 && boot.current == IDLE && !boot.kicked {
                    self.woken |= RESUME_BOOT;
                }
                let core = &mut self.cores[cpu];
                (core.current, core.process, core.kicked) = (IDLE, 0, false);
                return core.idle;
            }
        };
        self.slots.state[next] = State::Ready;
        self.running |= bit(next);
        let core = &mut self.cores[cpu];
        (core.current, core.process) = (next, self.process[next]);
        self.frame[next]
    }
}

impl<const N: usize, const P: usize> Default for Scheduler<N, P> {
    fn default() -> Self {
        Self::new()
    }
}
