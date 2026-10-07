use kernel::handle::{EXEC, Handles, Object};
use kernel::syscall::{E2BIG, EFAULT, EINVAL, MAX_ARGS, argc, dispatch};

#[test]
fn argc_counts_nul_terminated_strings_up_to_the_limit() {
    assert_eq!(argc(b""), Ok(0));
    assert_eq!(argc(b"ls\0"), Ok(1));
    assert_eq!(argc(b"a\0\0b c\0"), Ok(3));
    assert_eq!(argc(b"ls"), Err(EINVAL));
    assert_eq!(argc(b"a\0b"), Err(EINVAL));
    let most = [*b"x\0"; MAX_ARGS].concat();
    assert_eq!(argc(&most), Ok(MAX_ARGS));
    assert_eq!(argc(&[most, b"x\0".to_vec()].concat()), Err(E2BIG));
}

#[test]
fn spawn_checks_the_argument_buffer() {
    const SPAWN: u64 = 6;
    let mut handles = Handles::new();
    let exe = handles
        .insert(Object::File { start: 0, end: 0 }, EXEC)
        .unwrap();
    let user = 1 << 32;
    let spawn = |handles: &mut Handles, ptr: u64, len: u64| {
        dispatch(SPAWN, &[exe, 0, 0, 0, 0, ptr, len], handles).err()
    };
    assert_eq!(spawn(&mut handles, user, 4096), None);
    assert_eq!(spawn(&mut handles, user, 4097), Some(E2BIG));
    assert_eq!(spawn(&mut handles, 0x1000, 1), Some(EFAULT));
    assert_eq!(spawn(&mut handles, u64::MAX, 2), Some(EFAULT));
    assert_eq!(spawn(&mut handles, 0, 0), None);
}
