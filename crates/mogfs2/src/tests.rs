use std::collections::{HashMap, HashSet};

use super::*;

/// Blocks a valid slot reaches through references that verify, superblocks excluded; restates the format.
pub(crate) fn reachable(disk: &[Block]) -> Vec<bool> {
    let mut reached = vec![false; disk.len()];
    for slot in 0..2 {
        if let Some(set) = slot_blocks(disk, slot) {
            for b in set {
                reached[b as usize] = true;
            }
        }
    }
    reached
}

/// The blocks slot `slot` reaches, if its superblock and bitmap verify.
fn slot_blocks(disk: &[Block], slot: u64) -> Option<HashSet<u64>> {
    let sb = &disk[slot as usize];
    let f = |i: usize| le64(sb, 8 * i);
    if le64(sb, END) != checksum(slot, &sb[..SB_LEN]) || f(0) != MAGIC || f(1) % 2 != slot {
        return None;
    }
    let blocks = f(2).min(disk.len() as u64);
    let words = blocks.div_ceil(64) as usize;
    let (ix, ix_sum) = (f(13), f(14));
    let len = 16 * pages(blocks);
    if ix >= disk.len() as u64
        || le64(&disk[ix as usize], END) != ix_sum
        || checksum(ix, &disk[ix as usize][..len]) != ix_sum
    {
        return None;
    }
    let mut set = HashSet::from([ix]);
    for p in 0..pages(blocks) {
        let (b, sum) = (
            le64(&disk[ix as usize], 16 * p),
            le64(&disk[ix as usize], 16 * p + 8),
        );
        if b == 0 {
            continue;
        }
        let len = 8 * PAGE_WORDS.min(words - p * PAGE_WORDS);
        if b >= disk.len() as u64 || checksum(b, &disk[b as usize][..len]) != sum {
            return None;
        }
        set.insert(b);
    }
    let mut stack = vec![(f(9), f(10))];
    while let Some((b, sum)) = stack.pop() {
        if b >= disk.len() as u64 || !set.insert(b) {
            continue;
        }
        let n = &disk[b as usize];
        if le64(n, END) != sum || checksum(b, &n[..END]) != sum {
            set.remove(&b);
            continue;
        }
        let c = count(n);
        if n[0] > 0 {
            for i in 0..c.min(FANOUT) {
                let e = eptr(n, i);
                stack.push((e.block, e.sum));
            }
            continue;
        }
        for i in 0..c.min(CAP / ITEM) {
            let (k, off, len) = (ikey(n, i), voff(n, i), vlen(n, i));
            if (k as u64) >> 62 != EXTENT || off + len > END || len < 16 {
                continue;
            }
            let v = &n[off..off + len];
            for j in 0..(len - 8) / 8 {
                let p = le64(v, 0).wrapping_add(j as u64);
                if p < disk.len() as u64 && checksum(p, &disk[p as usize]) == le64(v, 8 + 8 * j) {
                    set.insert(p);
                }
            }
        }
    }
    Some(set)
}

/// An in-memory disk that refuses (and records) a write to a block a valid slot reaches.
pub(crate) struct Guarded {
    pub(crate) blocks: Vec<Block>,
    reached: Vec<bool>,
    pub(crate) violation: Option<u64>,
    /// Panic on a violation instead of failing the write.
    strict: bool,
}

impl Guarded {
    pub(crate) fn new(blocks: Vec<Block>, strict: bool) -> Self {
        let reached = reachable(&blocks);
        Self {
            blocks,
            reached,
            violation: None,
            strict,
        }
    }
}

impl Disk for &mut Guarded {
    fn read(&mut self, block: u64, bufs: &mut [Block]) -> Result<(), Error> {
        let b = block as usize;
        if b + bufs.len() > self.blocks.len() {
            return Err(Error::Io);
        }
        bufs.copy_from_slice(&self.blocks[b..][..bufs.len()]);
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[Block]) -> Result<(), Error> {
        let b = block as usize;
        if b + bufs.len() > self.blocks.len() {
            return Err(Error::Io);
        }
        if let Some(i) = (b..b + bufs.len()).find(|&i| i >= 2 && self.reached[i]) {
            assert!(!self.strict, "write to block {i}, which a slot reaches");
            self.violation = Some(i as u64);
            return Err(Error::Io);
        }
        self.blocks[b..][..bufs.len()].copy_from_slice(bufs);
        if b < 2 {
            self.reached = reachable(&self.blocks);
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

pub(crate) struct Mem {
    cache: Vec<Block>,
    bits: Vec<u64>,
}

impl Mem {
    pub(crate) fn new(blocks: usize) -> Self {
        Self {
            cache: vec![[0; BLOCK_SIZE]; cache_blocks(blocks as u64, MIN_POOL)],
            bits: vec![0; bitmap_words(blocks as u64)],
        }
    }

    pub(crate) fn fs<D: Disk>(&mut self, disk: D) -> Fs<'_, D> {
        Fs::new(disk, &mut self.cache, &mut self.bits)
    }
}

/// xorshift64
pub(crate) fn next(state: &mut u64, below: u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state % below
}

/// Every item of the live tree, through the cache, and the blocks its nodes and extents hold.
fn items<D: Disk>(fs: &mut Fs<D>) -> (Vec<(u128, Vec<u8>)>, HashSet<u64>) {
    let (mut out, mut blocks) = (vec![], HashSet::new());
    let mut stack = vec![(fs.root, fs.height - 1)];
    while let Some((p, level)) = stack.pop() {
        if p.block & TAG == 0 {
            assert!(blocks.insert(p.block), "node {} reached twice", p.block);
        }
        let s = fs.node(p, level, 0, NONE).unwrap();
        let n = fs.cache[s];
        if level > 0 {
            for i in (0..count(&n)).rev() {
                stack.push((eptr(&n, i), level - 1));
            }
            continue;
        }
        for i in 0..count(&n) {
            let (k, v) = (ikey(&n, i), value(&n, i).to_vec());
            if (k as u64) >> 62 == EXTENT {
                for j in 0..(v.len() as u64 - 8) / 8 {
                    assert!(blocks.insert(le64(&v, 0) + j), "data block reached twice");
                }
            }
            out.push((k, v));
        }
    }
    (out, blocks)
}

/// The tree's items are sorted, every inode item is reached from the root exactly once by entries whose kinds match,
/// and the live bitmap holds exactly the blocks the tree, the bitmap and the superblocks reach.
fn check<D: Disk>(fs: &mut Fs<D>, ctx: &str) {
    let (items, mut blocks) = items(fs);
    assert!(items.windows(2).all(|w| w[0].0 < w[1].0), "{ctx}: unsorted");
    let inodes: HashMap<u64, u8> = items
        .iter()
        .filter(|(k, _)| (*k as u64) >> 62 == INODE)
        .map(|(k, v)| ((k >> 64) as u64, v[0]))
        .collect();
    let mut seen = HashSet::from([0]);
    for (k, v) in items.iter().filter(|(k, _)| (*k as u64) >> 62 == DIRENT) {
        let (dir, child) = ((k >> 64) as u64, le64(v, 0));
        assert_eq!(
            inodes.get(&dir),
            Some(&DIR),
            "{ctx}: entry in non-directory {dir}"
        );
        assert_eq!(
            inodes.get(&child),
            Some(&v[8]),
            "{ctx}: entry kind for {child}"
        );
        assert!(seen.insert(child), "{ctx}: inode {child} named twice");
        assert!(child < fs.next_inode, "{ctx}: inode past the counter");
    }
    assert_eq!(seen.len(), inodes.len(), "{ctx}: unnamed inode");
    blocks.extend([0, 1, fs.index.0]);
    blocks.extend(
        (0..fs.pages)
            .map(|p| le64(&fs.cache[0], 16 * p))
            .filter(|&b| b != 0),
    );
    for b in 0..fs.blocks {
        assert_eq!(fs.has(LIVE, b), blocks.contains(&b), "{ctx}: live bit {b}");
    }
    let used = (0..fs.blocks).filter(|&b| fs.used(b)).count() as u64;
    assert_eq!(fs.free, fs.blocks - used, "{ctx}: free count");
    assert!((0..fs.hint).all(|b| fs.used(b)), "{ctx}: hint");
}

/// Whether `target` is `root` or below it, by a walk independent of `Fs::below`.
fn subtree_has<D: Disk>(fs: &mut Fs<D>, root: Inode, target: Inode) -> bool {
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        if d == target {
            return true;
        }
        if fs.kind(d) == Ok(Kind::Dir) {
            fs.readdir(d, 0, |_, i, _| {
                stack.push(i);
                false
            })
            .unwrap();
        }
    }
    false
}

/// A name unique to `step` and `i`, 1 to 200 bytes long.
fn name(rng: &mut u64, step: usize, i: u64) -> Vec<u8> {
    let mut n = format!("f{step}.{i}-").into_bytes();
    n.resize(n.len() + next(rng, 190) as usize, b'x');
    n
}

/// A random entry of `dir`.
fn pick<D: Disk>(fs: &mut Fs<D>, dir: Inode, rng: &mut u64) -> Option<(Vec<u8>, Inode)> {
    let mut entries = vec![];
    fs.readdir(dir, 0, |n, i, _| {
        entries.push((n.to_vec(), i));
        false
    })
    .unwrap();
    (!entries.is_empty()).then(|| entries.swap_remove(next(rng, entries.len() as u64) as usize))
}

/// Random changes, commits and remounts with the smallest cache: no write reaches a block a slot reaches; after every
/// step the tree and the live bitmap agree, and after every commit the free space equals a fresh mount's.
#[test]
fn random_changes_keep_the_tree_and_free_space_consistent() {
    for seed in 1..=200u64 {
        let rng = &mut seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let blocks = if seed % 8 == 0 {
            6000
        } else {
            48 + next(rng, 1500) as usize
        };
        let mut disk = Guarded::new(vec![[0; BLOCK_SIZE]; blocks], true);
        let mut mem = Mem::new(blocks);
        let mut fs = mem.fs(&mut disk);
        fs.format(seed).unwrap();
        let (mut files, mut dirs) = (vec![], vec![ROOT]);
        for step in 0..300 {
            let ctx = format!("seed {seed} step {step}");
            let r = match next(rng, 16) {
                0 | 1 => {
                    let d = dirs[next(rng, dirs.len() as u64) as usize];
                    fs.create(d, &name(rng, step, 0)).map(|f| files.push(f))
                }
                13 => {
                    let d = dirs[next(rng, dirs.len() as u64) as usize];
                    let burst = if seed % 8 == 0 { 400 } else { 40 };
                    (0..20 + next(rng, burst))
                        .try_for_each(|i| fs.create(d, &name(rng, step, i)).map(|f| files.push(f)))
                }
                14 => {
                    let d = dirs[next(rng, dirs.len() as u64) as usize];
                    let burst = if seed % 8 == 0 { 600 } else { 40 };
                    (0..next(rng, burst)).try_for_each(|_| match pick(&mut fs, d, rng) {
                        Some((name, i)) if fs.kind(i) == Ok(Kind::File) => {
                            fs.unlink(d, &name).map(|()| {
                                files.retain(|&f| f != i);
                            })
                        }
                        _ => Ok(()),
                    })
                }
                2 => {
                    let d = dirs[next(rng, dirs.len() as u64) as usize];
                    fs.mkdir(d, format!("d{step}").as_bytes())
                        .map(|d| dirs.push(d))
                }
                3..=5 if !files.is_empty() => {
                    let f = files[next(rng, files.len() as u64) as usize];
                    let at = next(rng, 40 * BLOCK_SIZE as u64);
                    let len = 1 + next(rng, 3 * BLOCK_SIZE as u64);
                    fs.write(f, at, &vec![step as u8; len as usize])
                }
                6 if !files.is_empty() => {
                    let f = files[next(rng, files.len() as u64) as usize];
                    fs.truncate(f)
                }
                7 if next(rng, 3) == 0 => {
                    fs.commit().unwrap();
                    let mut d = Guarded::new(fs.disk.blocks.clone(), true);
                    let mut m = Mem::new(blocks);
                    let mut fresh = m.fs(&mut d);
                    fresh.mount().unwrap();
                    assert_eq!(fresh.bits, fs.bits, "{ctx}: bitmaps");
                    assert_eq!(fresh.free, fs.free, "{ctx}: free");
                    check(&mut fresh, &ctx);
                    Ok(())
                }
                9 => fs.mount().map(|()| {
                    files.retain(|&f| fs.kind(f) == Ok(Kind::File));
                    dirs.retain(|&d| fs.kind(d) == Ok(Kind::Dir));
                }),
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
                12 if !files.is_empty() => {
                    let f = files[next(rng, files.len() as u64) as usize];
                    let size = fs.stat(f).unwrap().size;
                    let mut buf = vec![0; size as usize];
                    fs.read(f, 0, &mut buf).map(|_| ())
                }
                _ => Ok(()),
            };
            assert!(
                matches!(r, Ok(()) | Err(Error::NoSpace | Error::NotEmpty)),
                "{ctx}: {r:?}"
            );
            check(&mut fs, &ctx);
        }
    }
}
