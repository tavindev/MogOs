use super::*;

/// An in-memory disk that panics on a write to a block either on-disk slot reaches.
struct Guarded {
    blocks: Vec<[u8; BLOCK_SIZE]>,
    /// Rebuilt when a superblock is written; nothing else changes what the slots reach.
    reached: Vec<bool>,
}

impl Guarded {
    fn new(blocks: Vec<[u8; BLOCK_SIZE]>) -> Self {
        let mut disk = Self {
            reached: vec![false; blocks.len()],
            blocks,
        };
        disk.rebuild();
        disk
    }

    fn rebuild(&mut self) {
        self.reached.fill(false);
        for slot in 0..2 {
            let sb = &self.blocks[slot];
            if le64(sb, PAYLOAD) != checksum(slot as u32, sb) || le64(sb, 0) != MAGIC {
                continue;
            }
            for t in (0..TABLE_BLOCKS)
                .map(|i| le32(sb, 20 + 4 * i))
                .filter(|&t| t != 0)
            {
                self.reached[t as usize] = true;
                let table = &self.blocks[t as usize];
                for r in table[..PER_TABLE * RECORD]
                    .chunks(RECORD)
                    .filter(|r| r[0] != FREE)
                {
                    for p in (0..PTRS).map(|p| le32(r, 8 + 4 * p)).filter(|&p| p != 0) {
                        self.reached[p as usize] = true;
                    }
                }
            }
        }
    }
}

impl Disk for Guarded {
    fn clamp(index: usize, len: usize) -> usize {
        index.min(len - 1)
    }

    fn read(&mut self, block: u64, bufs: &mut [[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        bufs.copy_from_slice(&self.blocks[block as usize..][..bufs.len()]);
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[[u8; BLOCK_SIZE]]) -> Result<(), Error> {
        let b = block as usize;
        assert!(!self.reached[b], "write to block {b}, which a slot reaches");
        self.blocks[b..][..bufs.len()].copy_from_slice(bufs);
        if b < 2 {
            self.rebuild();
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.blocks.len() as u64
    }
}

/// xorshift64
fn next(state: &mut u64, below: u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state % below
}

/// A random entry of `dir`.
fn pick(fs: &mut Fs<Guarded>, dir: Inode, rng: &mut u64) -> Option<(Vec<u8>, Inode)> {
    let mut entries = vec![];
    fs.readdir(dir, 0, |n, i, _| {
        entries.push((n.to_vec(), i));
        false
    })
    .unwrap();
    (!entries.is_empty()).then(|| entries.swap_remove(next(rng, entries.len() as u64) as usize))
}

/// Every in-use inode is reached exactly once from the root, and no free inode is reached.
fn check_tree(fs: &mut Fs<Guarded>, ctx: &str) {
    let mut seen = vec![false; MAX_INODES as usize];
    let mut stack = vec![ROOT];
    seen[0] = true;
    while let Some(d) = stack.pop() {
        let mut kids = vec![];
        fs.readdir(d, 0, |_, i, _| {
            kids.push(i);
            false
        })
        .unwrap();
        for i in kids {
            let kind = fs.records[i.0 as usize].kind;
            assert_ne!(kind, FREE, "{ctx}: an entry names free inode {}", i.0);
            assert!(!seen[i.0 as usize], "{ctx}: inode {} reached twice", i.0);
            seen[i.0 as usize] = true;
            if kind == DIR {
                stack.push(i);
            }
        }
    }
    for (i, r) in fs.records.iter().enumerate() {
        assert_eq!(r.kind != FREE, seen[i], "{ctx}: inode {i} unreached");
    }
}

/// Whether `target` is `root` or below it, by a walk independent of `Fs::below`.
fn subtree_has(fs: &mut Fs<Guarded>, root: Inode, target: Inode) -> bool {
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        if d == target {
            return true;
        }
        if fs.records[d.0 as usize].kind == DIR {
            fs.readdir(d, 0, |_, i, _| {
                stack.push(i);
                false
            })
            .unwrap();
        }
    }
    false
}

/// Random changes and commits: no write reaches a block either slot reaches, and after every commit the free space
/// derived incrementally equals a fresh mount's, and the directories form a tree in memory and after a remount.
#[test]
fn incremental_free_space_matches_mount_and_never_writes_reachable() {
    for seed in 1..=200u64 {
        let rng = &mut seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let blocks = 20 + next(rng, 101) as usize;
        let mut fs = Box::new(Fs::new(Guarded::new(vec![[0; BLOCK_SIZE]; blocks])));
        fs.format().unwrap();
        let (mut files, mut dirs) = (vec![], vec![ROOT]);
        for step in 0..400 {
            let ctx = format!("seed {seed} step {step}");
            let r = match next(rng, 12) {
                0 | 1 => {
                    let d = dirs[next(rng, dirs.len() as u64) as usize];
                    fs.create(d, format!("f{step}").as_bytes())
                        .map(|f| files.push(f))
                }
                2 => {
                    let d = dirs[next(rng, dirs.len() as u64) as usize];
                    fs.mkdir(d, format!("d{step}").as_bytes())
                        .map(|d| dirs.push(d))
                }
                3..=5 if !files.is_empty() => {
                    let f = files[next(rng, files.len() as u64) as usize];
                    let at = next(rng, MAX_FILE_SIZE);
                    let len = min(1 + next(rng, 3 * PAYLOAD as u64), MAX_FILE_SIZE - at);
                    fs.write(f, at, &vec![step as u8; len as usize])
                }
                6 if !files.is_empty() => {
                    let f = files[next(rng, files.len() as u64) as usize];
                    fs.truncate(f)
                }
                7 | 8 => {
                    fs.commit().unwrap();
                    let mut m = Box::new(Fs::new(Guarded::new(fs.disk.blocks.clone())));
                    m.mount().unwrap();
                    assert_eq!(m.newest, fs.newest, "{ctx}: newest");
                    assert_eq!(m.committed, fs.committed, "{ctx}: committed");
                    assert_eq!(m.used, fs.used, "{ctx}: used");
                    assert_eq!(m.free, fs.free, "{ctx}: free");
                    check_tree(&mut m, &ctx);
                    Ok(())
                }
                10 => {
                    let d = dirs[next(rng, dirs.len() as u64) as usize];
                    match pick(&mut fs, d, rng) {
                        Some((name, i)) => fs.unlink(d, &name).map(|()| {
                            files.retain(|&f| f != i);
                            dirs.retain(|&d| d != i);
                        }),
                        None => Ok(()),
                    }
                }
                11 => {
                    let from = dirs[next(rng, dirs.len() as u64) as usize];
                    let to = dirs[next(rng, dirs.len() as u64) as usize];
                    match pick(&mut fs, from, rng) {
                        Some((name, i)) => {
                            match fs.rename(from, &name, to, format!("r{step}").as_bytes()) {
                                Err(Error::InvalidName) => {
                                    assert!(
                                        subtree_has(&mut fs, i, to),
                                        "{ctx}: refused a legal move"
                                    );
                                    Ok(())
                                }
                                r => r,
                            }
                        }
                        None => Ok(()),
                    }
                }
                9 => fs.mount().map(|()| {
                    (files, dirs) = (vec![], vec![]);
                    for (i, r) in fs.records.iter().enumerate() {
                        match r.kind {
                            FILE => files.push(Inode(i as u32)),
                            DIR => dirs.push(Inode(i as u32)),
                            _ => {}
                        }
                    }
                }),
                _ => Ok(()),
            };
            assert!(
                matches!(
                    r,
                    Ok(()) | Err(Error::NoSpace | Error::TooBig | Error::NotEmpty)
                ),
                "{ctx}: {r:?}"
            );
            let used: u32 = fs.used.iter().map(|w| w.count_ones()).sum();
            assert_eq!(fs.free, fs.blocks - used, "{ctx}: free count");
            assert!(fs.used[..fs.hint].iter().all(|&w| w == !0), "{ctx}: hint");
            let inodes = &fs.records[..fs.inode_hint];
            assert!(inodes.iter().all(|r| r.kind != FREE), "{ctx}: inode hint");
            check_tree(&mut fs, &ctx);
        }
    }
}
