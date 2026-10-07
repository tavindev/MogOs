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

/// The process table: per index, an address space (its level-1 table; `PhysAddr(0)` is the boot table), handles,
/// memory and live thread count. Index 0 is the kernel: the boot context and kernel tasks, in the boot table.
pub struct Processes<const P: usize> {
    space: [PhysAddr; P],
    handles: [Handles; P],
    /// An ended process's budget stays here until `reap`.
    memory: [Memory; P],
    threads: [usize; P],
    /// Bumped by each `add_process`, so a process handle reaches one process, never a later one at the same index.
    generation: [u64; P],
    state: [State; P],
}

/// Run queue of up to `N` threads of up to `P` processes, each thread known by its saved trap frame address, process,
/// kernel stack and priority. The highest-priority ready thread runs, round robin within a level; blocked ones are
/// skipped; with none ready, the boot context (slot 0) runs.
pub struct Scheduler<const N: usize, const P: usize> {
    frame: [usize; N],
    process: [usize; N],
    /// First frame of the kernel stack, charged to the thread's process.
    stack: [PhysAddr; N],
    state: [State; N],
    /// Bumped by each `add`, so a thread handle reaches one thread, never a later one in the same slot.
    generation: [u64; N],
    /// Own priority per slot.
    priority: [u8; N],
    /// Own priority, raised while a higher-priority task waits for a mutex the slot owns.
    effective: [u8; N],
    /// One past the highest slot ever used.
    end: usize,
    current: usize,
    /// `process[current]`, cached: the syscall, switch and exit paths read it on every call.
    current_process: usize,
    processes: Processes<P>,
}

impl<const N: usize, const P: usize> Scheduler<N, P> {
    /// Slot 0 is the boot context, the kernel process's first thread; its frame is recorded on its first switch.
    pub const fn new() -> Self {
        let mut state = [State::Exited(0); N];
        state[0] = State::Ready;
        let mut process_state = [State::Exited(0); P];
        process_state[0] = State::Ready;
        let mut threads = [0; P];
        threads[0] = 1;
        Self {
            frame: [0; N],
            process: [0; N],
            stack: [PhysAddr(0); N],
            state,
            generation: [0; N],
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
                generation: [0; P],
                state: process_state,
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
        handles: Handles,
    ) {
        let p = &mut self.processes;
        p.space[index] = space;
        p.handles[index] = handles;
        p.memory[index] = memory;
        p.threads[index] = 0;
        p.generation[index] = generation;
        p.state[index] = State::Ready;
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
        self.state[slot] = State::Ready;
        self.generation[slot] = generation;
        self.priority[slot] = priority;
        self.effective[slot] = priority;
        self.end = self.end.max(slot + 1);
        self.processes.threads[process] += 1;
    }

    /// The slot the next `add` takes, if any is free, and the generation it gives the thread there.
    pub fn free_slot(&self) -> Option<(usize, u64)> {
        free(&self.state, &self.generation)
    }

    /// The index the next `add_process` takes, if any is free, and the generation it gives the process there.
    pub fn free_process(&self) -> Option<(usize, u64)> {
        free(&self.processes.state, &self.processes.generation)
    }

    /// Saves the current task's `frame` and returns the next task's.
    #[inline]
    pub fn switch(&mut self, frame: usize) -> usize {
        self.frame[self.current] = frame;
        self.advance()
    }

    /// Marks the current task blocked until `wake(event)`; the caller switches away.
    pub fn block(&mut self, event: Event) {
        self.state[self.current] = State::Blocked(event);
    }

    /// True if a task was waiting for `event`.
    pub fn wake(&mut self, event: Event) -> bool {
        let mut woke = false;
        for state in &mut self.state[..self.end] {
            if *state == State::Blocked(event) {
                *state = State::Ready;
                woke = true;
            }
        }
        woke
    }

    /// Ends the live thread in `slot` (never slot 0) with `code`, a zombie while a handle reaches it, and wakes its
    /// joiners; returns its kernel stack and the event it was blocked on. With its last thread its process ends alike,
    /// so its handles must already be taken (`take_handles`).
    pub fn end(&mut self, slot: usize, code: u64) -> (PhysAddr, Option<Event>) {
        assert!(slot != 0, "the boot context cannot exit");
        let blocked = match self.state[slot] {
            State::Blocked(event) => Some(event),
            _ => None,
        };
        let index = self.process[slot];
        let thread = Object::Thread {
            slot,
            generation: self.generation[slot],
        };
        let ended = |held| match held {
            true => State::Zombie(code),
            false => State::Exited(code),
        };
        self.state[slot] = ended(self.holds(|o| o == thread));
        self.wake(Event::Join(slot));
        self.processes.threads[index] -= 1;
        if self.processes.threads[index] == 0 {
            let process = Object::Process {
                index,
                generation: self.processes.generation[index],
            };
            self.processes.state[index] = ended(self.holds(|o| o == process));
            self.wake(Event::Exit(index));
        }
        (self.stack[slot], blocked)
    }

    /// Whether any process's handle table holds a handle to an object `f` matches. Only running processes but the
    /// kernel hold handles: an ended process's table was taken.
    pub fn holds(&self, f: impl Fn(Object) -> bool) -> bool {
        let p = &self.processes;
        (1..P).any(|i| p.state[i] == State::Ready && p.handles[i].objects().any(&f))
    }

    /// For the process at `index` with `generation`: `None` while it runs; once it ended, its code and its budget's
    /// limit, which only the first call gets (later ones get 0), and its index is freed; `EBADF` once a newer process
    /// took the index.
    pub fn reap(&mut self, index: usize, generation: u64) -> Result<Option<(u64, usize)>, i64> {
        let p = &mut self.processes;
        let Some(code) = reap(&mut p.state[index], p.generation[index], generation)? else {
            return Ok(None);
        };
        let budget = core::mem::replace(&mut p.memory[index].budget, Budget::new(0));
        Ok(Some((code, budget.limit())))
    }

    /// For the thread in `slot` with `generation`: `None` while it runs; once it ended, its code, and its slot is
    /// freed; `EBADF` once a newer thread took the slot.
    pub fn join(&mut self, slot: usize, generation: u64) -> Result<Option<u64>, i64> {
        reap(&mut self.state[slot], self.generation[slot], generation)
    }

    /// A handle to the process at `index` with `generation` was closed: if it ended, frees its index and returns its
    /// budget's limit, as `reap` would; otherwise 0.
    pub fn close(&mut self, index: usize, generation: u64) -> usize {
        if !matches!(self.processes.state[index], State::Zombie(_)) {
            return 0;
        }
        match self.reap(index, generation) {
            Ok(Some((_, limit))) => limit,
            _ => 0,
        }
    }

    /// A handle to the thread in `slot` with `generation` was closed: if it ended, frees its slot.
    pub fn close_thread(&mut self, slot: usize, generation: u64) {
        if matches!(self.state[slot], State::Zombie(_)) {
            let _ = self.join(slot, generation);
        }
    }

    /// Whether the process at `index` with `generation` still runs; `EBADF` once a newer process took the index.
    pub fn process_live(&self, index: usize, generation: u64) -> Result<bool, i64> {
        let p = &self.processes;
        live(p.state[index], p.generation[index], generation)
    }

    /// Whether the thread in `slot` with `generation` still runs; `EBADF` once a newer thread took the slot.
    pub fn thread_live(&self, slot: usize, generation: u64) -> Result<bool, i64> {
        live(self.state[slot], self.generation[slot], generation)
    }

    /// The budget of the process at `index` with `generation`, unless it ended.
    pub fn budget(&mut self, index: usize, generation: u64) -> Option<&mut Budget> {
        let live = self.process_live(index, generation) == Ok(true);
        live.then_some(&mut self.processes.memory[index].budget)
    }

    /// The current task's slot and generation.
    pub fn current(&self) -> (usize, u64) {
        (self.current, self.generation[self.current])
    }

    /// The current task's process index (and ASID).
    pub fn process(&self) -> usize {
        self.current_process
    }

    /// The current task's process generation.
    pub fn generation(&self) -> u64 {
        self.processes.generation[self.process()]
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
        self.processes.threads[index]
    }

    /// A live thread of the process at `index`, if it has one.
    pub fn thread_of(&self, index: usize) -> Option<usize> {
        let live = |s: usize| matches!(self.state[s], State::Ready | State::Blocked(_));
        (1..self.end).find(|&s| self.process[s] == index && live(s))
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
        for (state, &effective) in self.state[..self.end].iter().zip(&self.effective) {
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
        (0..self.end).any(|s| self.state[s] == State::Ready && self.effective[s] > current)
    }

    /// The memory of the process at `index`.
    pub fn memory(&mut self, index: usize) -> &mut Memory {
        &mut self.processes.memory[index]
    }

    /// The current process's handles.
    pub fn handles(&mut self) -> &mut Handles {
        &mut self.processes.handles[self.current_process]
    }

    /// Empties the handle table of the process at `index`; returns what it held.
    pub fn take_handles(&mut self, index: usize) -> Handles {
        core::mem::take(&mut self.processes.handles[index])
    }

    /// Threads in the queue, the boot context included.
    pub fn count(&self) -> usize {
        let queued = |s: &&State| matches!(s, State::Ready | State::Blocked(_));
        self.state[..self.end].iter().filter(queued).count()
    }

    /// Moves to the highest-priority ready task, the first after the current one (itself last) among equals; with none
    /// ready, to the boot context.
    fn advance(&mut self) -> usize {
        let (mut slot, mut next) = (self.current, None);
        for _ in 0..self.end {
            slot = if slot + 1 == self.end { 0 } else { slot + 1 };
            if matches!(self.state[slot], State::Ready)
                && next.is_none_or(|n: usize| self.effective[slot] > self.effective[n])
            {
                next = Some(slot);
            }
        }
        self.current = next.unwrap_or(0);
        self.state[self.current] = State::Ready;
        self.current_process = self.process[self.current];
        self.frame[self.current]
    }
}

impl<const N: usize, const P: usize> Default for Scheduler<N, P> {
    fn default() -> Self {
        Self::new()
    }
}

/// The first free entry after entry 0 and the generation its next occupant gets.
fn free(state: &[State], generation: &[u64]) -> Option<(usize, u64)> {
    let i = 1 + state[1..]
        .iter()
        .position(|s| matches!(s, State::Exited(_)))?;
    Some((i, generation[i] + 1))
}

/// For an entry at `current` generation reached with `generation`: whether it runs; `EBADF` if they differ.
fn live(state: State, current: u64, generation: u64) -> Result<bool, i64> {
    if current != generation {
        return Err(EBADF);
    }
    Ok(matches!(state, State::Ready | State::Blocked(_)))
}

/// As `live`, but once the entry ended, frees it and returns its code.
fn reap(state: &mut State, current: u64, generation: u64) -> Result<Option<u64>, i64> {
    if current != generation {
        return Err(EBADF);
    }
    let (State::Exited(code) | State::Zombie(code)) = *state else {
        return Ok(None);
    };
    *state = State::Exited(code);
    Ok(Some(code))
}
