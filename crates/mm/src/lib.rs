#![cfg_attr(not(test), no_std)]

use core::fmt;
use core::ops::Range;

const FRAME_SIZE: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysAddr(pub u64);

impl fmt::LowerHex for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

/// Bitmap allocator for 4 KiB frames; manages at most `WORDS * 64` frames, a set bit is in use.
pub struct FrameAllocator<const WORDS: usize> {
    base: u64,
    frames: usize,
    used: [u64; WORDS],
}

impl<const WORDS: usize> FrameAllocator<WORDS> {
    /// Manages no frames; a placeholder until one built by `new` replaces it.
    pub const fn empty() -> Self {
        Self {
            base: 0,
            frames: 0,
            used: [u64::MAX; WORDS],
        }
    }

    /// All whole frames in `ram` start free; frames past the capacity are ignored.
    pub fn new(ram: Range<PhysAddr>) -> Self {
        let base = ram.start.0.next_multiple_of(FRAME_SIZE);
        let frames = ((ram.end.0.saturating_sub(base) / FRAME_SIZE) as usize).min(WORDS * 64);
        let mut used = [u64::MAX; WORDS];
        used[..frames / 64].fill(0);
        if !frames.is_multiple_of(64) {
            used[frames / 64] = u64::MAX << (frames % 64);
        }
        Self { base, frames, used }
    }

    /// Marks every frame overlapping `range` as in use.
    pub fn reserve(&mut self, range: Range<PhysAddr>) {
        let first = range.start.0.saturating_sub(self.base) / FRAME_SIZE;
        let end = range.end.0.saturating_sub(self.base).div_ceil(FRAME_SIZE);
        for i in first as usize..(end as usize).min(self.frames) {
            self.used[i / 64] |= 1 << (i % 64);
        }
    }

    /// Allocates the first run of `count` free frames.
    pub fn alloc_contiguous(&mut self, count: usize) -> Option<Range<PhysAddr>> {
        let mut run = 0; // free frames ending at the previous word's top
        for (w, &word) in self.used.iter().enumerate() {
            let free = !word;
            let start = if run + free.trailing_ones() as usize >= count {
                Some(w * 64 - run)
            } else if count < 64 {
                // Bit i of `fit` stays set while frames i..i + k are all free.
                let (mut fit, mut k) = (free, 1);
                while k < count {
                    let shift = k.min(count - k);
                    fit &= fit >> shift;
                    k += shift;
                }
                (fit != 0).then(|| w * 64 + fit.trailing_zeros() as usize)
            } else {
                None
            };
            if let Some(start) = start {
                let first = self.base + start as u64 * FRAME_SIZE;
                let range = PhysAddr(first)..PhysAddr(first + count as u64 * FRAME_SIZE);
                self.reserve(range.clone());
                return Some(range);
            }
            run = if word == 0 {
                run + 64
            } else {
                free.leading_ones() as usize
            };
        }
        None
    }

    pub fn alloc(&mut self) -> Option<PhysAddr> {
        let word = self.used.iter().position(|&w| w != u64::MAX)?;
        let bit = self.used[word].trailing_ones() as usize;
        self.used[word] |= 1 << bit;
        Some(PhysAddr(self.base + (word * 64 + bit) as u64 * FRAME_SIZE))
    }

    /// Panics if `frame` was not allocated from this allocator.
    pub fn free(&mut self, frame: PhysAddr) {
        let offset = frame.0 - self.base;
        let i = (offset / FRAME_SIZE) as usize;
        assert!(
            offset.is_multiple_of(FRAME_SIZE) && i < self.frames,
            "bad frame {frame:#x}"
        );
        assert!(
            self.used[i / 64] & (1 << (i % 64)) != 0,
            "double free {frame:#x}"
        );
        self.used[i / 64] &= !(1 << (i % 64));
    }

    pub fn free_count(&self) -> usize {
        self.used.iter().map(|w| w.count_zeros() as usize).sum()
    }
}

/// Frames a process may hold; every frame taken through it is charged at allocation time.
pub struct Budget {
    limit: usize,
    used: usize,
}

impl Budget {
    pub const fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    /// Takes a frame from `frames` and charges it; `None` (nothing charged) over budget or out of frames.
    pub fn alloc<const W: usize>(&mut self, frames: &mut FrameAllocator<W>) -> Option<PhysAddr> {
        if self.used == self.limit {
            return None;
        }
        let frame = frames.alloc()?;
        self.used += 1;
        Some(frame)
    }

    /// Takes `count` contiguous frames and charges them; `None` (nothing charged) over budget or out of frames.
    pub fn alloc_contiguous<const W: usize>(
        &mut self,
        frames: &mut FrameAllocator<W>,
        count: usize,
    ) -> Option<Range<PhysAddr>> {
        if self.remaining() < count {
            return None;
        }
        let range = frames.alloc_contiguous(count)?;
        self.used += count;
        Some(range)
    }

    /// Returns `frame` to `frames` and refunds it.
    pub fn free<const W: usize>(&mut self, frames: &mut FrameAllocator<W>, frame: PhysAddr) {
        frames.free(frame);
        self.used -= 1;
    }

    /// Lowers the limit by `frames`, which moved to a child's budget; panics if fewer remain.
    pub fn shrink(&mut self, frames: usize) {
        assert!(frames <= self.remaining(), "budget overdrawn");
        self.limit -= frames;
    }

    /// Raises the limit by `frames`, which came back from an exited child's budget.
    pub fn grow(&mut self, frames: usize) {
        self.limit += frames;
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn remaining(&self) -> usize {
        self.limit - self.used
    }
}
