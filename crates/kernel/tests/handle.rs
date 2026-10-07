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
