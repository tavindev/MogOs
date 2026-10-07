use mm::{Budget, PhysAddr};

use crate::handle::{Handles, Object};
use crate::syscall::{EBADF, KILLED};

/// Priority levels: 0 (lowest; the boot context and kernel tasks) to `PRIORITIES - 1`.
pub const PRIORITIES: u8 = 4;

/// No room for the task: the run queue is full or its memory could not be allocated.
#[derive(Debug)]
pub struct Full;

/// A task's kernel stack and, for a process, its budget and where its next `map` goes. Handle tables are fixed arrays
/// in the scheduler, so they are not charged.
pub struct Memory {
    /// First frame of the kernel stack.
    pub stack: PhysAddr,
    pub budget: Budget,
    pub next: u64,
}

const NO_MEMORY: Memory = Memory {
    stack: PhysAddr(0),
    budget: Budget::new(0),
    next: 0,
};

/// What a blocked task waits for; `wake` makes every task waiting for it ready.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Event {
    /// Data, room or a closed end in the pipe at this table index.
    Pipe(usize),
    /// The process in this slot exiting.
    Exit(usize),
    /// The mutex at this table index being unlocked.
    Lock(usize),
    /// A console line being entered.
    Console,
    /// Nothing: the boot context, which runs only once no other task is ready.
    Idle,
}

/// What `kill` hands back: handles, address space, kernel stack, and the event the task was blocked on.
pub type Killed = (Handles, PhysAddr, PhysAddr, Option<Event>);

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ready,
    Blocked(Event),
    /// A free slot; the task that last ran in it exited with this code.
    Exited(u64),
    /// Exited with this code, its slot kept until its parent `reap`s it or closes its process handle.
    Zombie(u64),
}

/// Run queue of up to `N` tasks, each known by its saved trap frame address and address space (its level-1 table;
/// `PhysAddr(0)` is the boot table), its memory, its handle table and its priority. The highest-priority ready task
/// runs, round robin within a level; blocked tasks are skipped; with none ready, the boot context (slot 0) runs.
pub struct Scheduler<const N: usize> {
    /// Frame address and address space per slot.
    tasks: [(usize, PhysAddr); N],
    state: [State; N],
    /// Bumped by each `add`, so a process handle reaches one task, never a later one in the same slot.
    generation: [u64; N],
    /// An exited task's budget stays here until `reap`.
    memory: [Memory; N],
    handles: [Handles; N],
    /// Own priority per slot.
    priority: [u8; N],
    /// Own priority, raised while a higher-priority task waits for a mutex the slot owns.
    effective: [u8; N],
    /// One past the highest slot ever used.
    end: usize,
    current: usize,
}

impl<const N: usize> Scheduler<N> {
    /// Slot 0 is the boot context in the boot table; its frame is recorded on its first switch.
    pub const fn new() -> Self {
        let mut state = [State::Exited(0); N];
        state[0] = State::Ready;
        Self {
            tasks: [(0, PhysAddr(0)); N],
            state,
            generation: [0; N],
            memory: [NO_MEMORY; N],
            handles: [Handles::new(); N],
            priority: [0; N],
            effective: [0; N],
            end: 1,
            current: 0,
        }
    }

    /// Queues a new task in `slot` with `generation` (from `free_slot`), its first frame at `frame` in address space
    /// `space`, with `memory`, `handles` and `priority` (below `PRIORITIES`).
    pub fn add(
        &mut self,
        (slot, generation): (usize, u64),
        frame: usize,
        space: PhysAddr,
        memory: Memory,
        handles: Handles,
        priority: u8,
    ) {
        self.tasks[slot] = (frame, space);
        self.state[slot] = State::Ready;
        self.generation[slot] = generation;
        self.memory[slot] = memory;
        self.handles[slot] = handles;
        self.priority[slot] = priority;
        self.effective[slot] = priority;
        self.end = self.end.max(slot + 1);
    }

    /// The slot the next `add` takes, if any is free, and the generation it gives the task there.
    pub fn free_slot(&self) -> Option<(usize, u64)> {
        let slot = 1 + self.state[1..]
            .iter()
            .position(|s| matches!(s, State::Exited(_)))?;
        Some((slot, self.generation[slot] + 1))
    }

    /// Saves the current task's `frame` and returns the next task's.
    pub fn switch(&mut self, frame: usize) -> usize {
        self.tasks[self.current].0 = frame;
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

    /// Ends the current task (never slot 0) with `code` and wakes its waiters; returns the next task's frame and the
    /// ended task's kernel stack, which the caller frees with its other frames. The caller first takes and releases
    /// its handles; its budget stays for `reap`, and while another task holds a handle to it, so does its slot.
    pub fn exit(&mut self, code: u64) -> (usize, PhysAddr) {
        assert!(self.current != 0, "the boot context cannot exit");
        let stack = self.memory[self.current].stack;
        self.end(self.current, code);
        (self.advance(), stack)
    }

    /// Ends the process in `slot` with `generation`, not the current task, as a fault would (`KILLED`) and wakes its
    /// waiters; returns its handles (for the caller to release), address space, kernel stack (to free) and the event it
    /// was blocked on, or `None` if it already exited; `EBADF` once a newer task took the slot.
    pub fn kill(&mut self, slot: usize, generation: u64) -> Result<Option<Killed>, i64> {
        if self.generation[slot] != generation {
            return Err(EBADF);
        }
        let blocked = match self.state[slot] {
            State::Ready => None,
            State::Blocked(event) => Some(event),
            _ => return Ok(None),
        };
        let handles = core::mem::take(&mut self.handles[slot]);
        self.end(slot, KILLED);
        Ok(Some((
            handles,
            self.tasks[slot].1,
            self.memory[slot].stack,
            blocked,
        )))
    }

    /// Marks the task in `slot` exited with `code`, kept as a zombie while another task holds a handle to it, and wakes
    /// its waiters.
    fn end(&mut self, slot: usize, code: u64) {
        let process = Object::Process {
            slot,
            generation: self.generation[slot],
        };
        let held = self.handles[..self.end]
            .iter()
            .any(|h| h.objects().any(|o| o == process));
        self.state[slot] = match held {
            true => State::Zombie(code),
            false => State::Exited(code),
        };
        self.wake(Event::Exit(slot));
    }

    /// For the process in `slot` with `generation`: `None` while it runs; once it exited, its code and its budget's
    /// limit, which only the first call gets (later ones get 0), and its slot is freed; `EBADF` once a newer task took
    /// the slot.
    pub fn reap(&mut self, slot: usize, generation: u64) -> Result<Option<(u64, usize)>, i64> {
        if self.generation[slot] != generation {
            return Err(EBADF);
        }
        let (State::Exited(code) | State::Zombie(code)) = self.state[slot] else {
            return Ok(None);
        };
        self.state[slot] = State::Exited(code);
        let budget = core::mem::replace(&mut self.memory[slot].budget, Budget::new(0));
        Ok(Some((code, budget.limit())))
    }

    /// A handle to the process in `slot` with `generation` was closed: if it exited, frees its slot and returns its
    /// budget's limit, as `reap` would; otherwise 0.
    pub fn close(&mut self, slot: usize, generation: u64) -> usize {
        if !matches!(self.state[slot], State::Zombie(_)) {
            return 0;
        }
        match self.reap(slot, generation) {
            Ok(Some((_, limit))) => limit,
            _ => 0,
        }
    }

    /// The budget of the process in `slot` with `generation`, unless it exited.
    pub fn budget(&mut self, slot: usize, generation: u64) -> Option<&mut Budget> {
        let live = self.generation[slot] == generation
            && matches!(self.state[slot], State::Ready | State::Blocked(_));
        live.then_some(&mut self.memory[slot].budget)
    }

    /// The current task's slot and address space.
    pub fn current(&self) -> (usize, PhysAddr) {
        (self.current, self.tasks[self.current].1)
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

    /// The current task's generation.
    pub fn generation(&self) -> u64 {
        self.generation[self.current]
    }

    /// The current task's memory.
    pub fn memory(&mut self) -> &mut Memory {
        &mut self.memory[self.current]
    }

    /// The current task's handles.
    pub fn handles(&mut self) -> &mut Handles {
        &mut self.handles[self.current]
    }

    /// Tasks in the queue, the boot context included.
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
        self.tasks[self.current].0
    }
}

impl<const N: usize> Default for Scheduler<N> {
    fn default() -> Self {
        Self::new()
    }
}
