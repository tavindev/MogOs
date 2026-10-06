use mm::PhysAddr;

use crate::handle::Handles;

/// No room for the task: the run queue is full or its memory could not be allocated.
#[derive(Debug)]
pub struct Full;

/// Round-robin run queue of up to `N` tasks, each known by its saved trap frame address and address space
/// (its level-1 table; `PhysAddr(0)` is the boot table), and its handle table.
pub struct Scheduler<const N: usize> {
    /// Frame address and address space per slot; frame 0 marks a free slot (never slot 0).
    tasks: [(usize, PhysAddr); N],
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
            handles: [Handles::new(); N],
            end: 1,
            current: 0,
        }
    }

    /// Queues a new task whose first frame is at `frame` in address space `space`, with `handles(slot)` as its
    /// handles.
    pub fn add(
        &mut self,
        frame: usize,
        space: PhysAddr,
        handles: impl FnOnce(usize) -> Handles,
    ) -> Result<(), Full> {
        let slot = 1 + self.tasks[1..].iter().position(|t| t.0 == 0).ok_or(Full)?;
        self.tasks[slot] = (frame, space);
        self.handles[slot] = handles(slot);
        self.end = self.end.max(slot + 1);
        Ok(())
    }

    /// Saves the current task's `frame` and returns the next task's.
    pub fn switch(&mut self, frame: usize) -> usize {
        self.tasks[self.current].0 = frame;
        self.advance()
    }

    /// Drops the current task (never slot 0) and its handles, and returns the next task's frame.
    pub fn exit(&mut self) -> usize {
        assert!(self.current != 0, "the boot context cannot exit");
        self.tasks[self.current] = (0, PhysAddr(0));
        self.handles[self.current] = Handles::new();
        self.advance()
    }

    /// The current task's slot and address space.
    pub fn current(&self) -> (usize, PhysAddr) {
        (self.current, self.tasks[self.current].1)
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
