use kernel::handle::{Handles, Object, WAIT};
use kernel::syscall::EBADF;
use kernel::{Event, Memory, Scheduler};
use mm::{Budget, PhysAddr};

const MEMORY: Memory = Memory {
    stack: PhysAddr(0x9000),
    budget: Budget::new(0),
    next: 0,
};

fn add<const N: usize>(sched: &mut Scheduler<N>, frame: usize, space: u64) -> (usize, u64) {
    let slot = sched.free_slot().unwrap();
    sched.add(slot, frame, PhysAddr(space), MEMORY, Handles::new());
    slot
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
    sched.add((slot, old), 0x100, PhysAddr(0x1000), memory, Handles::new());
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

#[test]
fn an_exited_child_keeps_its_slot_until_reaped_and_one_without_a_handle_frees_at_exit() {
    let mut sched = Scheduler::<4>::new();
    add(&mut sched, 0x100, 0x1000);
    let child = add(&mut sched, 0x200, 0x2000);
    let orphan = add(&mut sched, 0x300, 0x3000);
    assert_eq!(sched.switch(0x10), 0x100);
    let process = Object::Process {
        slot: child.0,
        generation: child.1,
    };
    sched.handles().insert(process, WAIT).unwrap();

    assert_eq!(sched.switch(0x110), 0x200);
    assert_eq!(sched.exit(7).0, 0x300);
    assert_eq!(sched.free_slot(), None, "the child's slot stays reserved");
    assert_eq!(sched.exit(0).0, 0x10);
    assert_eq!(sched.free_slot().unwrap().0, orphan.0, "no handle: freed");
    assert_eq!(sched.count(), 2, "an unreaped child is not a task");

    add(&mut sched, 0x400, 0x4000);
    assert_eq!(sched.free_slot(), None);
    assert_eq!(sched.reap(child.0, child.1), Ok(Some((7, 0))));
    assert_eq!(sched.free_slot().unwrap().0, child.0, "reaped: freed");
    assert_eq!(
        sched.reap(child.0, child.1),
        Ok(Some((7, 0))),
        "until reused"
    );
}

#[test]
fn closing_the_handle_frees_an_exited_child_and_lets_a_running_one_free_at_exit() {
    let mut sched = Scheduler::<3>::new();
    add(&mut sched, 0x100, 0x1000);
    let child = add(&mut sched, 0x200, 0x2000);
    assert_eq!(sched.switch(0x10), 0x100);
    let process = Object::Process {
        slot: child.0,
        generation: child.1,
    };
    let handle = sched.handles().insert(process, WAIT).unwrap();
    assert_eq!(sched.switch(0x110), 0x200);
    assert_eq!(sched.exit(1).0, 0x10);
    assert_eq!(sched.free_slot(), None);
    assert_eq!(sched.switch(0x11), 0x110);

    sched.close(child.0, child.1 + 1);
    assert_eq!(sched.free_slot(), None, "another generation's close");
    sched.handles().close(handle).unwrap();
    sched.close(child.0, child.1);
    assert_eq!(sched.free_slot().unwrap().0, child.0, "closed: freed");

    let child = add(&mut sched, 0x300, 0x3000);
    let process = Object::Process {
        slot: child.0,
        generation: child.1,
    };
    let handle = sched.handles().insert(process, WAIT).unwrap();
    sched.handles().close(handle).unwrap();
    sched.close(child.0, child.1);
    assert_eq!(sched.switch(0x111), 0x300);
    sched.exit(2);
    assert_eq!(
        sched.free_slot().unwrap().0,
        child.0,
        "closed first: freed at exit"
    );
}
