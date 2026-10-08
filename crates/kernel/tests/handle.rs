use kernel::handle::{
    DUPLICATE, Handle, Handles, INIT_ARCHIVE, KILL, MAP, MAX_HANDLES, Object, WRITE,
};
use kernel::syscall::{EACCES, EBADF, EMFILE};

fn h(value: u64) -> Handle {
    Handle::new::<Min>(value)
}

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

#[test]
fn closed_handle_and_its_reused_entry_reject_the_old_value() {
    let mut handles = Handles::init(1, 1, INIT_ARCHIVE);
    let old = handles.dup(h(0), WRITE).unwrap().0;
    handles.close(h(old)).unwrap();
    assert_eq!(handles.get(h(old), WRITE), Err(EBADF));
    assert_eq!(handles.close(h(old)), Err(EBADF));
    assert_eq!(handles.close(h(MAX_HANDLES as u64)), Err(EBADF));
    assert_eq!(handles.close(h(1 << 32)), Err(EBADF), "wrong generation");

    let (new, object) = handles.dup(h(0), WRITE).unwrap();
    assert_eq!(object, Object::Console);
    assert_eq!(new as u32, old as u32, "the closed entry is reused");
    assert_ne!(new, old);
    assert_eq!(handles.get(h(old), WRITE), Err(EBADF));
    assert_eq!(handles.get(h(new), WRITE), Ok(Object::Console));
}

#[test]
fn dup_fails_when_the_table_is_full() {
    let mut handles = Handles::init(1, 1, INIT_ARCHIVE);
    for _ in 3..MAX_HANDLES {
        handles.dup(h(0), WRITE).unwrap();
    }
    assert_eq!(handles.dup(h(0), WRITE), Err(EMFILE));
    assert_eq!(
        handles.get(h(1), KILL),
        Ok(Object::Process {
            index: 1,
            generation: 1
        })
    );
}

#[test]
fn dup_rights_must_be_a_subset_and_need_duplicate() {
    let mut handles = Handles::init(1, 1, INIT_ARCHIVE);
    assert_eq!(handles.dup(h(0), WRITE | MAP), Err(EACCES));
    assert_eq!(handles.dup(h(0), 1 << 40), Err(EACCES));
    let no_write = handles.dup(h(0), DUPLICATE).unwrap().0;
    assert_eq!(handles.get(h(no_write), WRITE), Err(EACCES));
    let none = handles.dup(h(no_write), 0).unwrap().0;
    assert_eq!(handles.dup(h(none), 0), Err(EACCES), "no duplicate right");
    assert_eq!(handles.dup(h(1), KILL), Err(EACCES), "self lacks duplicate");
}

#[test]
fn split_moves_transferable_handles_or_nothing() {
    let handles = Handles::init(1, 1, INIT_ARCHIVE);
    let (rest, moved) = handles.split::<Min>(&[0]).unwrap();
    assert_eq!(rest.get(h(0), WRITE), Err(EBADF), "moved out");
    assert_eq!(moved.get(h(0), WRITE), Ok(Object::Console));
    assert_eq!(moved.get(h(1), 0), Err(EBADF), "only what was passed");
    assert_eq!(
        handles.get(h(0), WRITE),
        Ok(Object::Console),
        "the original is unchanged"
    );

    assert_eq!(
        handles.split::<Min>(&[0, 0]).err(),
        Some(EBADF),
        "passed twice"
    );
    assert_eq!(
        handles.split::<Min>(&[1]).err(),
        Some(EACCES),
        "no transfer right"
    );
    let mut handles = handles;
    let no_transfer = handles.dup(h(0), WRITE).unwrap().0;
    assert_eq!(handles.split::<Min>(&[0, no_transfer]).err(), Some(EACCES));
}

#[test]
fn a_lookup_racing_close_and_dup_sees_the_old_or_the_new_entry_never_a_torn_one() {
    use kernel::Process;
    use kernel::handle::{Seen, Table};
    use kernel::syscall::EBADF;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

    let table = Table::new();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut process = Process { next: 0 };
            for i in 1..200_000usize {
                let mut handles = Handles::new();
                let object = match i % 3 {
                    0 => Some(Object::Process {
                        index: i,
                        generation: 3 * i as u64,
                    }),
                    1 => Some(Object::File {
                        start: i,
                        end: 3 * i,
                    }),
                    _ => None,
                };
                if let Some(object) = object {
                    handles.insert(object, WRITE).unwrap();
                }
                table.commit(&mut process, &handles);
            }
            done.store(true, Relaxed);
        });
        let (mut seen, mut read) = (0, 0);
        while !done.load(Relaxed) {
            match table.entry(h(0), &mut Seen::default()) {
                Ok((Object::Process { index, generation }, WRITE)) => {
                    assert_eq!(generation, 3 * index as u64)
                }
                Ok((Object::File { start, end }, WRITE)) => assert_eq!(end, 3 * start),
                Err(EBADF) => {}
                other => panic!("torn: {other:?}"),
            }
            read += 1;
            seen += table.unchanged(&Seen::default()) as u32;
        }
        assert!(read > 0 && seen == read);
    });
}

#[test]
fn a_table_write_changes_the_sequence_a_lookup_saw() {
    use kernel::Process;
    use kernel::handle::{Seen, Table};

    let table = Table::new();
    let mut process = Process { next: 0 };
    let mut handles = Handles::new();
    let console = handles.insert(Object::Console, WRITE).unwrap();
    table.commit(&mut process, &handles);
    let mut seen = Seen::default();
    assert_eq!(
        table.entry(h(console), &mut seen),
        Ok((Object::Console, WRITE))
    );
    assert!(table.unchanged(&seen));
    let other = table.insert(&mut process, Object::Archive, WRITE).unwrap();
    assert!(table.unchanged(&seen), "another entry");
    assert_eq!(table.close(&mut process, h(console)), Ok(Object::Console));
    assert!(!table.unchanged(&seen), "its entry closed");
    assert_eq!(table.entry(h(console), &mut Seen::default()), Err(EBADF));
    assert_eq!(
        table.take(&mut process).objects().collect::<Vec<_>>(),
        [Object::Archive]
    );
    assert_eq!(table.entry(h(other), &mut Seen::default()), Err(EBADF));
}
