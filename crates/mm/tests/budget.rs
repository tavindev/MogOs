use mm::{Budget, FrameAllocator, PhysAddr};

const BASE: u64 = 0x4000_0000;

fn frame(i: u64) -> PhysAddr {
    PhysAddr(BASE + i * 4096)
}

#[test]
fn out_of_frames_charges_nothing() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(2));
    let mut budget = Budget::new(10);
    assert_eq!(budget.alloc_contiguous(&mut frames, 3), None);
    budget.alloc(&mut frames).unwrap();
    budget.alloc(&mut frames).unwrap();
    assert_eq!(budget.alloc(&mut frames), None);
    assert_eq!(budget.remaining(), 8);
}
