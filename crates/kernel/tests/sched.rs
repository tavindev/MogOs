use kernel::handle::Object;
use kernel::syscall::{EBADF, KILLED};
use kernel::{Event, Scheduler};
use mm::PhysAddr;

const STACK: PhysAddr = PhysAddr(0x9000);

#[test]
fn a_task_woken_between_its_block_and_its_switch_is_not_picked_by_another_core() {
    let mut sched = Scheduler::<4, 4>::new();
    sched.start_cores(2, 0xd0);
    assert_eq!(sched.switch(1, 0xd1).0, 0xd1);
    let ((a, _), _) = spawn(&mut sched, 0x100);
    assert_eq!(sched.switch(1, 0xd1).0, 0x100);
    sched.block(1, Event::Pipe(1));
    sched.wake(Event::Pipe(1));
    assert_eq!(sched.switch(0, 0x10).0, 0x10, "a still runs on core 1");
    assert_eq!(sched.core_of(a), Some(1));
    assert_eq!(
        sched.switch(1, 0x110).0,
        0x110,
        "core 1 switches away, a is ready"
    );
}

type Id = (usize, u64);

/// A new process with one thread at `frame` and `priority`; returns the thread and the process.
fn spawn_with<const N: usize, const P: usize>(
    sched: &mut Scheduler<N, P>,
    frame: usize,
    priority: u8,
) -> (Id, Id) {
    let process = sched.free_process().unwrap();
    sched.add_process(process, PhysAddr(frame as u64));
    (thread(sched, process.0, frame, priority), process)
}

fn spawn<const N: usize, const P: usize>(sched: &mut Scheduler<N, P>, frame: usize) -> (Id, Id) {
    spawn_with(sched, frame, 0)
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

/// A handle to `object`, counted as the board counts it.
fn give<const N: usize, const P: usize>(sched: &mut Scheduler<N, P>, object: Object) {
    sched.held(object);
}

/// Ends the current thread, as `thread_exit`, and its process with its last thread (released at once), and switches;
/// returns the next frame.
fn exit<const N: usize, const P: usize>(sched: &mut Scheduler<N, P>, code: u64) -> usize {
    let (slot, process) = (sched.current(0).0, sched.process(0));
    let (stack, blocked, last) = sched.end(slot, code);
    assert_eq!((stack, blocked), (STACK, None));
    if last {
        sched.exited(process, code);
    }
    sched.switch(0, 0xdead).0
}

#[test]
fn exited_slot_is_skipped_then_reused() {
    let mut sched = Scheduler::<4, 4>::new();
    sched.start_cores(1, 0);
    spawn(&mut sched, 0x100);
    spawn(&mut sched, 0x200);
    assert_eq!(sched.count(), 3);

    assert_eq!(sched.switch(0, 0x10).0, 0x100);
    assert_eq!(exit(&mut sched, 0), 0x200);
    assert_eq!(sched.current(0).0, 2);
    assert_eq!(sched.process(0), 2);
    assert_eq!(sched.space(2), PhysAddr(0x200));
    assert_eq!(sched.count(), 2);

    assert_eq!(sched.switch(0, 0x210).0, 0x10);
    assert_eq!(sched.switch(0, 0x11).0, 0x210, "the free slot 1 is skipped");

    spawn(&mut sched, 0x300);
    assert_eq!(sched.count(), 3);
    assert_eq!(sched.switch(0, 0x220).0, 0x11);
    assert_eq!(sched.switch(0, 0x12).0, 0x300, "slot 1 is reused");
    assert_eq!(sched.current(0).0, 1);
    assert_eq!(sched.process(0), 1, "so is process index 1");
}

#[test]
fn reap_reports_the_exit_once_and_a_reused_index_is_ebadf() {
    let mut sched = Scheduler::<4, 4>::new();
    sched.start_cores(1, 0);
    let (_, (index, old)) = spawn_with(&mut sched, 0x100, 0);
    assert_eq!(sched.reap(index, old), Ok(None), "still running");

    sched.switch(0, 0x10);
    exit(&mut sched, 7);
    assert_eq!(sched.reap(index, old), Ok(Some(7)));
    assert_eq!(sched.reap(index, old), Ok(Some(7)), "until reused");

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
    sched.start_cores(1, 0);
    spawn(&mut sched, 0x100);
    spawn(&mut sched, 0x200);

    sched.block(0, Event::Idle);
    assert_eq!(sched.switch(0, 0x10).0, 0x100);
    sched.block(0, Event::Pipe(3));
    assert_eq!(
        sched.switch(0, 0x110).0,
        0x200,
        "boot is idle, 1 is blocked"
    );
    sched.block(0, Event::Exit(1));
    assert_eq!(sched.switch(0, 0x210).0, 0x10, "none ready: boot");
    assert_eq!(sched.count(), 3);

    sched.wake(Event::Pipe(3));
    assert_eq!(sched.switch(0, 0x11).0, 0x110, "woken");
    assert_eq!(sched.switch(0, 0x111).0, 0x11, "2 still blocked");
    assert_eq!(sched.switch(0, 0x12).0, 0x111);
    assert_eq!(exit(&mut sched, 0), 0x210, "process 1's end woke 2");
}

#[test]
fn an_exited_child_stays_a_zombie_until_reaped_and_one_without_a_handle_frees_at_exit() {
    let mut sched = Scheduler::<4, 4>::new();
    sched.start_cores(1, 0);
    spawn(&mut sched, 0x100);
    let (_, child) = spawn(&mut sched, 0x200);
    let (_, orphan) = spawn(&mut sched, 0x300);
    assert_eq!(sched.switch(0, 0x10).0, 0x100);
    let process = Object::Process {
        index: child.0,
        generation: child.1,
    };
    give(&mut sched, process);

    assert_eq!(sched.switch(0, 0x110).0, 0x200);
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
    assert_eq!(sched.reap(child.0, child.1), Ok(Some(7)));
    assert_eq!(sched.free_process().unwrap().0, child.0, "reaped: freed");
    assert_eq!(sched.reap(child.0, child.1), Ok(Some(7)), "until reused");
}

#[test]
fn closing_the_handle_frees_an_exited_child_and_lets_a_running_one_free_at_exit() {
    let mut sched = Scheduler::<3, 3>::new();
    sched.start_cores(1, 0);
    spawn(&mut sched, 0x100);
    let (_, child) = spawn_with(&mut sched, 0x200, 0);
    assert_eq!(sched.switch(0, 0x10).0, 0x100);
    let process = Object::Process {
        index: child.0,
        generation: child.1,
    };
    give(&mut sched, process);
    assert_eq!(sched.switch(0, 0x110).0, 0x200);
    assert_eq!(exit(&mut sched, 1), 0x10);
    assert_eq!(sched.free_process(), None);
    assert_eq!(sched.switch(0, 0x11).0, 0x110);

    assert!(!sched.close(child.0, child.1 + 1));
    assert_eq!(sched.free_process(), None, "another generation's close");
    assert!(sched.close(child.0, child.1), "freed, as reap");
    assert_eq!(sched.free_process().unwrap().0, child.0, "closed: freed");

    let (_, child) = spawn(&mut sched, 0x300);
    let process = Object::Process {
        index: child.0,
        generation: child.1,
    };
    give(&mut sched, process);
    assert!(!sched.close(child.0, child.1), "still running");
    assert_eq!(sched.switch(0, 0x111).0, 0x300);
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
    sched.start_cores(1, 0);
    let (main, process) = spawn(&mut sched, 0x100);
    let second = thread(&mut sched, process.0, 0x200, 0);
    assert_eq!(sched.free_process().unwrap().0, 2, "one index for both");
    assert_eq!(sched.threads(process.0), 2);
    assert_eq!(sched.process_of(second.0), process.0);

    sched.block(0, Event::Exit(process.0));
    assert_eq!(sched.switch(0, 0x10).0, 0x100);
    assert_eq!((sched.process(0), sched.generation(0)), process);
    assert_eq!(exit(&mut sched, 3), 0x200, "the boot context still waits");
    assert_eq!(sched.process_live(process.0, process.1), Ok(true));
    assert_eq!(sched.thread_of(process.0, 0), Some(second.0));
    assert_eq!(sched.thread_live(main.0, main.1), Ok(false));

    assert_eq!(exit(&mut sched, 5), 0x10, "the last thread woke it");
    assert_eq!(sched.process_live(process.0, process.1), Ok(false));
    assert_eq!(sched.thread_of(process.0, 0), None);
    assert_eq!(sched.reap(process.0, process.1), Ok(Some(5)));
}

#[test]
fn a_join_reports_the_code_once_and_a_held_thread_stays_a_zombie() {
    let mut sched = Scheduler::<4, 3>::new();
    sched.start_cores(1, 0);
    let (_, process) = spawn(&mut sched, 0x100);
    let held = thread(&mut sched, process.0, 0x200, 0);
    let free = thread(&mut sched, process.0, 0x300, 0);
    sched.block(0, Event::Idle);
    assert_eq!(sched.switch(0, 0x10).0, 0x100);
    let object = Object::Thread {
        slot: held.0,
        generation: held.1,
    };
    give(&mut sched, object);
    assert_eq!(sched.join(held.0, held.1), Ok(None), "still running");
    sched.block(0, Event::Join(held.0));

    assert_eq!(sched.switch(0, 0x110).0, 0x200);
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

    let held = thread(&mut sched, process.0, 0x500, 0);
    let object = Object::Thread {
        slot: held.0,
        generation: held.1,
    };
    give(&mut sched, object);
    let (stack, blocked, last) = sched.end(held.0, KILLED);
    assert_eq!((stack, blocked, last), (STACK, None, false));
    assert_eq!(sched.free_slot(), None, "killed, held: a zombie");
    sched.close_thread(held.0, held.1);
    assert_eq!(sched.free_slot().unwrap().0, held.0, "closed: freed");
}

#[test]
fn highest_priority_runs_round_robin_within_a_level_and_a_waiter_lends_its_priority() {
    let mut sched = Scheduler::<5, 5>::new();
    sched.start_cores(1, 0);
    let ((low, _), _) = spawn_with(&mut sched, 0x100, 1);
    spawn_with(&mut sched, 0x200, 2);
    spawn_with(&mut sched, 0x300, 2);
    let ((high, _), _) = spawn_with(&mut sched, 0x400, 3);

    assert_eq!(sched.switch(0, 0x10).0, 0x400);
    assert_eq!(sched.switch(0, 0x400).0, 0x400, "alone at its level");
    sched.boost(0, low);
    sched.block(0, Event::Lock(5));
    assert_eq!(
        sched.switch(0, 0x400).0,
        0x100,
        "low runs at high's priority"
    );

    sched.unboost(low, |e| e == Event::Lock(5));
    assert_eq!(
        sched.switch(0, 0x100).0,
        0x100,
        "high still waits on a mutex low owns"
    );
    sched.unboost(low, |e| e == Event::Lock(6));
    assert_eq!(sched.switch(0, 0x100).0, 0x200, "back to 1: the 2s run");
    assert_eq!(sched.switch(0, 0x200).0, 0x300, "round robin");
    assert_eq!(sched.switch(0, 0x300).0, 0x200);

    sched.wake(Event::Lock(5));
    assert_eq!(sched.switch(0, 0x200).0, 0x400);
    assert_eq!(sched.current(0).0, high);
}

#[test]
fn ending_a_waiter_reports_its_event_so_the_owner_drops_its_boost_and_is_outranked() {
    let mut sched = Scheduler::<4, 4>::new();
    sched.start_cores(1, 0);
    let ((low, _), _) = spawn_with(&mut sched, 0x100, 1);
    spawn_with(&mut sched, 0x200, 2);
    let ((high, _), _) = spawn_with(&mut sched, 0x300, 3);

    assert_eq!(sched.switch(0, 0x10).0, 0x300);
    sched.boost(0, low);
    sched.block(0, Event::Lock(5));
    assert_eq!(sched.switch(0, 0x300).0, 0x100);
    assert!(!sched.outranked(0), "low runs at high's priority");

    assert_eq!(sched.end(high, KILLED), (STACK, Some(Event::Lock(5)), true));
    sched.unboost(low, |e| e == Event::Lock(5));
    assert!(sched.outranked(0), "back to 1, below the 2");
    assert_eq!(sched.switch(0, 0x100).0, 0x200);
}

#[test]
fn a_zombie_thread_stays_until_its_last_handle_closes_and_a_stale_close_counts_for_nothing() {
    let mut sched = Scheduler::<3, 2>::new();
    sched.start_cores(1, 0);
    let (_, process) = spawn(&mut sched, 0x100);
    let held = thread(&mut sched, process.0, 0x200, 0);
    let object = Object::Thread {
        slot: held.0,
        generation: held.1,
    };
    give(&mut sched, object);
    give(&mut sched, object);
    sched.block(0, Event::Idle);
    assert_eq!(sched.switch(0, 0x10).0, 0x100);
    assert_eq!(sched.switch(0, 0x110).0, 0x200);
    assert_eq!(exit(&mut sched, 4), 0x110);
    sched.close_thread(held.0, held.1);
    assert_eq!(sched.free_slot(), None, "one handle left: a zombie");
    sched.close_thread(held.0, held.1);
    let (slot, generation) = sched.free_slot().unwrap();
    assert_eq!(slot, held.0, "the last one frees it");

    let reused = thread(&mut sched, process.0, 0x300, 0);
    sched.close_thread(held.0, held.1);
    assert_eq!(sched.switch(0, 0x111).0, 0x300);
    assert_eq!(exit(&mut sched, 0), 0x111);
    assert_eq!(
        sched.free_slot(),
        Some((reused.0, generation + 1)),
        "an older generation's close neither counted nor kept it"
    );
}

#[test]
fn a_woken_task_signals_an_idle_core_only_while_it_still_waits_for_one() {
    let mut sched = Scheduler::<4, 4>::new();
    sched.start_cores(2, 0xd0);
    assert_eq!(
        sched.switch(1, 0xd1).0,
        0xd1,
        "core 1 comes up: nothing to run"
    );
    spawn(&mut sched, 0x100);
    let ((b, _), _) = spawn(&mut sched, 0x200);
    sched.take_woken();
    sched.block(0, Event::Idle);
    assert_eq!(sched.switch(0, 0x10).0, 0x100);
    sched.block(0, Event::Pipe(1));
    assert_eq!(sched.switch(0, 0x110).0, 0x200);
    sched.block(0, Event::Pipe(2));
    sched.wake(Event::Pipe(1));
    assert_eq!(sched.take_woken(), 1, "a waits while core 1 idles");
    assert_eq!(
        sched.switch(0, 0x210).0,
        0x110,
        "b blocked: this core takes a"
    );
    sched.wake(Event::Pipe(2));
    sched.block(0, Event::Pipe(1));
    assert_eq!(sched.switch(0, 0x111).0, 0x210);
    assert_eq!(sched.current(0).0, b);
    assert_eq!(sched.take_woken(), 0, "b runs here: nothing waits");

    let mut one = Scheduler::<4, 4>::new();
    one.start_cores(1, 0xd0);
    spawn(&mut one, 0x100);
    assert_eq!(one.take_woken(), 0, "no core to signal");
}

#[test]
fn a_core_runs_only_tasks_no_other_core_runs_and_core_0_resumes_boot_once_every_core_idles() {
    let mut sched = Scheduler::<4, 4>::new();
    sched.start_cores(2, 0xd0);
    assert!(sched.idle(1));
    spawn(&mut sched, 0x50);
    assert_eq!(sched.claim_idle(0), None, "core 1 is not up yet");
    sched.take_woken();
    assert_eq!(
        sched.switch(1, 0xd1).0,
        0x50,
        "up, it reschedules and finds the task"
    );
    let (x, _) = sched.current(1);
    assert_eq!(sched.end(x, 0), (STACK, None, true));
    sched.exited(sched.process_of(x), 0);
    assert_eq!(sched.switch(1, 0xdead).0, 0xd1);
    let ((a, _), process) = spawn(&mut sched, 0x100);
    let b = thread(&mut sched, process.0, 0x200, 0);
    assert_eq!(sched.claim_idle(0), Some(1));
    assert_eq!(sched.claim_idle(0), None, "one signal per idle period");
    assert_eq!(sched.switch(1, 0xd1).0, 0x100);
    assert_eq!(sched.core_of(a), Some(1));
    assert_eq!(sched.switch(0, 0x10).0, 0x200, "a runs on core 1");
    assert_eq!(sched.switch(0, 0x210).0, 0x10, "round robin, never a");
    assert_eq!(sched.thread_of(process.0, 0), Some(b.0));
    assert_eq!(sched.threads_elsewhere(process.0, 0), 1 << a);

    sched.mark(a, 9);
    assert_eq!(sched.marked(1), Some(9));
    assert_eq!(sched.marked(0), None);
    assert_eq!(sched.end(a, 9), (STACK, None, false));
    assert_eq!(sched.marked(1), None);
    assert_eq!(sched.switch(1, 0xdead).0, 0x210, "b is free");
    sched.block(0, Event::Idle);
    assert_eq!(sched.switch(0, 0x11).0, 0xd0, "core 1 runs b: core 0 idles");
    assert_eq!(sched.claim_idle(1), Some(0));
    assert_eq!(sched.end(b.0, 0), (STACK, None, true));
    sched.exited(process.0, 0);
    assert!(sched.boot_waits());
    assert_eq!(sched.switch(1, 0xdead).0, 0xd1, "slot 0 is core 0's");
    assert_eq!(sched.switch(0, 0xd0).0, 0x11, "no core runs a task: boot");
    assert_eq!(sched.claim_idle(0), Some(1), "idle again: signalled again");
}

#[test]
fn a_process_whose_last_thread_ended_is_neither_live_nor_reapable_until_exited() {
    let mut sched = Scheduler::<4, 3>::new();
    sched.start_cores(1, 0);
    let (main, process) = spawn(&mut sched, 0x100);
    give(
        &mut sched,
        Object::Process {
            index: process.0,
            generation: process.1,
        },
    );
    let (_, _, last) = sched.end(main.0, KILLED);
    assert!(last);
    assert_eq!(
        sched.process_live(process.0, process.1),
        Ok(false),
        "kill skips it"
    );
    assert_eq!(sched.reap(process.0, process.1), Ok(None), "wait blocks");
    assert!(
        !sched.close(process.0, process.1),
        "the last close keeps its index"
    );
    assert_eq!(sched.free_process().unwrap().0, 2, "its index is not free");

    assert_eq!(sched.count(), 2, "the boot context and the release");
    sched.block(0, Event::Exit(process.0));
    assert_eq!(
        sched.to_idle(0, 0x10),
        (0, 0, 0),
        "core 0 idles for the release"
    );
    sched.exited(process.0, KILLED);
    assert_eq!(sched.count(), 1);
    assert_eq!(sched.switch(0, 0).0, 0x10, "the waiter woke");
    assert_eq!(
        sched.free_process().unwrap().0,
        process.0,
        "no handle left: freed"
    );
}

#[test]
fn a_waiter_marked_to_end_lends_no_priority() {
    let mut sched = Scheduler::<5, 5>::new();
    sched.start_cores(2, 0xd0);
    let ((low, _), _) = spawn_with(&mut sched, 0x100, 1);
    spawn_with(&mut sched, 0x200, 2);
    let ((high, _), _) = spawn_with(&mut sched, 0x300, 3);
    assert_eq!(sched.switch(1, 0xd1).0, 0x300);
    sched.block(0, Event::Idle);
    assert_eq!(sched.switch(0, 0x10).0, 0x200, "mid runs on core 0");

    // High, killed from core 0, reaches the mutex low owns before its own core ends it: it ends instead of waiting.
    sched.mark(high, KILLED);
    sched.boost(1, low);
    sched.end(high, KILLED);
    assert!(
        !sched.outranked(0),
        "low is not lifted above mid by a waiter that never waited"
    );
}

#[test]
fn a_core_back_from_a_release_goes_on_round_robin_after_the_thread_it_ended() {
    let mut sched = Scheduler::<5, 5>::new();
    sched.start_cores(1, 0xd0);
    spawn(&mut sched, 0x100);
    let (b, _) = spawn(&mut sched, 0x200);
    spawn(&mut sched, 0x300);
    sched.block(0, Event::Idle);
    assert_eq!(sched.switch(0, 0x10).0, 0x100);
    assert_eq!(sched.switch(0, 0x100).0, 0x200);
    let (_, _, last) = sched.end(b.0, 0);
    assert!(last);
    sched.to_idle(0, 0x200);
    assert_eq!(
        sched.switch(0, 0xd0).0,
        0x300,
        "the next after the ended thread, not the first"
    );
}
