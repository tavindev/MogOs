use mm::{Budget, FrameAllocator, PhysAddr};

const BASE: u64 = 0x4000_0000;

fn frame(i: u64) -> PhysAddr {
    PhysAddr(BASE + i * 4096)
}

#[test]
fn a_charge_counts_against_the_budget_until_refunded() {
    let budget = Budget::new(10);
    assert!(budget.charge(8));
    assert!(!budget.charge(3));
    assert_eq!(budget.remaining(), 2);
    budget.refund(8);
    assert_eq!(budget.remaining(), 10);
}

#[test]
fn out_of_frames_charges_nothing() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(2));
    let budget = Budget::new(10);
    assert_eq!(budget.alloc_contiguous(&mut frames, 3), None);
    budget.alloc(&mut frames).unwrap();
    budget.alloc(&mut frames).unwrap();
    assert_eq!(budget.alloc(&mut frames), None);
    assert_eq!(budget.remaining(), 8);
}

#[test]
fn shrink_and_charge_over_the_limit_change_nothing() {
    let budget = Budget::new(10);
    assert!(budget.charge(4));
    assert!(!budget.shrink(7));
    assert!(!budget.charge(7));
    assert_eq!((budget.limit(), budget.remaining()), (10, 6));
    assert!(budget.shrink(6));
    assert_eq!((budget.limit(), budget.remaining()), (4, 0));
    budget.grow(6);
    assert_eq!(budget.take(), 10);
    assert_eq!(budget.limit(), 0);
}
