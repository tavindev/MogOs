use mm::{Budget, PhysAddr};

use crate::handle::Handles;

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

/// Round-robin run queue of up to `N` tasks, each known by its saved trap frame address and address space
/// (its level-1 table; `PhysAddr(0)` is the boot table), its memory and its handle table.
pub struct Scheduler<const N: usize> {
    /// Frame address and address space per slot; frame 0 marks a free slot (never slot 0).
    tasks: [(usize, PhysAddr); N],
    memory: [Memory; N],
    handles: [Handles; N],
    /// One past the highest slot ever used.
    end: usize,
    current: usize,
}

impl<const N: usize> Scheduler<N> {
    /// Slot 0 is the boot context in the boot table; its frame is recorded on its first switch.
    pub const fn new() -> Self {
        Self {
            tasks: [(0, PhysAddr(0)); N],
            memory: [NO_MEMORY; N],
            handles: [Handles::new(); N],
            end: 1,
            current: 0,
        }
    }

    /// Queues a new task whose first frame is at `frame` in address space `space`, with `memory`, and `handles(slot)`
    /// as its handles.
    pub fn add(
        &mut self,
        frame: usize,
        space: PhysAddr,
        memory: Memory,
        handles: impl FnOnce(usize) -> Handles,
    ) -> Result<(), Full> {
        let slot = 1 + self.tasks[1..].iter().position(|t| t.0 == 0).ok_or(Full)?;
        self.tasks[slot] = (frame, space);
        self.memory[slot] = memory;
        self.handles[slot] = handles(slot);
        self.end = self.end.max(slot + 1);
        Ok(())
    }

    /// Saves the current task's `frame` and returns the next task's.
    pub fn switch(&mut self, frame: usize) -> usize {
        self.tasks[self.current].0 = frame;
        self.advance()
    }

    /// Drops the current task (never slot 0) and its handles; returns the next task's frame and the dropped task's
    /// memory, whose frames the caller frees.
    pub fn exit(&mut self) -> (usize, Memory) {
        assert!(self.current != 0, "the boot context cannot exit");
        let memory = core::mem::replace(&mut self.memory[self.current], NO_MEMORY);
        self.tasks[self.current] = (0, PhysAddr(0));
        self.handles[self.current] = Handles::new();
        (self.advance(), memory)
    }

    /// The current task's slot and address space.
    pub fn current(&self) -> (usize, PhysAddr) {
        (self.current, self.tasks[self.current].1)
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
        1 + self.tasks[1..self.end].iter().filter(|t| t.0 != 0).count()
    }

    fn advance(&mut self) -> usize {
        loop {
            self.current = (self.current + 1) % self.end;
            if self.current == 0 || self.tasks[self.current].0 != 0 {
                return self.tasks[self.current].0;
            }
        }
    }
}

impl<const N: usize> Default for Scheduler<N> {
    fn default() -> Self {
        Self::new()
    }
}
