#![cfg_attr(not(test), no_std)]

use core::fmt;
use core::ops::Range;

pub const FRAME_SIZE: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
    /// All whole frames in `ram` start free; frames past the capacity are ignored.
    pub fn new(ram: Range<PhysAddr>) -> Self {
        let base = ram.start.0.next_multiple_of(FRAME_SIZE);
        let frames = (ram.end.0.saturating_sub(base) / FRAME_SIZE) as usize;
        let mut allocator = Self {
            base,
            frames: frames.min(WORDS * 64),
            used: [u64::MAX; WORDS],
        };
        for i in 0..allocator.frames {
            allocator.used[i / 64] &= !(1 << (i % 64));
        }
        allocator
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
        let mut run = 0;
        for i in 0..self.frames {
            run = if self.used[i / 64] & (1 << (i % 64)) == 0 {
                run + 1
            } else {
                0
            };
            if run == count {
                let first = self.base + (i + 1 - count) as u64 * FRAME_SIZE;
                let range = PhysAddr(first)..PhysAddr(first + count as u64 * FRAME_SIZE);
                self.reserve(range.clone());
                return Some(range);
            }
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

/// Memory type of a mapping; the value is its `MAIR_EL1` attribute index.
#[derive(Clone, Copy)]
pub enum MemoryType {
    Device = 0,
    Normal = 1,
}

/// `MAIR_EL1` matching `MemoryType`: Device-nGnRE, Normal write-back cacheable.
pub const MAIR: u64 = 0x04 | 0xff << 8;

/// Level-1 block descriptor (4 KiB granule) mapping the 1 GiB at `addr` for EL1 read/write.
pub const fn l1_block(addr: PhysAddr, ty: MemoryType) -> u64 {
    const VALID_BLOCK: u64 = 0b01;
    const INNER_SHAREABLE: u64 = 0b11 << 8;
    const ACCESS_FLAG: u64 = 1 << 10;
    const PXN_UXN: u64 = 0b11 << 53;
    let attrs = match ty {
        MemoryType::Device => PXN_UXN,
        MemoryType::Normal => INNER_SHAREABLE,
    };
    addr.0 & 0x0000_ffff_c000_0000 | attrs | ACCESS_FLAG | (ty as u64) << 2 | VALID_BLOCK
}
