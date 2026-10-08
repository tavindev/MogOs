#![cfg_attr(not(test), no_std)]

use core::fmt;
use core::ops::Range;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering::Relaxed;

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
    /// Every word below it is full, so first fit starts its scan here.
    hint: usize,
}

impl<const WORDS: usize> FrameAllocator<WORDS> {
    /// Manages no frames; a placeholder until one built by `new` replaces it.
    pub const fn empty() -> Self {
        Self {
            base: 0,
            frames: 0,
            used: [u64::MAX; WORDS],
            hint: 0,
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
        Self {
            base,
            frames,
            used,
            hint: 0,
        }
    }

    /// Marks every frame overlapping `range` as in use.
    pub fn reserve(&mut self, range: Range<PhysAddr>) {
        let first = range.start.0.saturating_sub(self.base) / FRAME_SIZE;
        let end = range.end.0.saturating_sub(self.base).div_ceil(FRAME_SIZE);
        let (mut i, end) = (first as usize, (end as usize).min(self.frames));
        // A word at a time where the range covers one, so boot pays per 64 frames of the image.
        while i < end {
            match i % 64 {
                0 if end - i >= 64 => (self.used[i / 64], i) = (u64::MAX, i + 64),
                _ => (self.used[i / 64], i) = (self.used[i / 64] | 1 << (i % 64), i + 1),
            }
        }
    }

    /// Allocates the first run of `count` free frames.
    pub fn alloc_contiguous(&mut self, count: usize) -> Option<Range<PhysAddr>> {
        let mut from = self.hint;
        while self.used.get(from) == Some(&u64::MAX) {
            from += 1;
        }
        self.hint = from;
        // Free frames ending at the previous word's top: none at the hint, as every word below it is full.
        let mut run = 0;
        for (i, &word) in self.used[from..].iter().enumerate() {
            let w = from + i;
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
        let found = self.used[self.hint..].iter().position(|&w| w != u64::MAX);
        let Some(word) = found.map(|w| self.hint + w) else {
            self.hint = WORDS;
            return None;
        };
        self.hint = word;
        let bit = self.used[word].trailing_ones() as usize;
        self.used[word] |= 1 << bit;
        Some(PhysAddr(self.base + (word * 64 + bit) as u64 * FRAME_SIZE))
    }

    /// Takes `count` free frames in one pass over the bitmap, handing each to `put` with its position, after a count of
    /// the free bits from the hint on; false, taking none, if too few are free.
    pub fn alloc_many(&mut self, count: usize, mut put: impl FnMut(usize, PhysAddr)) -> bool {
        let mut free = 0;
        let enough = (self.used[self.hint..].iter()).any(|w| {
            free += w.count_zeros() as usize;
            free >= count
        });
        if count == 0 || !enough {
            return count == 0;
        }
        let mut taken = 0;
        for w in self.hint..WORDS {
            let word = &mut self.used[w];
            while *word != u64::MAX {
                let bit = word.trailing_ones() as usize;
                *word |= 1 << bit;
                put(
                    taken,
                    PhysAddr(self.base + (w * 64 + bit) as u64 * FRAME_SIZE),
                );
                taken += 1;
                if taken == count {
                    self.hint = w;
                    return true;
                }
            }
        }
        unreachable!("counted")
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
        if i / 64 < self.hint {
            self.hint = i / 64;
        }
    }

    pub fn free_count(&self) -> usize {
        self.used.iter().map(|w| w.count_zeros() as usize).sum()
    }
}

/// Most frames a `Budget` counts: its limit and use are each a `u32` (16 TiB of 4 KiB frames).
pub const MAX_FRAMES: usize = u32::MAX as usize;

/// Frames a process may hold; every frame taken through it is charged at allocation time. The limit (high half) and
/// the frames used (low half) share one word and each change is one CAS on both, so a check and its update never see
/// two different limits, and any core may charge or refund it without a lock.
pub struct Budget(AtomicU64);

impl Budget {
    /// Panics above `MAX_FRAMES`.
    pub const fn new(limit: usize) -> Self {
        assert!(limit <= MAX_FRAMES, "budget over MAX_FRAMES");
        Self(AtomicU64::new((limit as u64) << 32))
    }

    /// Applies `f` to (limit, used) in one CAS; false, changing nothing, if `f` refuses.
    fn update(&self, f: impl Fn(u64, u64) -> Option<(u64, u64)>) -> bool {
        (self.0)
            .try_update(Relaxed, Relaxed, |w| {
                f(w >> 32, w & u64::from(u32::MAX)).map(|(limit, used)| limit << 32 | used)
            })
            .is_ok()
    }

    /// Takes a frame from `frames` and charges it; `None` (nothing charged) over budget or out of frames.
    pub fn alloc<const W: usize>(&self, frames: &mut FrameAllocator<W>) -> Option<PhysAddr> {
        self.charge(1).then(|| frames.alloc())?.or_else(|| {
            self.refund(1);
            None
        })
    }

    /// Takes `count` contiguous frames and charges them; `None` (nothing charged) over budget or out of frames.
    pub fn alloc_contiguous<const W: usize>(
        &self,
        frames: &mut FrameAllocator<W>,
        count: usize,
    ) -> Option<Range<PhysAddr>> {
        self.charge(count)
            .then(|| frames.alloc_contiguous(count))?
            .or_else(|| {
                self.refund(count);
                None
            })
    }

    /// Returns `frame` to `frames` and refunds it.
    pub fn free<const W: usize>(&self, frames: &mut FrameAllocator<W>, frame: PhysAddr) {
        frames.free(frame);
        self.refund(1);
    }

    /// Charges `count` frames; false, charging nothing, over budget. A charge the allocator then cannot fill is
    /// refunded, so meanwhile another charge may fail that would fit once it is (only with the frames run out).
    pub fn charge(&self, count: usize) -> bool {
        self.update(|limit, used| {
            (count as u64 <= limit - used).then(|| (limit, used + count as u64))
        })
    }

    /// Refunds `count` frames `charge` took.
    pub fn refund(&self, count: usize) {
        self.update(|limit, used| Some((limit, used - count as u64)));
    }

    /// Lowers the limit by `frames`, which move to a child's budget; false, changing nothing, if fewer remain.
    pub fn shrink(&self, frames: usize) -> bool {
        self.update(|limit, used| {
            (frames as u64 <= limit - used).then(|| (limit - frames as u64, used))
        })
    }

    /// Raises the limit by `frames`, which came back from an exited child's budget.
    pub fn grow(&self, frames: usize) {
        self.update(|limit, used| Some((limit + frames as u64, used)));
    }

    /// Starts the budget over at `limit` with nothing charged, for the next process at an index.
    pub fn reset(&self, limit: usize) {
        assert!(limit <= MAX_FRAMES, "budget over MAX_FRAMES");
        self.0.store((limit as u64) << 32, Relaxed);
    }

    /// Empties the budget: returns its limit and leaves 0 (frames still charged stay with their holders).
    pub fn take(&self) -> usize {
        (self.0.swap(0, Relaxed) >> 32) as usize
    }

    pub fn limit(&self) -> usize {
        (self.0.load(Relaxed) >> 32) as usize
    }

    pub fn remaining(&self) -> usize {
        let w = self.0.load(Relaxed);
        ((w >> 32) - (w & u64::from(u32::MAX))) as usize
    }
}
