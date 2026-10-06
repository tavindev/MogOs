use std::hint::black_box;
use std::time::Instant;

use mm::{FrameAllocator, PhysAddr};

const RUNS: usize = 51;
const FRAMES: usize = 1000;

/// Allocates then frees `FRAMES` frames from a fresh 128 MiB allocator; prints min and median ns per alloc+free.
fn main() {
    let mut samples: Vec<f64> = (0..RUNS)
        .map(|_| {
            let mut frames =
                FrameAllocator::<512>::new(PhysAddr(0x4000_0000)..PhysAddr(0x4800_0000));
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
    samples.sort_by(f64::total_cmp);
    println!(
        "frames alloc+free: min {:.1} ns/op, median {:.1} ns/op ({RUNS} runs x {FRAMES})",
        samples[0],
        samples[RUNS / 2]
    );
}
