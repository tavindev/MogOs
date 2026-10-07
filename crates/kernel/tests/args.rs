use kernel::handle::{EXEC, Handles, Object};
use kernel::syscall::{E2BIG, EFAULT, EINVAL, MAX_ARGS, argc, dispatch};

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
fn argc_counts_nul_terminated_strings_up_to_the_limit() {
    assert_eq!(argc(b""), Ok(0));
    assert_eq!(argc(b"ls\0"), Ok(1));
    assert_eq!(argc(b"a\0\0b c\0"), Ok(3));
    assert_eq!(argc(b"ls"), Err(EINVAL));
    assert_eq!(argc(b"a\0b"), Err(EINVAL));
    let most = [*b"x\0"; MAX_ARGS].concat();
    assert_eq!(argc(&most), Ok(MAX_ARGS));
    assert_eq!(argc(&[most, b"x\0".to_vec()].concat()), Err(E2BIG));
    let mut long = vec![b'x'; 4096];
    long[4095] = 0;
    assert_eq!(argc(&long), Ok(1));
    long.push(0);
    assert_eq!(argc(&long), Err(E2BIG));
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
        dispatch::<Min>(SPAWN, &[exe, 0, 0, 0, 0, ptr, len], handles).err()
    };
    assert_eq!(spawn(&mut handles, user, 4096), None);
    assert_eq!(spawn(&mut handles, user, 4097), Some(E2BIG));
    assert_eq!(spawn(&mut handles, 0x1000, 1), Some(EFAULT));
    assert_eq!(spawn(&mut handles, u64::MAX, 2), Some(EFAULT));
    assert_eq!(spawn(&mut handles, 0, 0), None);
}
