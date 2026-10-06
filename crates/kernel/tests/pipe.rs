use kernel::pipe::{End, Pipes, SIZE};
use kernel::syscall::EPIPE;
use mm::PhysAddr;

#[test]
fn ring_wraps_writes_whole_or_blocks_and_ends_report_eof_and_epipe() {
    let mut pipes = Pipes::<2>::new();
    let read = pipes.free().unwrap();
    let write = End {
        write: true,
        ..read
    };
    pipes.create(read, PhysAddr(0x5000), (1, 1));
    let mut page = [0; SIZE];
    let mut out = [0; SIZE];
    let pipe = pipes.get(read).unwrap();

    assert_eq!(pipe.read(&page, &mut out[..1]), None, "empty: block");
    assert_eq!(pipe.read(&page, &mut []), Some(0));
    assert_eq!(pipe.write(&mut page, &[1; SIZE - 2]), Some(SIZE as i64 - 2));
    assert_eq!(
        pipe.read(&page, &mut out[..SIZE - 4]),
        Some(SIZE as i64 - 4)
    );
    assert_eq!(pipe.write(&mut page, &[2, 3, 4, 5, 6]), Some(5), "wraps");
    assert_eq!(pipe.read(&page, &mut out), Some(7));
    assert_eq!(out[..7], [1, 1, 2, 3, 4, 5, 6]);

    assert_eq!(pipe.write(&mut page, &[7; SIZE]), Some(SIZE as i64));
    assert_eq!(pipe.write(&mut page, &[8]), None, "full: block");
    assert_eq!(pipe.write(&mut page, &[]), Some(0));
    assert_eq!(pipe.read(&page, &mut out[..1]), Some(1));
    assert_eq!(pipe.write(&mut page, &[8, 8]), None, "atomic: all or block");
    assert_eq!(pipe.write(&mut page, &[8]), Some(1));
    assert_eq!(pipes.close(write), None);
    let pipe = pipes.get(read).unwrap();
    assert_eq!(pipe.read(&page, &mut out), Some(SIZE as i64));
    assert_eq!(out[SIZE - 1], 8);
    assert_eq!(pipe.read(&page, &mut out), Some(0), "no writer: EOF");

    pipes.open(write);
    assert_eq!(pipes.close(read), None);
    let pipe = pipes.get(write).unwrap();
    assert_eq!(pipe.write(&mut page, &[9]), Some(EPIPE), "no reader");
    assert_eq!(pipe.write(&mut page, &[]), Some(0), "empty: 0, as on Linux");
    assert_eq!(pipes.charged_to((1, 1)), 1);
    let freed = Some((PhysAddr(0x5000), (1, 1)));
    assert_eq!(pipes.close(write), freed, "the last handle frees it");
    assert!(pipes.get(read).is_none());
    assert_eq!(pipes.charged_to((1, 1)), 0);

    let reused = pipes.free().unwrap();
    assert_eq!(reused.index, read.index);
    pipes.create(reused, PhysAddr(0x6000), (2, 1));
    assert!(
        pipes.get(read).is_none(),
        "a stale end never reaches the new pipe"
    );
}
