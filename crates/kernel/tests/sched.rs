use kernel::Scheduler;
use kernel::handle::Handles;
use mm::PhysAddr;

#[test]
fn exited_slot_is_skipped_then_reused() {
    let mut sched = Scheduler::<4>::new();
    sched
        .add(0x100, PhysAddr(0x1000), |_| Handles::new())
        .unwrap();
    sched
        .add(0x200, PhysAddr(0x2000), |_| Handles::new())
        .unwrap();
    assert_eq!(sched.count(), 3);

    assert_eq!(sched.switch(0x10), 0x100);
    assert_eq!(sched.exit(), 0x200);
    assert_eq!(sched.current(), (2, PhysAddr(0x2000)));
    assert_eq!(sched.count(), 2);

    assert_eq!(sched.switch(0x210), 0x10);
    assert_eq!(sched.switch(0x11), 0x210, "the free slot 1 is skipped");
    assert_eq!(sched.current(), (2, PhysAddr(0x2000)));

    sched
        .add(0x300, PhysAddr(0x3000), |_| Handles::new())
        .unwrap();
    assert_eq!(sched.count(), 3);
    assert_eq!(sched.switch(0x220), 0x11);
    assert_eq!(sched.switch(0x12), 0x300, "slot 1 is reused");
    assert_eq!(sched.current(), (1, PhysAddr(0x3000)));
}
