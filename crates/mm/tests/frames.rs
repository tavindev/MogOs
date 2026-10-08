use mm::{FrameAllocator, PhysAddr};

const BASE: u64 = 0x4000_0000;

fn frame(i: u64) -> PhysAddr {
    PhysAddr(BASE + i * 4096)
}

#[test]
fn allocates_every_frame_then_exhausts() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(3));
    assert_eq!(frames.free_count(), 3);
    assert_eq!(frames.alloc(), Some(frame(0)));
    assert_eq!(frames.alloc(), Some(frame(1)));
    assert_eq!(frames.alloc(), Some(frame(2)));
    assert_eq!(frames.alloc(), None);
    assert_eq!(frames.free_count(), 0);
}

#[test]
fn freed_frame_is_reused() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(2));
    let a = frames.alloc().unwrap();
    frames.alloc().unwrap();
    frames.free(a);
    assert_eq!(frames.alloc(), Some(a));
}

#[test]
#[should_panic]
fn double_free_panics() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(2));
    let a = frames.alloc().unwrap();
    frames.free(a);
    frames.free(a);
}

#[test]
fn reserve_covers_partial_frames_and_clips() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(8));
    frames.reserve(PhysAddr(BASE + 4095)..PhysAddr(BASE + 4097));
    frames.reserve(PhysAddr(BASE - 0x10000)..frame(1));
    frames.reserve(frame(7)..frame(100));
    assert_eq!(frames.free_count(), 5);
    assert_eq!(frames.alloc(), Some(frame(2)));
}

#[test]
fn ram_is_trimmed_to_whole_frames_and_capacity() {
    let frames = FrameAllocator::<1>::new(PhysAddr(BASE + 1)..PhysAddr(BASE + 3 * 4096 - 1));
    assert_eq!(frames.free_count(), 1);
    let frames = FrameAllocator::<1>::new(frame(0)..frame(1000));
    assert_eq!(frames.free_count(), 64);
}

#[test]
fn contiguous_alloc_skips_used_frames() {
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(8));
    frames.reserve(frame(2)..frame(3));
    assert_eq!(frames.alloc_contiguous(3), Some(frame(3)..frame(6)));
    assert_eq!(frames.alloc_contiguous(3), None);
    assert_eq!(frames.alloc_contiguous(2), Some(frame(0)..frame(2)));
    assert_eq!(frames.free_count(), 2);
}

#[test]
fn contiguous_alloc_straddles_words() {
    let mut frames = FrameAllocator::<3>::new(frame(0)..frame(192));
    frames.reserve(frame(0)..frame(62));
    assert_eq!(frames.alloc_contiguous(4), Some(frame(62)..frame(66)));
    frames.reserve(frame(66)..frame(127));
    assert_eq!(frames.alloc_contiguous(65), Some(frame(127)..frame(192)));
    assert_eq!(frames.free_count(), 0);
}

#[test]
fn contiguous_alloc_spans_a_whole_word() {
    let mut frames = FrameAllocator::<4>::new(frame(0)..frame(256));
    frames.reserve(frame(0)..frame(60));
    frames.reserve(frame(190)..frame(191));
    assert_eq!(frames.alloc_contiguous(130), Some(frame(60)..frame(190)));
}

#[test]
fn contiguous_alloc_takes_the_run_at_the_very_end() {
    let mut frames = FrameAllocator::<2>::new(frame(0)..frame(70));
    frames.reserve(frame(0)..frame(65));
    assert_eq!(frames.alloc_contiguous(6), None);
    assert_eq!(frames.alloc_contiguous(5), Some(frame(65)..frame(70)));
    assert_eq!(frames.alloc_contiguous(1), None);
    let mut frames = FrameAllocator::<1>::new(frame(0)..frame(64));
    frames.reserve(frame(0)..frame(1));
    assert_eq!(frames.alloc_contiguous(63), Some(frame(1)..frame(64)));
}

#[test]
fn contiguous_alloc_exhausts_and_reuses_freed_frames() {
    let mut frames = FrameAllocator::<2>::new(frame(0)..frame(128));
    assert_eq!(frames.alloc_contiguous(129), None);
    assert_eq!(frames.alloc_contiguous(128), Some(frame(0)..frame(128)));
    assert_eq!(frames.alloc_contiguous(1), None);
    frames.free(frame(64));
    assert_eq!(frames.alloc_contiguous(2), None);
    assert_eq!(frames.alloc_contiguous(1), Some(frame(64)..frame(65)));
    assert_eq!(FrameAllocator::<2>::empty().alloc_contiguous(1), None);
}

#[test]
fn contiguous_alloc_skips_fragmented_runs() {
    let mut frames = FrameAllocator::<4>::new(frame(0)..frame(256));
    for i in (0..200).step_by(4) {
        frames.reserve(frame(i)..frame(i + 1));
    }
    assert_eq!(frames.alloc_contiguous(3), Some(frame(1)..frame(4)));
    assert_eq!(frames.alloc_contiguous(4), Some(frame(197)..frame(201)));
}

/// Drives the allocator and a bit-per-frame model with the same random operations and compares every result. Long
/// reservations from frame 0 (the boot image) and frees of whole low runs move the first-fit hint both ways.
#[test]
fn matches_a_bit_by_bit_model() {
    for seed in 1..=2000u64 {
        let mut rng = seed;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let total = 1 + next(256);
        let mut frames = FrameAllocator::<4>::new(frame(0)..frame(total));
        let mut used = vec![false; total as usize];
        for _ in 0..200 {
            match next(4) {
                0 => {
                    let max = if next(4) == 0 { 140 } else { 8 };
                    let count = 1 + next(max) as usize;
                    let expect = used
                        .windows(count)
                        .position(|w| w.iter().all(|u| !u))
                        .map(|s| {
                            used[s..s + count].fill(true);
                            frame(s as u64)..frame((s + count) as u64)
                        });
                    assert_eq!(frames.alloc_contiguous(count), expect, "seed {seed}");
                }
                1 if next(2) == 0 => {
                    let expect = used.iter().position(|u| !u).map(|s| {
                        used[s] = true;
                        frame(s as u64)
                    });
                    assert_eq!(frames.alloc(), expect, "seed {seed}");
                }
                1 => {
                    let mut out = vec![PhysAddr(0); 1 + next(8) as usize];
                    let free: Vec<_> = (0..total as usize).filter(|&i| !used[i]).collect();
                    let fits = free.len() >= out.len();
                    assert_eq!(frames.alloc_many(&mut out), fits, "seed {seed}");
                    if fits {
                        let expect: Vec<_> =
                            free[..out.len()].iter().map(|&i| frame(i as u64)).collect();
                        assert_eq!(out.to_vec(), expect, "seed {seed}");
                        free[..out.len()].iter().for_each(|&i| used[i] = true);
                    }
                }
                2 => {
                    let start = if next(4) == 0 { 0 } else { next(total) };
                    let end = start + if next(4) == 0 { next(total) } else { next(8) };
                    used[start as usize..(end.min(total)) as usize].fill(true);
                    frames.reserve(frame(start)..frame(end));
                }
                _ => {
                    let low = next(4) == 0;
                    for _ in 0..next(16) {
                        let i = if low {
                            next(total.min(70))
                        } else {
                            next(total)
                        } as usize;
                        if used[i] {
                            used[i] = false;
                            frames.free(frame(i as u64));
                        }
                    }
                }
            }
            assert_eq!(
                frames.free_count(),
                used.iter().filter(|u| !**u).count(),
                "seed {seed}"
            );
        }
    }
}

#[test]
fn alloc_many_takes_all_or_none() {
    let mut frames = FrameAllocator::<2>::new(frame(0)..frame(70));
    frames.reserve(frame(1)..frame(2));
    let mut out = [PhysAddr(0); 3];
    assert!(frames.alloc_many(&mut out));
    assert_eq!(out, [frame(0), frame(2), frame(3)]);
    let mut big = [PhysAddr(0); 67];
    assert!(!frames.alloc_many(&mut big));
    assert_eq!(frames.free_count(), 66);
    let mut rest = [PhysAddr(0); 66];
    assert!(frames.alloc_many(&mut rest));
    assert_eq!((rest[0], rest[65]), (frame(4), frame(69)));
    assert_eq!(frames.free_count(), 0);
}
