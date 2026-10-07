use mm::{Budget, PhysAddr};

use crate::Clamp;
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
pub struct Processes<const P: usize, C> {
    space: [PhysAddr; P],
    handles: [Handles<C>; P],
    /// An ended process's budget stays here until `reap`.
    memory: [Memory; P],
    /// A bit per slot of its live threads.
    threads: [u64; P],
    entries: Entries<P>,
}

/// Run queue of up to `N` threads of up to `P` processes, each thread known by its saved trap frame address, process,
/// kernel stack and priority. The highest-priority ready thread runs, round robin within a level; blocked ones are
/// skipped; with none ready, the boot context (slot 0) runs. `C` clamps user handle indexes (`Handles`).
pub struct Scheduler<const N: usize, const P: usize, C> {
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
    current: usize,
    /// `process[current]`, cached: the syscall, switch and exit paths read it on every call.
    current_process: usize,
    processes: Processes<P, C>,
}

impl<const N: usize, const P: usize, C: Clamp> Scheduler<N, P, C> {
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
            current: 0,
            current_process: 0,
            processes: Processes {
                space: [PhysAddr(0); P],
                handles: [Handles::new(); P],
                memory: [NO_MEMORY; P],
                threads,
                entries: Entries::new(),
            },
        }
    }

    /// Starts a process with no threads yet at `index` with `generation` (from `free_process`), in address space
    /// `space`, with `memory` and `handles`.
    pub fn add_process(
        &mut self,
        (index, generation): (usize, u64),
        space: PhysAddr,
        memory: Memory,
        handles: Handles<C>,
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
    }

    /// The slot the next `add` takes, if any is free, and the generation it gives the thread there.
    pub fn free_slot(&self) -> Option<(usize, u64)> {
        self.slots.free()
    }

    /// The index the next `add_process` takes, if any is free, and the generation it gives the process there.
    pub fn free_process(&self) -> Option<(usize, u64)> {
        self.processes.entries.free()
    }

    /// Saves the current task's `frame` and returns the next task's.
    #[inline]
    pub fn switch(&mut self, frame: usize) -> usize {
        self.frame[self.current] = frame;
        self.advance()
    }

    /// Marks the current task blocked until `wake(event)`; the caller switches away.
    pub fn block(&mut self, event: Event) {
        self.slots.state[self.current] = State::Blocked(event);
    }

    /// True if a task was waiting for `event`.
    pub fn wake(&mut self, event: Event) -> bool {
        let mut woke = false;
        for state in &mut self.slots.state[..self.end] {
            if *state == State::Blocked(event) {
                *state = State::Ready;
                woke = true;
            }
        }
        woke
    }

    /// Ends the live thread in `slot` (never slot 0) with `code`, a zombie while a handle reaches it, and wakes its
    /// joiners; returns its kernel stack and the event it was blocked on. With its last thread its process ends alike,
    /// so its own handles must already be released (`take_handles`).
    pub fn end(&mut self, slot: usize, code: u64) -> (PhysAddr, Option<Event>) {
        assert!(slot != 0, "the boot context cannot exit");
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

    /// The current task's slot and generation.
    pub fn current(&self) -> (usize, u64) {
        (self.current, self.slots.generation[self.current])
    }

    /// The current task's process index (and ASID).
    pub fn process(&self) -> usize {
        self.current_process
    }

    /// The current task's process generation.
    pub fn generation(&self) -> u64 {
        self.processes.entries.generation[self.process()]
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

    /// A live thread of the process at `index`, if it has one.
    pub fn thread_of(&self, index: usize) -> Option<usize> {
        let threads = self.processes.threads[index];
        (threads != 0).then(|| threads.trailing_zeros() as usize)
    }

    /// The current task's own priority.
    pub fn priority(&self) -> u8 {
        self.priority[self.current]
    }

    /// The current task waits for a mutex `slot` owns: `slot` runs at least at the current task's priority.
    pub fn boost(&mut self, slot: usize) {
        self.effective[slot] = self.effective[slot].max(self.effective[self.current]);
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

    /// A ready task's effective priority beats the current task's.
    pub fn outranked(&self) -> bool {
        let current = self.effective[self.current];
        (0..self.end).any(|s| self.slots.state[s] == State::Ready && self.effective[s] > current)
    }

    /// The memory of the process at `index`.
    pub fn memory(&mut self, index: usize) -> &mut Memory {
        &mut self.processes.memory[index]
    }

    /// The current process's handles.
    pub fn handles(&mut self) -> &mut Handles<C> {
        &mut self.processes.handles[self.current_process]
    }

    /// Empties the handle table of the process at `index`; returns what it held.
    pub fn take_handles(&mut self, index: usize) -> Handles<C> {
        core::mem::take(&mut self.processes.handles[index])
    }

    /// Threads in the queue, the boot context included.
    pub fn count(&self) -> usize {
        let queued = |s: &&State| matches!(s, State::Ready | State::Blocked(_));
        self.slots.state[..self.end].iter().filter(queued).count()
    }

    /// Moves to the highest-priority ready task, the first after the current one (itself last) among equals; with none
    /// ready, to the boot context.
    fn advance(&mut self) -> usize {
        let (mut slot, mut next) = (self.current, None);
        for _ in 0..self.end {
            slot = if slot + 1 == self.end { 0 } else { slot + 1 };
            if matches!(self.slots.state[slot], State::Ready)
                && next.is_none_or(|n: usize| self.effective[slot] > self.effective[n])
            {
                next = Some(slot);
            }
        }
        self.current = next.unwrap_or(0);
        self.slots.state[self.current] = State::Ready;
        self.current_process = self.process[self.current];
        self.frame[self.current]
    }
}

impl<const N: usize, const P: usize, C: Clamp> Default for Scheduler<N, P, C> {
    fn default() -> Self {
        Self::new()
    }
}
