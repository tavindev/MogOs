use std::hint::black_box;

use criterion::{BatchSize, BenchmarkGroup, Criterion, criterion_group, criterion_main};
use mm::{FrameAllocator, PhysAddr};

#[path = "../../../benches/thread_time.rs"]
mod thread_time;
use thread_time::ThreadTime;

const FRAMES: usize = 1000;
const RAM: core::ops::Range<PhysAddr> = PhysAddr(0x4000_0000)..PhysAddr(0x4800_0000);

/// Times `FRAMES` rounds of `alloc_contiguous(4)` then freeing the 4 frames, on the bitmap `setup` leaves.
fn contiguous(
    g: &mut BenchmarkGroup<ThreadTime>,
    name: &str,
    setup: impl Fn(&mut FrameAllocator<512>),
) {
    g.bench_function(name, |b| {
        b.iter_batched_ref(
            || {
                let mut frames = FrameAllocator::<512>::new(RAM);
                setup(&mut frames);
                frames
            },
            |frames| {
                for _ in 0..FRAMES {
                    let run = black_box(&mut *frames).alloc_contiguous(4).unwrap();
                    for i in 0..4 {
                        black_box(&mut *frames).free(PhysAddr(run.start.0 + i * 4096));
                    }
                }
            },
            BatchSize::SmallInput,
        )
    });
}

/// Allocates then frees `FRAMES` frames from a fresh 128 MiB allocator; then `alloc_contiguous(4)` + free on an
/// empty bitmap, behind the kernel's reserved prefix (725 frames, as at boot), and behind 4096 frames where every
/// fourth is used. Each iteration is `FRAMES` ops, so its time in us reads as ns per op.
fn frames(c: &mut Criterion<ThreadTime>) {
    let mut g = thread_time::group(c, "frames");
    g.bench_function("alloc+free", |b| {
        b.iter_batched_ref(
            || (FrameAllocator::<512>::new(RAM), Vec::with_capacity(FRAMES)),
            |(frames, held)| {
                for _ in 0..FRAMES {
                    held.push(black_box(&mut *frames).alloc().unwrap());
                }
                for &frame in held.iter() {
                    black_box(&mut *frames).free(frame);
                }
            },
            BatchSize::SmallInput,
        )
    });
    contiguous(&mut g, "contiguous(4)+free, empty", |_| {});
    contiguous(&mut g, "contiguous(4)+free, 725 reserved", |f| {
        f.reserve(RAM.start..PhysAddr(RAM.start.0 + 725 * 4096));
    });
    contiguous(&mut g, "contiguous(4)+free, fragmented", |f| {
        for i in (0..4096).step_by(4) {
            let frame = RAM.start.0 + i * 4096;
            f.reserve(PhysAddr(frame)..PhysAddr(frame + 4096));
        }
    });
}

criterion_group! {
    name = benches;
    config = thread_time::config();
    targets = frames
}
criterion_main!(benches);
