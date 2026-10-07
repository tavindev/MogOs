use std::hint::black_box;
use std::time::Instant;

use mm::{FrameAllocator, PhysAddr};

const RUNS: usize = 51;
const FRAMES: usize = 1000;
const RAM: core::ops::Range<PhysAddr> = PhysAddr(0x4000_0000)..PhysAddr(0x4800_0000);

fn report(name: &str, mut samples: Vec<f64>) {
    samples.sort_by(f64::total_cmp);
    println!(
        "{name}: min {:.1} ns/op, median {:.1} ns/op ({RUNS} runs x {FRAMES})",
        samples[0],
        samples[RUNS / 2]
    );
}

/// Times `FRAMES` rounds of `alloc_contiguous(4)` then freeing the 4 frames, on the bitmap `setup` leaves.
fn contiguous(name: &str, setup: impl Fn(&mut FrameAllocator<512>)) {
    let samples = (0..RUNS)
        .map(|_| {
            let mut frames = FrameAllocator::<512>::new(RAM);
            setup(&mut frames);
            let start = Instant::now();
            for _ in 0..FRAMES {
                let run = black_box(&mut frames).alloc_contiguous(4).unwrap();
                for i in 0..4 {
                    black_box(&mut frames).free(PhysAddr(run.start.0 + i * 4096));
                }
            }
            start.elapsed().as_nanos() as f64 / FRAMES as f64
        })
        .collect();
    report(name, samples);
}

/// Allocates then frees `FRAMES` frames from a fresh 128 MiB allocator; then `alloc_contiguous(4)` + free on an
/// empty bitmap, behind the kernel's reserved prefix (725 frames, as at boot), and behind 4096 frames where every
/// fourth is used. Prints min and median ns per op.
fn main() {
    let samples = (0..RUNS)
        .map(|_| {
            let mut frames = FrameAllocator::<512>::new(RAM);
            let mut held = Vec::with_capacity(FRAMES);
            let start = Instant::now();
            for _ in 0..FRAMES {
                held.push(black_box(&mut frames).alloc().unwrap());
            }
            for &frame in &held {
                black_box(&mut frames).free(frame);
            }
            start.elapsed().as_nanos() as f64 / FRAMES as f64
        })
        .collect();
    report("frames alloc+free", samples);
    contiguous("frames contiguous(4)+free, empty", |_| {});
    contiguous("frames contiguous(4)+free, 725 reserved", |f| {
        f.reserve(RAM.start..PhysAddr(RAM.start.0 + 725 * 4096));
    });
    contiguous("frames contiguous(4)+free, fragmented", |f| {
        for i in (0..4096).step_by(4) {
            let frame = RAM.start.0 + i * 4096;
            f.reserve(PhysAddr(frame)..PhysAddr(frame + 4096));
        }
    });
}
