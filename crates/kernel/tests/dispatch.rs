use kernel::Process;
use kernel::handle::{Handles, Object, READ, Seen, Table, WRITE};
use kernel::network::OP_CONNECT;
use kernel::syscall::{Call, EACCES, EADDRNOTAVAIL, EINVAL, ENOTDIR, EROFS, NetCall, dispatch};
use mogfs::ROOT;

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

const SYNC: u64 = 15;
const UNLINK: u64 = 16;
const RENAME: u64 = 17;
const BIND: u64 = 21;
const LISTEN: u64 = 22;
const IO_SUBMIT: u64 = 23;
const USER: u64 = 1 << 32;

/// A live table holding `handles`.
fn table(handles: &Handles) -> Table {
    let table = Table::new();
    table.commit(
        &mut Process {
            next: 0,
            alone: false,
        },
        handles,
    );
    table
}

#[test]
fn unlink_and_rename_need_a_writable_mogfs_directory() {
    let mut handles = Handles::new();
    let archive = handles.insert(Object::Archive, READ | WRITE).unwrap();
    let read_only = handles.insert(Object::Dir(ROOT), READ).unwrap();
    let writable = handles.insert(Object::Dir(ROOT), WRITE).unwrap();
    let file = handles.insert(Object::Node(ROOT), WRITE).unwrap();
    let handles = table(&handles);
    for (dir, error) in [(archive, EROFS), (read_only, EACCES), (file, ENOTDIR)] {
        let unlink = dispatch::<Min>(
            UNLINK,
            &[dir, USER, 1, 0, 0, 0, 0],
            &handles,
            &mut Seen::default(),
        );
        assert_eq!(unlink.err(), Some(error));
        let from = dispatch::<Min>(
            RENAME,
            &[dir, USER, 1, writable, USER, 1, 0],
            &handles,
            &mut Seen::default(),
        );
        assert_eq!(from.err(), Some(error));
        let to = dispatch::<Min>(
            RENAME,
            &[writable, USER, 1, dir, USER, 1, 0],
            &handles,
            &mut Seen::default(),
        );
        assert_eq!(to.err(), Some(error));
    }
    let unlink = dispatch::<Min>(
        UNLINK,
        &[writable, USER, 1, 0, 0, 0, 0],
        &handles,
        &mut Seen::default(),
    );
    assert!(matches!(unlink, Ok(Call::Unlink { .. })));
    let rename = dispatch::<Min>(
        RENAME,
        &[writable, USER, 1, writable, USER, 1, 0],
        &handles,
        &mut Seen::default(),
    );
    assert!(matches!(rename, Ok(Call::Rename { .. })));
}

#[test]
fn bind_and_listen_change_a_socket_so_they_need_its_write_right() {
    let mut handles = Handles::new();
    let sock = Object::Socket(kernel::network::Sock {
        index: 0,
        generation: 1,
    });
    let read_only = handles.insert(sock, READ).unwrap();
    let writable = handles.insert(sock, WRITE).unwrap();
    let handles = table(&handles);
    let elsewhere = dispatch::<Min>(
        BIND,
        &[writable, 80, 0x0a00_020f, 0, 0, 0, 0],
        &handles,
        &mut Seen::default(),
    );
    assert_eq!(elsewhere.err(), Some(EADDRNOTAVAIL));
    for nr in [BIND, LISTEN] {
        let call = dispatch::<Min>(
            nr,
            &[read_only, 80, 0, 0, 0, 0, 0],
            &handles,
            &mut Seen::default(),
        );
        assert_eq!(call.err(), Some(EACCES), "syscall {nr}");
        let call = dispatch::<Min>(
            nr,
            &[writable, 80, 0, 0, 0, 0, 0],
            &handles,
            &mut Seen::default(),
        );
        assert!(matches!(call, Ok(Call::Net(_))), "syscall {nr}");
    }
}

#[test]
fn sync_takes_any_mogfs_handle_without_a_right() {
    let mut handles = Handles::new();
    let dir = handles.insert(Object::Dir(ROOT), 0).unwrap();
    let file = handles.insert(Object::Node(ROOT), 0).unwrap();
    let console = handles.insert(Object::Console, WRITE).unwrap();
    let handles = table(&handles);
    for handle in [dir, file] {
        let sync = dispatch::<Min>(
            SYNC,
            &[handle, 0, 0, 0, 0, 0, 0],
            &handles,
            &mut Seen::default(),
        );
        assert!(matches!(sync, Ok(Call::Sync)));
    }
    let sync = dispatch::<Min>(
        SYNC,
        &[console, 0, 0, 0, 0, 0, 0],
        &handles,
        &mut Seen::default(),
    );
    assert_eq!(sync.err(), Some(ENOTDIR));
}

#[test]
fn connect_takes_an_address_and_port_that_fit() {
    let mut handles = Handles::new();
    let sock = Object::Socket(kernel::network::Sock {
        index: 0,
        generation: 1,
    });
    let sock = handles.insert(sock, READ | WRITE).unwrap();
    let handles = table(&handles);
    let connect = |ip, port| {
        dispatch::<Min>(
            IO_SUBMIT,
            &[sock, OP_CONNECT, ip, port, 7, 0, 0],
            &handles,
            &mut Seen::default(),
        )
    };
    assert_eq!(connect(1 << 32, 80).err(), Some(EINVAL));
    assert_eq!(connect(0x7f00_0001, 1 << 16).err(), Some(EINVAL));
    let ok = connect(0x7f00_0001, 80);
    assert!(matches!(
        ok,
        Ok(Call::Net(NetCall::Submit {
            peer: (0x7f00_0001, 80),
            ..
        }))
    ));
    let op = dispatch::<Min>(
        IO_SUBMIT,
        &[sock, 4, USER, 1, 0, 0, 0],
        &handles,
        &mut Seen::default(),
    );
    assert_eq!(op.err(), Some(EINVAL));
}
