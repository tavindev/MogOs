use kernel::handle::Handles;
use kernel::syscall::EBADF;
use kernel::{Event, Memory, Scheduler};
use mm::{Budget, PhysAddr};

const MEMORY: Memory = Memory {
    stack: PhysAddr(0x9000),
    budget: Budget::new(0),
    next: 0,
};

fn add<const N: usize>(sched: &mut Scheduler<N>, frame: usize, space: u64) {
    sched
        .add(frame, PhysAddr(space), MEMORY, |_, _| Handles::new())
        .unwrap();
}

#[test]
fn exited_slot_is_skipped_then_reused() {
    let mut sched = Scheduler::<4>::new();
    add(&mut sched, 0x100, 0x1000);
    add(&mut sched, 0x200, 0x2000);
    assert_eq!(sched.count(), 3);

    assert_eq!(sched.switch(0x10), 0x100);
    assert_eq!(sched.exit(0), (0x200, PhysAddr(0x9000)));
    assert_eq!(sched.current(), (2, PhysAddr(0x2000)));
    assert_eq!(sched.count(), 2);

    assert_eq!(sched.switch(0x210), 0x10);
    assert_eq!(sched.switch(0x11), 0x210, "the free slot 1 is skipped");
    assert_eq!(sched.current(), (2, PhysAddr(0x2000)));

    add(&mut sched, 0x300, 0x3000);
    assert_eq!(sched.count(), 3);
    assert_eq!(sched.switch(0x220), 0x11);
    assert_eq!(sched.switch(0x12), 0x300, "slot 1 is reused");
    assert_eq!(sched.current(), (1, PhysAddr(0x3000)));
}

#[test]
fn reap_reports_the_exit_once_and_a_reused_slot_is_ebadf() {
    let mut sched = Scheduler::<4>::new();
    let (slot, old) = sched.free_slot().unwrap();
    let memory = Memory {
        budget: Budget::new(12),
        ..MEMORY
    };
    sched
        .add(0x100, PhysAddr(0x1000), memory, |_, _| Handles::new())
        .unwrap();
    assert_eq!(sched.reap(slot, old), Ok(None), "still running");

    sched.switch(0x10);
    sched.exit(7);
    assert_eq!(sched.reap(slot, old), Ok(Some((7, 12))));
    assert_eq!(sched.reap(slot, old), Ok(Some((7, 0))), "budget only once");

    let (reused, new) = sched.free_slot().unwrap();
    assert_eq!(reused, slot);
    assert_ne!(new, old);
    add(&mut sched, 0x200, 0x2000);
    assert_eq!(sched.reap(slot, old), Err(EBADF), "never the new task");
    assert_eq!(sched.reap(slot, new), Ok(None));
}

#[test]
fn blocked_tasks_are_skipped_until_woken_and_boot_runs_when_none_is_ready() {
    let mut sched = Scheduler::<4>::new();
    add(&mut sched, 0x100, 0x1000);
    add(&mut sched, 0x200, 0x2000);

    sched.block(Event::Idle);
    assert_eq!(sched.switch(0x10), 0x100);
    sched.block(Event::Pipe(3));
    assert_eq!(sched.switch(0x110), 0x200, "boot is idle, 1 is blocked");
    sched.block(Event::Exit(1));
    assert_eq!(sched.switch(0x210), 0x10, "none ready: boot");
    assert_eq!(sched.count(), 3);

    sched.wake(Event::Pipe(3));
    assert_eq!(sched.switch(0x11), 0x110, "woken");
    assert_eq!(sched.switch(0x111), 0x11, "2 still blocked");
    assert_eq!(sched.switch(0x12), 0x111);
    assert_eq!(sched.exit(0).0, 0x210, "1's exit woke 2");
}
