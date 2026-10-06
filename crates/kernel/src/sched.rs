/// No room for the task: the run queue is full or its memory could not be allocated.
#[derive(Debug)]
pub struct Full;

/// Round-robin run queue of up to `N` tasks, each known by its saved trap frame address and address space.
pub struct Scheduler<const N: usize> {
    /// Frame address and address space per slot; frame 0 marks a free slot (never slot 0).
    tasks: [(usize, usize); N],
    /// One past the highest slot ever used.
    end: usize,
    current: usize,
}

impl<const N: usize> Scheduler<N> {
    /// Slot 0 is the boot context in space 0; its frame is recorded on its first switch.
    pub const fn new() -> Self {
        Self {
            tasks: [(0, 0); N],
            end: 1,
            current: 0,
        }
    }

    /// Queues a new task whose first frame is at `frame` in address space `space`.
    pub fn add(&mut self, frame: usize, space: usize) -> Result<(), Full> {
        let slot = 1 + self.tasks[1..].iter().position(|t| t.0 == 0).ok_or(Full)?;
        self.tasks[slot] = (frame, space);
        self.end = self.end.max(slot + 1);
        Ok(())
    }

    /// Saves the current task's `frame` and returns the next task's.
    pub fn switch(&mut self, frame: usize) -> usize {
        self.tasks[self.current].0 = frame;
        self.advance()
    }

    /// Drops the current task (never slot 0) and returns the next task's frame.
    pub fn exit(&mut self) -> usize {
        assert!(self.current != 0, "the boot context cannot exit");
        self.tasks[self.current] = (0, 0);
        self.advance()
    }

    /// The current task's slot and address space.
    pub fn current(&self) -> (usize, usize) {
        (self.current, self.tasks[self.current].1)
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
