use kernel::handle::{Handles, Object, READ, WRITE};
use kernel::syscall::{Call, EACCES, EADDRNOTAVAIL, ENOTDIR, EROFS, dispatch};
use mogfs::ROOT;

/// The host's clamp: no speculation to bound.
struct Min;

impl kernel::Clamp for Min {
    fn clamp(index: usize, len: usize) -> usize {
        index.min(len - 1)
    }
}

const SYNC: u64 = 15;
const UNLINK: u64 = 16;
const RENAME: u64 = 17;
const BIND: u64 = 21;
const LISTEN: u64 = 22;
const USER: u64 = 1 << 32;

#[test]
fn unlink_and_rename_need_a_writable_mogfs_directory() {
    let mut handles = Handles::<Min>::new();
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
fn bind_and_listen_change_a_socket_so_they_need_its_write_right() {
    let mut handles = Handles::<Min>::new();
    let sock = Object::Socket(kernel::network::Sock {
        index: 0,
        generation: 1,
    });
    let read_only = handles.insert(sock, READ).unwrap();
    let writable = handles.insert(sock, WRITE).unwrap();
    let elsewhere = dispatch(BIND, &[writable, 80, 0x0a00_020f, 0, 0, 0, 0], &mut handles);
    assert_eq!(elsewhere.err(), Some(EADDRNOTAVAIL));
    for nr in [BIND, LISTEN] {
        let call = dispatch(nr, &[read_only, 80, 0, 0, 0, 0, 0], &mut handles);
        assert_eq!(call.err(), Some(EACCES), "syscall {nr}");
        let call = dispatch(nr, &[writable, 80, 0, 0, 0, 0, 0], &mut handles);
        assert!(matches!(call, Ok(Call::Net(_))), "syscall {nr}");
    }
}

#[test]
fn sync_takes_any_mogfs_handle_without_a_right() {
    let mut handles = Handles::<Min>::new();
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
