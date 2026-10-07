use kernel::handle::{Handles, Object, READ, WRITE};
use kernel::syscall::{Call, EACCES, ENOTDIR, EROFS, dispatch};
use mogfs::ROOT;

const SYNC: u64 = 15;
const UNLINK: u64 = 16;
const RENAME: u64 = 17;
const USER: u64 = 1 << 32;

#[test]
fn unlink_and_rename_need_a_writable_mogfs_directory() {
    let mut handles = Handles::new();
    let archive = handles.insert(Object::Archive, READ | WRITE).unwrap();
    let read_only = handles.insert(Object::Dir(ROOT), READ).unwrap();
    let writable = handles.insert(Object::Dir(ROOT), WRITE).unwrap();
    let file = handles.insert(Object::Node(ROOT), WRITE).unwrap();
    for (dir, error) in [(archive, EROFS), (read_only, EACCES), (file, ENOTDIR)] {
        let unlink = dispatch(UNLINK, &[dir, USER, 1, 0, 0, 0, 0], &mut handles);
        assert_eq!(unlink.err(), Some(error));
        let from = dispatch(RENAME, &[dir, USER, 1, writable, USER, 1, 0], &mut handles);
        assert_eq!(from.err(), Some(error));
        let to = dispatch(RENAME, &[writable, USER, 1, dir, USER, 1, 0], &mut handles);
        assert_eq!(to.err(), Some(error));
    }
    let unlink = dispatch(UNLINK, &[writable, USER, 1, 0, 0, 0, 0], &mut handles);
    assert!(matches!(unlink, Ok(Call::Unlink { .. })));
    let rename = dispatch(
        RENAME,
        &[writable, USER, 1, writable, USER, 1, 0],
        &mut handles,
    );
    assert!(matches!(rename, Ok(Call::Rename { .. })));
}

#[test]
fn sync_takes_any_mogfs_handle_without_a_right() {
    let mut handles = Handles::new();
    let dir = handles.insert(Object::Dir(ROOT), 0).unwrap();
    let file = handles.insert(Object::Node(ROOT), 0).unwrap();
    let console = handles.insert(Object::Console, WRITE).unwrap();
    for handle in [dir, file] {
        let sync = dispatch(SYNC, &[handle, 0, 0, 0, 0, 0, 0], &mut handles);
        assert!(matches!(sync, Ok(Call::Sync)));
    }
    let sync = dispatch(SYNC, &[console, 0, 0, 0, 0, 0, 0], &mut handles);
    assert_eq!(sync.err(), Some(ENOTDIR));
}
