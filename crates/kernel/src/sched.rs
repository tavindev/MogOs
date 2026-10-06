/// The run queue is full.
#[derive(Debug)]
pub struct Full;

/// Round-robin run queue of up to `N` tasks, each known by its saved trap frame address.
pub struct Scheduler<const N: usize> {
    frames: [usize; N],
    len: usize,
    current: usize,
}

impl<const N: usize> Scheduler<N> {
    /// Slot 0 is the boot context; its frame is recorded on its first switch.
    pub const fn new() -> Self {
        Self {
            frames: [0; N],
            len: 1,
            current: 0,
        }
    }

    /// Queues a new task whose first frame is at `frame`.
    pub fn add(&mut self, frame: usize) -> Result<(), Full> {
        let slot = self.frames.get_mut(self.len).ok_or(Full)?;
        *slot = frame;
        self.len += 1;
        Ok(())
    }

    /// Saves the current task's `frame` and returns the next task's.
    pub fn switch(&mut self, frame: usize) -> usize {
        self.frames[self.current] = frame;
        self.current = (self.current + 1) % self.len;
        self.frames[self.current]
    }
}

impl<const N: usize> Default for Scheduler<N> {
    fn default() -> Self {
        Self::new()
    }
}
