use kernel::handle::{DUPLICATE, Handles, INIT_ARCHIVE, KILL, MAP, MAX_HANDLES, Object, WRITE};
use kernel::syscall::{EACCES, EBADF, EMFILE};

#[test]
fn closed_handle_and_its_reused_entry_reject_the_old_value() {
    let mut handles = Handles::init(1, 1, INIT_ARCHIVE);
    let old = handles.dup(0, WRITE).unwrap().0;
    handles.close(old).unwrap();
    assert_eq!(handles.get(old, WRITE), Err(EBADF));
    assert_eq!(handles.close(old), Err(EBADF));
    assert_eq!(handles.close(MAX_HANDLES as u64), Err(EBADF));
    assert_eq!(handles.close(1 << 32), Err(EBADF), "wrong generation");

    let (new, object) = handles.dup(0, WRITE).unwrap();
    assert_eq!(object, Object::Console);
    assert_eq!(new as u32, old as u32, "the closed entry is reused");
    assert_ne!(new, old);
    assert_eq!(handles.get(old, WRITE), Err(EBADF));
    assert_eq!(handles.get(new, WRITE), Ok(Object::Console));
}

#[test]
fn dup_fails_when_the_table_is_full() {
    let mut handles = Handles::init(1, 1, INIT_ARCHIVE);
    for _ in 3..MAX_HANDLES {
        handles.dup(0, WRITE).unwrap();
    }
    assert_eq!(handles.dup(0, WRITE), Err(EMFILE));
    assert_eq!(
        handles.get(1, KILL),
        Ok(Object::Process {
            slot: 1,
            generation: 1
        })
    );
}

#[test]
fn dup_rights_must_be_a_subset_and_need_duplicate() {
    let mut handles = Handles::init(1, 1, INIT_ARCHIVE);
    assert_eq!(handles.dup(0, WRITE | MAP), Err(EACCES));
    assert_eq!(handles.dup(0, 1 << 40), Err(EACCES));
    let no_write = handles.dup(0, DUPLICATE).unwrap().0;
    assert_eq!(handles.get(no_write, WRITE), Err(EACCES));
    let none = handles.dup(no_write, 0).unwrap().0;
    assert_eq!(handles.dup(none, 0), Err(EACCES), "no duplicate right");
    assert_eq!(handles.dup(1, KILL), Err(EACCES), "self lacks duplicate");
}

#[test]
fn split_moves_transferable_handles_or_nothing() {
    let handles = Handles::init(1, 1, INIT_ARCHIVE);
    let (rest, moved) = handles.split(&[0]).unwrap();
    assert_eq!(rest.get(0, WRITE), Err(EBADF), "moved out");
    assert_eq!(moved.get(0, WRITE), Ok(Object::Console));
    assert_eq!(moved.get(1, 0), Err(EBADF), "only what was passed");
    assert_eq!(
        handles.get(0, WRITE),
        Ok(Object::Console),
        "the original is unchanged"
    );

    assert_eq!(handles.split(&[0, 0]).err(), Some(EBADF), "passed twice");
    assert_eq!(handles.split(&[1]).err(), Some(EACCES), "no transfer right");
    let mut handles = handles;
    let no_transfer = handles.dup(0, WRITE).unwrap().0;
    assert_eq!(handles.split(&[0, no_transfer]).err(), Some(EACCES));
}
