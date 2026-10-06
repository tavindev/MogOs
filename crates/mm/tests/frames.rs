use mm::{FrameAllocator, PhysAddr};

const BASE: u64 = 0x4000_0000;

fn frame(i: u64) -> PhysAddr {
    PhysAddr(BASE + i * 4096)
}

#[test]
fn allocates_every_frame_then_exhausts() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(3));
    assert_eq!(frames.free_count(), 3);
    assert_eq!(frames.alloc(), Some(frame(0)));
    assert_eq!(frames.alloc(), Some(frame(1)));
    assert_eq!(frames.alloc(), Some(frame(2)));
    assert_eq!(frames.alloc(), None);
    assert_eq!(frames.free_count(), 0);
}

#[test]
fn freed_frame_is_reused() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(2));
    let a = frames.alloc().unwrap();
    frames.alloc().unwrap();
    frames.free(a);
    assert_eq!(frames.alloc(), Some(a));
}

#[test]
#[should_panic]
fn double_free_panics() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(2));
    let a = frames.alloc().unwrap();
    frames.free(a);
    frames.free(a);
}

#[test]
fn reserve_covers_partial_frames_and_clips() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(8));
    frames.reserve(PhysAddr(BASE + 4095)..PhysAddr(BASE + 4097));
    frames.reserve(PhysAddr(BASE - 0x10000)..frame(1));
    frames.reserve(frame(7)..frame(100));
    assert_eq!(frames.free_count(), 5);
    assert_eq!(frames.alloc(), Some(frame(2)));
}

#[test]
fn ram_is_trimmed_to_whole_frames_and_capacity() {
    let frames = FrameAllocator::<1>::new(PhysAddr(BASE + 1)..PhysAddr(BASE + 3 * 4096 - 1));
    assert_eq!(frames.free_count(), 1);
    let frames = FrameAllocator::<1>::new(frame(0)..frame(1000));
    assert_eq!(frames.free_count(), 64);
}

#[test]
fn contiguous_alloc_skips_used_frames() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(8));
    frames.reserve(frame(2)..frame(3));
    assert_eq!(frames.alloc_contiguous(3), Some(frame(3)..frame(6)));
    assert_eq!(frames.alloc_contiguous(3), None);
    assert_eq!(frames.alloc_contiguous(2), Some(frame(0)..frame(2)));
    assert_eq!(frames.free_count(), 2);
}
