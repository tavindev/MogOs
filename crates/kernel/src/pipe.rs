//! Pipes: a fixed table of one-page ring buffers. A handle reaches an end by table index and generation; each entry
//! counts the handles to its ends and is freed once none is left.

use mm::PhysAddr;

use crate::syscall::EPIPE;

/// Buffer bytes: one page.
pub const SIZE: usize = 4096;

/// One end of the pipe at `index` with `generation`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct End {
    /// `u32`, not `usize`: it keeps copying an `Object` plain moves, not a `memcpy` on the syscall path.
    pub index: u32,
    pub generation: u64,
    /// The write end; otherwise the read end.
    pub write: bool,
}

pub struct Pipe {
    /// The buffer's frame; `PhysAddr(0)` while the entry is free.
    pub page: PhysAddr,
    /// The process (slot, generation) whose budget pays for the page.
    pub creator: (usize, u64),
    generation: u64,
    head: usize,
    len: usize,
    readers: usize,
    writers: usize,
}

const FREE: Pipe = Pipe {
    page: PhysAddr(0),
    creator: (0, 0),
    generation: 0,
    head: 0,
    len: 0,
    readers: 0,
    writers: 0,
};

pub struct Pipes<const N: usize>([Pipe; N]);

impl<const N: usize> Pipes<N> {
    pub const fn new() -> Self {
        Self([FREE; N])
    }

    /// The read end of the pipe the next `create` makes, if an entry is free.
    pub fn free(&self) -> Option<End> {
        let index = self.0.iter().position(|p| p.page.0 == 0)?;
        let generation = self.0[index].generation + 1;
        Some(End {
            index: index as u32,
            generation,
            write: false,
        })
    }

    /// Makes the pipe whose read end is `read` (from `free`), with one handle to each end and its buffer at `page`,
    /// charged to `creator`.
    pub fn create(&mut self, read: End, page: PhysAddr, creator: (usize, u64)) {
        self.0[read.index as usize] = Pipe {
            page,
            creator,
            generation: read.generation,
            readers: 1,
            writers: 1,
            ..FREE
        };
    }

    /// The open pipe `end` reaches.
    pub fn get(&mut self, end: End) -> Option<&mut Pipe> {
        let pipe = self.0.get_mut(end.index as usize)?;
        (pipe.page.0 != 0 && pipe.generation == end.generation).then_some(pipe)
    }

    /// Counts one more handle to `end`.
    pub fn open(&mut self, end: End) {
        if let Some(pipe) = self.get(end) {
            *pipe.count(end) += 1;
        }
    }

    /// Drops a handle to `end`; once no handle reaches the pipe, frees its entry and returns its page and creator.
    pub fn close(&mut self, end: End) -> Option<(PhysAddr, (usize, u64))> {
        let pipe = self.get(end)?;
        *pipe.count(end) -= 1;
        if pipe.readers + pipe.writers > 0 {
            return None;
        }
        let page = core::mem::replace(&mut pipe.page, PhysAddr(0));
        Some((page, pipe.creator))
    }

    /// Open pipes whose page is charged to `creator`.
    pub fn charged_to(&self, creator: (usize, u64)) -> usize {
        let charged = |p: &&Pipe| p.page.0 != 0 && p.creator == creator;
        self.0.iter().filter(charged).count()
    }
}

impl<const N: usize> Default for Pipes<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl Pipe {
    /// Moves up to `out.len()` bytes from the buffer in `page` to `out`; returns the count, 0 at end of file (empty,
    /// no write end left), or `None` while it is empty and a write end is open.
    pub fn read(&mut self, page: &[u8; SIZE], out: &mut [u8]) -> Option<i64> {
        if self.len == 0 && self.writers > 0 && !out.is_empty() {
            return None;
        }
        let n = out.len().min(self.len);
        let first = n.min(SIZE - self.head);
        out[..first].copy_from_slice(&page[self.head..self.head + first]);
        out[first..n].copy_from_slice(&page[..n - first]);
        self.head = (self.head + n) % SIZE;
        self.len -= n;
        Some(n as i64)
    }

    /// Moves all of `data` (at most `SIZE` bytes) into the buffer in `page`, never part of it; returns the count,
    /// `EPIPE` once no read end is left, or `None` until it fits. Empty `data` returns 0, as on Linux.
    pub fn write(&mut self, page: &mut [u8; SIZE], data: &[u8]) -> Option<i64> {
        if data.is_empty() {
            return Some(0);
        }
        if self.readers == 0 {
            return Some(EPIPE);
        }
        if SIZE - self.len < data.len() {
            return None;
        }
        let n = data.len();
        let tail = (self.head + self.len) % SIZE;
        let first = n.min(SIZE - tail);
        page[tail..tail + first].copy_from_slice(&data[..first]);
        page[..n - first].copy_from_slice(&data[first..n]);
        self.len += n;
        Some(n as i64)
    }

    fn count(&mut self, end: End) -> &mut usize {
        match end.write {
            true => &mut self.writers,
            false => &mut self.readers,
        }
    }
}
