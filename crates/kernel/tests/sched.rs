use kernel::handle::{Handle, Handles, Object, WAIT};
use kernel::syscall::{EBADF, KILLED};
use kernel::{Event, Memory, Scheduler};
use mm::{Budget, PhysAddr};

/// The host's clamp: no speculation to bound.
struct Min;

impl kernel::Clamp for Min {
    fn clamp<const N: usize>(values: [u64; N], maxes: [u64; N]) -> [u64; N] {
        core::array::from_fn(|i| values[i].min(maxes[i]))
    }

    fn mask(value: u64, mask: u64) -> u64 {
        value & mask
    }
}

const STACK: PhysAddr = PhysAddr(0x9000);

type Id = (usize, u64);

/// A new process with `budget` frames and one thread at `frame` and `priority`; returns the thread and the process.
fn spawn_with<const N: usize, const P: usize>(
    sched: &mut Scheduler<N, P>,
    frame: usize,
    budget: usize,
    priority: u8,
) -> (Id, Id) {
    let process = sched.free_process().unwrap();
    let memory = Memory {
        budget: Budget::new(budget),
        next: 0,
    };
    sched.add_process(process, PhysAddr(frame as u64), memory, Handles::new());
    (thread(sched, process.0, frame, priority), process)
}

fn spawn<const N: usize, const P: usize>(sched: &mut Scheduler<N, P>, frame: usize) -> (Id, Id) {
    spawn_with(sched, frame, 0, 0)
}

/// A new thread of `process` at `frame` and `priority`.
fn thread<const N: usize, const P: usize>(
    sched: &mut Scheduler<N, P>,
    process: usize,
    frame: usize,
    priority: u8,
) -> Id {
    let slot = sched.free_slot().unwrap();
    sched.add(slot, process, (frame, STACK), priority);
    slot
}

/// A handle (wait) to `object` in the current process's table, counted as the board counts it.
fn give<const N: usize, const P: usize>(sched: &mut Scheduler<N, P>, object: Object) -> u64 {
    sched.held(object);
    sched.handles().insert(object, WAIT).unwrap()
}

/// Ends the current thread, as `thread_exit`, and switches; returns the next frame.
fn exit<const N: usize, const P: usize>(sched: &mut Scheduler<N, P>, code: u64) -> usize {
    let (slot, _) = sched.current();
    assert_eq!(sched.end(slot, code), (STACK, None));
    sched.switch(0xdead)
}

#[test]
fn exited_slot_is_skipped_then_reused() {
    let mut sched = Scheduler::<4, 4>::new();
    spawn(&mut sched, 0x100);
    spawn(&mut sched, 0x200);
    assert_eq!(sched.count(), 3);

    assert_eq!(sched.switch(0x10), 0x100);
    assert_eq!(exit(&mut sched, 0), 0x200);
    assert_eq!(sched.current().0, 2);
    assert_eq!(sched.process(), 2);
    assert_eq!(sched.space(2), PhysAddr(0x200));
    assert_eq!(sched.count(), 2);

    assert_eq!(sched.switch(0x210), 0x10);
    assert_eq!(sched.switch(0x11), 0x210, "the free slot 1 is skipped");

    spawn(&mut sched, 0x300);
    assert_eq!(sched.count(), 3);
    assert_eq!(sched.switch(0x220), 0x11);
    assert_eq!(sched.switch(0x12), 0x300, "slot 1 is reused");
    assert_eq!(sched.current().0, 1);
    assert_eq!(sched.process(), 1, "so is process index 1");
}

#[test]
fn reap_reports_the_exit_once_and_a_reused_index_is_ebadf() {
    let mut sched = Scheduler::<4, 4>::new();
    let (_, (index, old)) = spawn_with(&mut sched, 0x100, 12, 0);
    assert_eq!(sched.reap(index, old), Ok(None), "still running");

    sched.switch(0x10);
    exit(&mut sched, 7);
    assert_eq!(sched.reap(index, old), Ok(Some((7, 12))));
    assert_eq!(sched.reap(index, old), Ok(Some((7, 0))), "budget only once");

    let (reused, new) = sched.free_process().unwrap();
    assert_eq!(reused, index);
    assert_ne!(new, old);
    spawn(&mut sched, 0x200);
    assert_eq!(sched.reap(index, old), Err(EBADF), "never the new process");
    assert_eq!(sched.reap(index, new), Ok(None));
}

#[test]
fn blocked_tasks_are_skipped_until_woken_and_boot_runs_when_none_is_ready() {
    let mut sched = Scheduler::<4, 4>::new();
    spawn(&mut sched, 0x100);
    spawn(&mut sched, 0x200);

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
    assert_eq!(exit(&mut sched, 0), 0x210, "process 1's end woke 2");
}

#[test]
fn an_exited_child_stays_a_zombie_until_reaped_and_one_without_a_handle_frees_at_exit() {
    let mut sched = Scheduler::<4, 4>::new();
    spawn(&mut sched, 0x100);
    let (_, child) = spawn(&mut sched, 0x200);
    let (_, orphan) = spawn(&mut sched, 0x300);
    assert_eq!(sched.switch(0x10), 0x100);
    let process = Object::Process {
        index: child.0,
        generation: child.1,
    };
    give(&mut sched, process);

    assert_eq!(sched.switch(0x110), 0x200);
    assert_eq!(exit(&mut sched, 7), 0x300);
    assert_eq!(
        sched.free_process(),
        None,
        "the child's index stays reserved"
    );
    assert_eq!(exit(&mut sched, 0), 0x10);
    assert_eq!(
        sched.free_process().unwrap().0,
        orphan.0,
        "no handle: freed"
    );
    assert_eq!(sched.count(), 2, "a zombie has no threads");

    spawn(&mut sched, 0x400);
    assert_eq!(sched.free_process(), None);
    assert_eq!(sched.reap(child.0, child.1), Ok(Some((7, 0))));
    assert_eq!(sched.free_process().unwrap().0, child.0, "reaped: freed");
    assert_eq!(
        sched.reap(child.0, child.1),
        Ok(Some((7, 0))),
        "until reused"
    );
}

#[test]
fn closing_the_handle_frees_an_exited_child_and_lets_a_running_one_free_at_exit() {
    let mut sched = Scheduler::<3, 3>::new();
    spawn(&mut sched, 0x100);
    let (_, child) = spawn_with(&mut sched, 0x200, 12, 0);
    assert_eq!(sched.switch(0x10), 0x100);
    let process = Object::Process {
        index: child.0,
        generation: child.1,
    };
    let handle = give(&mut sched, process);
    assert_eq!(sched.switch(0x110), 0x200);
    assert_eq!(exit(&mut sched, 1), 0x10);
    assert_eq!(sched.free_process(), None);
    assert_eq!(sched.switch(0x11), 0x110);

    assert_eq!(sched.close(child.0, child.1 + 1), 0);
    assert_eq!(sched.free_process(), None, "another generation's close");
    sched.handles().close(Handle::new::<Min>(handle)).unwrap();
    assert_eq!(sched.close(child.0, child.1), 12, "the budget, as reap");
    assert_eq!(sched.free_process().unwrap().0, child.0, "closed: freed");

    let (_, child) = spawn(&mut sched, 0x300);
    let process = Object::Process {
        index: child.0,
        generation: child.1,
    };
    let handle = give(&mut sched, process);
    sched.handles().close(Handle::new::<Min>(handle)).unwrap();
    assert_eq!(sched.close(child.0, child.1), 0, "still running");
    assert_eq!(sched.switch(0x111), 0x300);
    exit(&mut sched, 2);
    assert_eq!(
        sched.free_process().unwrap().0,
        child.0,
        "closed first: freed at exit"
    );
}

#[test]
fn a_process_ends_with_its_last_thread_and_its_code() {
    let mut sched = Scheduler::<4, 3>::new();
    let (main, process) = spawn(&mut sched, 0x100);
    let second = thread(&mut sched, process.0, 0x200, 0);
    assert_eq!(sched.free_process().unwrap().0, 2, "one index for both");
    assert_eq!(sched.threads(process.0), 2);
    assert_eq!(sched.process_of(second.0), process.0);

    sched.block(Event::Exit(process.0));
    assert_eq!(sched.switch(0x10), 0x100);
    assert_eq!((sched.process(), sched.generation()), process);
    assert_eq!(exit(&mut sched, 3), 0x200, "the boot context still waits");
    assert_eq!(sched.process_live(process.0, process.1), Ok(true));
    assert_eq!(sched.thread_of(process.0), Some(second.0));
    assert_eq!(sched.thread_live(main.0, main.1), Ok(false));

    assert_eq!(exit(&mut sched, 5), 0x10, "the last thread woke it");
    assert_eq!(sched.process_live(process.0, process.1), Ok(false));
    assert_eq!(sched.thread_of(process.0), None);
    assert_eq!(sched.reap(process.0, process.1), Ok(Some((5, 0))));
}

#[test]
fn a_join_reports_the_code_once_and_a_held_thread_stays_a_zombie() {
    let mut sched = Scheduler::<4, 3>::new();
    let (_, process) = spawn(&mut sched, 0x100);
    let held = thread(&mut sched, process.0, 0x200, 0);
    let free = thread(&mut sched, process.0, 0x300, 0);
    sched.block(Event::Idle);
    assert_eq!(sched.switch(0x10), 0x100);
    let object = Object::Thread {
        slot: held.0,
        generation: held.1,
    };
    let handle = give(&mut sched, object);
    assert_eq!(sched.join(held.0, held.1), Ok(None), "still running");
    sched.block(Event::Join(held.0));

    assert_eq!(sched.switch(0x110), 0x200);
    assert_eq!(exit(&mut sched, 9), 0x300, "the joiner woke, after 2");
    assert_eq!(exit(&mut sched, 4), 0x110);
    assert_eq!(sched.free_slot().unwrap().0, free.0, "no handle: freed");
    let reused = thread(&mut sched, process.0, 0x400, 0);
    assert_eq!(sched.free_slot(), None, "the held thread's slot is kept");

    sched.close_thread(held.0, held.1 + 1);
    assert_eq!(sched.free_slot(), None, "another generation's close");
    assert_eq!(sched.join(held.0, held.1), Ok(Some(9)));
    assert_eq!(sched.free_slot().unwrap().0, held.0, "joined: freed");
    assert_eq!(sched.join(free.0, free.1), Err(EBADF), "a reused slot");
    assert_eq!(sched.join(reused.0, reused.1), Ok(None));

    sched.handles().close(Handle::new::<Min>(handle)).unwrap();
    let held = thread(&mut sched, process.0, 0x500, 0);
    let object = Object::Thread {
        slot: held.0,
        generation: held.1,
    };
    give(&mut sched, object);
    let (stack, blocked) = sched.end(held.0, KILLED);
    assert_eq!((stack, blocked), (STACK, None));
    assert_eq!(sched.free_slot(), None, "killed, held: a zombie");
    sched.take_handles(process.0);
    sched.close_thread(held.0, held.1);
    assert_eq!(sched.free_slot().unwrap().0, held.0, "closed: freed");
}

#[test]
fn highest_priority_runs_round_robin_within_a_level_and_a_waiter_lends_its_priority() {
    let mut sched = Scheduler::<5, 5>::new();
    let ((low, _), _) = spawn_with(&mut sched, 0x100, 0, 1);
    spawn_with(&mut sched, 0x200, 0, 2);
    spawn_with(&mut sched, 0x300, 0, 2);
    let ((high, _), _) = spawn_with(&mut sched, 0x400, 0, 3);

    assert_eq!(sched.switch(0x10), 0x400);
    assert_eq!(sched.switch(0x400), 0x400, "alone at its level");
    sched.boost(low);
    sched.block(Event::Lock(5));
    assert_eq!(sched.switch(0x400), 0x100, "low runs at high's priority");

    sched.unboost(low, |e| e == Event::Lock(5));
    assert_eq!(
        sched.switch(0x100),
        0x100,
        "high still waits on a mutex low owns"
    );
    sched.unboost(low, |e| e == Event::Lock(6));
    assert_eq!(sched.switch(0x100), 0x200, "back to 1: the 2s run");
    assert_eq!(sched.switch(0x200), 0x300, "round robin");
    assert_eq!(sched.switch(0x300), 0x200);

    sched.wake(Event::Lock(5));
    assert_eq!(sched.switch(0x200), 0x400);
    assert_eq!(sched.current().0, high);
}

#[test]
fn ending_a_waiter_reports_its_event_so_the_owner_drops_its_boost_and_is_outranked() {
    let mut sched = Scheduler::<4, 4>::new();
    let ((low, _), _) = spawn_with(&mut sched, 0x100, 0, 1);
    spawn_with(&mut sched, 0x200, 0, 2);
    let ((high, _), _) = spawn_with(&mut sched, 0x300, 0, 3);

    assert_eq!(sched.switch(0x10), 0x300);
    sched.boost(low);
    sched.block(Event::Lock(5));
    assert_eq!(sched.switch(0x300), 0x100);
    assert!(!sched.outranked(), "low runs at high's priority");

    assert_eq!(sched.end(high, KILLED), (STACK, Some(Event::Lock(5))));
    sched.unboost(low, |e| e == Event::Lock(5));
    assert!(sched.outranked(), "back to 1, below the 2");
    assert_eq!(sched.switch(0x100), 0x200);
}

#[test]
fn a_zombie_thread_stays_until_its_last_handle_closes_and_a_stale_close_counts_for_nothing() {
    let mut sched = Scheduler::<3, 2>::new();
    let (_, process) = spawn(&mut sched, 0x100);
    let held = thread(&mut sched, process.0, 0x200, 0);
    let object = Object::Thread {
        slot: held.0,
        generation: held.1,
    };
    give(&mut sched, object);
    give(&mut sched, object);
    sched.block(Event::Idle);
    assert_eq!(sched.switch(0x10), 0x100);
    assert_eq!(sched.switch(0x110), 0x200);
    assert_eq!(exit(&mut sched, 4), 0x110);
    sched.close_thread(held.0, held.1);
    assert_eq!(sched.free_slot(), None, "one handle left: a zombie");
    sched.close_thread(held.0, held.1);
    let (slot, generation) = sched.free_slot().unwrap();
    assert_eq!(slot, held.0, "the last one frees it");

    let reused = thread(&mut sched, process.0, 0x300, 0);
    sched.close_thread(held.0, held.1);
    assert_eq!(sched.switch(0x111), 0x300);
    assert_eq!(exit(&mut sched, 0), 0x111);
    assert_eq!(
        sched.free_slot(),
        Some((reused.0, generation + 1)),
        "an older generation's close neither counted nor kept it"
    );
}
