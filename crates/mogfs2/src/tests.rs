use std::collections::{HashMap, HashSet};

use super::*;

/// Blocks a valid slot reaches through references that verify, superblocks excluded; restates the format.
pub(crate) fn reachable(disk: &[Block]) -> Vec<bool> {
    let mut reached = vec![false; disk.len()];
    for slot in 0..2 {
        for b in slot_blocks(disk, slot).into_iter().flatten() {
            reached[b as usize] = true;
        }
    }
    reached
}

/// Slot `slot`'s superblock fields, if they keep the format's rules.
fn superblock_fields(disk: &[Block], slot: u64) -> Option<[u64; 15]> {
    let sb = &disk[slot as usize];
    let f: [u64; 15] = std::array::from_fn(|i| le64(sb, 8 * i));
    let blocks = f[2];
    let ok = le64(sb, END) == checksum(slot, &sb[..SB_LEN])
        && f[0] == MAGIC
        && f[1] % 2 == slot
        && (MIN_BLOCKS..=disk.len() as u64).contains(&blocks)
        && f[4] >= 1
        && (f[6], f[7], f[8]) == (0, 0, 1)
        && f[12] < MAX_HEIGHT as u64
        && f[11] <= f[1]
        && (2..blocks).contains(&f[9])
        && (2..blocks).contains(&f[13])
        && sb[SB_LEN..END].iter().all(|&b| b == 0);
    ok.then_some(f)
}

/// The bitmap index and page blocks of a slot, if its bitmap verifies, is well formed and marks them and the root.
fn bitmap_blocks(disk: &[Block], f: &[u64; 15]) -> Option<Vec<u64>> {
    let (blocks, ix) = (f[2], f[13]);
    let (pages, words) = (pages(blocks), blocks.div_ceil(64) as usize);
    let index = &disk[ix as usize];
    let len = 16 * pages;
    if le64(index, END) != f[14]
        || checksum(ix, &index[..len]) != f[14]
        || index[len..END].iter().any(|&b| b != 0)
    {
        return None;
    }
    let mut bits = vec![0u64; pages * PAGE_WORDS];
    let mut held = vec![ix];
    for p in 0..pages {
        let (b, sum) = (le64(index, 16 * p), le64(index, 16 * p + 8));
        if (b, sum) == (0, 0) {
            continue;
        }
        let n = PAGE_WORDS.min(words - p * PAGE_WORDS);
        if !(2..blocks).contains(&b)
            || checksum(b, &disk[b as usize][..8 * n]) != sum
            || disk[b as usize][8 * n..].iter().any(|&b| b != 0)
        {
            return None;
        }
        for i in 0..n {
            bits[p * PAGE_WORDS + i] = le64(&disk[b as usize], 8 * i);
        }
        held.push(b);
    }
    let has = |b: u64| bits[(b / 64) as usize] >> (b % 64) & 1 != 0;
    let tail = (blocks..(words as u64 * 64)).any(has);
    (!tail && [0, 1, f[9]].iter().chain(&held).all(|&b| has(b))).then_some(held)
}

/// Each node a root reaches through pointers that verify, with its parent node and entry (`None` for the root).
fn tree_nodes(disk: &[Block], root: u64, sum: u64) -> Vec<(u64, Option<(u64, usize)>)> {
    let (mut out, mut seen) = (vec![], HashSet::new());
    let mut stack = vec![(root, sum, None)];
    while let Some((b, sum, parent)) = stack.pop() {
        if b >= disk.len() as u64 || !seen.insert(b) {
            continue;
        }
        let n = &disk[b as usize];
        if le64(n, END) != sum || checksum(b, &n[..END]) != sum {
            continue;
        }
        out.push((b, parent));
        // A node whose count breaks the format holds no entries.
        if n[0] > 0 && count(n) <= FANOUT {
            for i in 0..count(n) {
                let e = eptr(n, i);
                stack.push((e.block, e.sum, Some((b, i))));
            }
        }
    }
    out
}

/// The extents (first block, pages, sums offset) a leaf's items hold; none if its items break the format's layout
/// (bytes past the count, as a removed item leaves, are not items).
fn extents(n: &Block) -> Vec<(u64, usize, usize)> {
    let (mut out, c, mut top) = (vec![], count(n), END);
    if n[0] != 0 || c * ITEM > CAP {
        return out;
    }
    for i in 0..c {
        let (k, off, len) = (ikey(n, i), voff(n, i), vlen(n, i));
        if off + len != top || off < HDR + ITEM * c {
            return vec![];
        }
        top = off;
        if (k as u64) >> 62 == EXTENT && len >= 16 {
            out.push((le64(n, off), (len - 8) / 8, off + 8));
        }
    }
    out
}

/// The blocks slot `slot` reaches, if it is valid: its bitmap, nodes and data pages that verify.
fn slot_blocks(disk: &[Block], slot: u64) -> Option<HashSet<u64>> {
    let f = superblock_fields(disk, slot)?;
    let mut set: HashSet<u64> = bitmap_blocks(disk, &f)?.into_iter().collect();
    for (b, _) in tree_nodes(disk, f[9], f[10]) {
        set.insert(b);
        let n = &disk[b as usize];
        for (start, pages, at) in extents(n) {
            for j in 0..pages {
                let p = start.wrapping_add(j as u64);
                if p < disk.len() as u64 && checksum(p, &disk[p as usize]) == le64(n, at + 8 * j) {
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

/// An image both slots reach parts of: a root of 150 long-named files (a tree of height 2), a directory with files, a
/// 20-page file and a sparse one; the second commit overwrites a page, unlinks and renames.
fn mutation_base() -> Vec<Block> {
    let blocks = 700;
    let mut disk = Guarded::new(vec![[0; BLOCK_SIZE]; blocks], true);
    let mut mem = Mem::new(blocks);
    let mut fs = mem.fs(&mut disk);
    fs.format(7).unwrap();
    let sub = fs.mkdir(ROOT, b"sub").unwrap();
    for i in 0..150 {
        let f = fs.create(ROOT, format!("{i:0>40}").as_bytes()).unwrap();
        fs.write(f, 0, &[i as u8; 100]).unwrap();
    }
    for i in 0..20 {
        fs.create(sub, format!("s{i}").as_bytes()).unwrap();
    }
    let big = fs.create(sub, b"big").unwrap();
    fs.write(big, 0, &[9; 20 * BLOCK_SIZE]).unwrap();
    let sparse = fs.create(ROOT, b"sparse").unwrap();
    fs.write(sparse, 50 * BLOCK_SIZE as u64, b"tail").unwrap();
    fs.commit().unwrap();
    fs.write(big, 5 * BLOCK_SIZE as u64, &[1; 10]).unwrap();
    for i in 0..10 {
        fs.unlink(ROOT, format!("{i:0>40}").as_bytes()).unwrap();
    }
    fs.rename(ROOT, b"sparse", sub, b"moved").unwrap();
    fs.commit().unwrap();
    assert!(fs.height() >= 2);
    disk.blocks
}

fn reseal_superblock(disk: &mut [Block], slot: usize) {
    let sum = checksum(slot as u64, &disk[slot][..SB_LEN]);
    disk[slot][END..].copy_from_slice(&sum.to_le_bytes());
}

/// Reseals node `b` and each parent up to slot `slot`'s superblock.
fn reseal_up(
    disk: &mut [Block],
    slot: usize,
    parents: &HashMap<u64, Option<(u64, usize)>>,
    mut b: u64,
) {
    loop {
        let sum = seal(b, &mut disk[b as usize], END);
        match parents[&b] {
            None => {
                disk[slot][80..88].copy_from_slice(&sum.to_le_bytes());
                return reseal_superblock(disk, slot);
            }
            Some((p, i)) => {
                disk[p as usize][HDR + ENTRY * i + 24..][..8].copy_from_slice(&sum.to_le_bytes());
                b = p;
            }
        }
    }
}

/// Reseals the bitmap index `ix` of slot `slot` and its superblock.
fn reseal_index(disk: &mut [Block], slot: usize, ix: u64, blocks: u64) {
    let sum = seal(ix, &mut disk[ix as usize], 16 * pages(blocks));
    disk[slot][112..120].copy_from_slice(&sum.to_le_bytes());
    reseal_superblock(disk, slot);
}

/// `old`, a field of `width` bytes, changed one of several ways, some aimed at block and inode numbers.
fn mutated(old: u128, width: usize, rng: &mut u64) -> u128 {
    let mask = if width == 16 {
        u128::MAX
    } else {
        (1 << (8 * width)) - 1
    };
    let v = match next(rng, 7) {
        0 => old ^ 1 << next(rng, 8 * width as u64),
        1 => old.wrapping_add(1),
        2 => old.wrapping_sub(1),
        3 => 0,
        4 => u128::MAX,
        5 => next(rng, 1500) as u128,
        _ => (next(rng, u64::MAX) as u128) << 64 | next(rng, u64::MAX) as u128,
    };
    v & mask
}

fn read(disk: &[Block], b: u64, at: usize, width: usize) -> u128 {
    let mut v = [0; 16];
    v[..width].copy_from_slice(&disk[b as usize][at..at + width]);
    u128::from_le_bytes(v)
}

fn write(disk: &mut [Block], b: u64, at: usize, width: usize, rng: &mut u64) {
    let v = mutated(read(disk, b, at, width), width, rng);
    disk[b as usize][at..at + width].copy_from_slice(&v.to_le_bytes()[..width]);
}

/// Changes one decoded field of a slot's superblock, bitmap index, bitmap page or tree node, resealing every sum
/// above it. Returns false, leaving the change out, for an extent moved onto blocks a slot reaches: catching that
/// takes a walk of every root (step 41's scrub), not a check when decoded.
fn mutate(disk: &mut [Block], rng: &mut u64) -> bool {
    let slot = next(rng, 2) as usize;
    // A slot an earlier change already made invalid takes no more.
    let Some(f) = superblock_fields(disk, slot as u64).filter(|f| bitmap_blocks(disk, f).is_some())
    else {
        return true;
    };
    let ix = f[13];
    match next(rng, 8) {
        0 => {
            write(disk, slot as u64, 8 * next(rng, 15) as usize, 8, rng);
            reseal_superblock(disk, slot);
            true
        }
        1 => {
            write(
                disk,
                ix,
                8 * next(rng, 2 * pages(f[2]) as u64) as usize,
                8,
                rng,
            );
            reseal_index(disk, slot, ix, f[2]);
            true
        }
        2 => {
            // Only marking a block used: a bitmap that frees a reachable block is also a scrub finding.
            let page = le64(&disk[ix as usize], 0) as usize;
            let bit = next(rng, f[2]);
            disk[page][(bit / 8) as usize] |= 1 << (bit % 8);
            let sum = checksum(page as u64, &disk[page][..8 * f[2].div_ceil(64) as usize]);
            disk[ix as usize][8..16].copy_from_slice(&sum.to_le_bytes());
            reseal_index(disk, slot, ix, f[2]);
            true
        }
        _ => {
            let reached = reachable(disk);
            let nodes = tree_nodes(disk, f[9], f[10]);
            if nodes.is_empty() {
                return true;
            }
            let parents: HashMap<_, _> = nodes.iter().copied().collect();
            let b = nodes[next(rng, nodes.len() as u64) as usize].0;
            let n = &disk[b as usize];
            let c = count(n);
            // An earlier change may have broken this node's count or offsets; keep the regions inside it.
            let stride = if n[0] == 0 { ITEM } else { ENTRY };
            let c = c.min(CAP / stride);
            let values = if n[0] == 0 && c > 0 {
                voff(n, c - 1).min(END)
            } else {
                END
            };
            let width = [1, 2, 8, 16][next(rng, 4) as usize];
            let (lo, hi) =
                [(0, HDR), (HDR, HDR + stride * c), (values, END)][next(rng, 3) as usize];
            if hi < lo + width {
                return false;
            }
            let before = extents(n);
            let at = lo + next(rng, (hi - lo - width + 1) as u64) as usize;
            if next(rng, 3) == 0 && lo == HDR && c > 1 {
                // Another item's or entry's same field, give or take one: a neighbour in a hash chain, a block or
                // an inode another pointer already names.
                let other = HDR + (at - HDR) % stride + stride * next(rng, c as u64) as usize;
                let v = read(disk, b, other, width)
                    .wrapping_add(next(rng, 3) as u128)
                    .wrapping_sub(1);
                disk[b as usize][at..at + width].copy_from_slice(&v.to_le_bytes()[..width]);
            } else {
                write(disk, b, at, width, rng);
            }
            reseal_up(disk, slot, &parents, b);
            !extents(&disk[b as usize])
                .into_iter()
                .any(|(start, pages, at)| {
                    !before
                        .iter()
                        .any(|&(s, p, a)| (s, p, a) == (start, pages, at))
                        && (0..pages as u64)
                            .any(|j| reached.get(start.wrapping_add(j) as usize) == Some(&true))
                })
        }
    }
}

/// Reads, lists, stats and maps everything reachable, then changes and commits a lot, remounts and does it again.
/// Results are ignored: only panics, an inode `lookup` reaches twice from the root, and writes to reachable blocks fail
/// the test.
fn exercise<D: Disk>(fs: &mut Fs<D>) {
    for round in 0..2 {
        let (mut dirs, mut files, mut names) = (vec![ROOT], vec![], vec![]);
        let mut seen = HashSet::from([ROOT.0]);
        let mut i = 0;
        while i < dirs.len() && i < 50 {
            let d = dirs[i];
            i += 1;
            let mut entries = vec![];
            let _ = fs.readdir(d, 0, |n, inode, _| {
                entries.push((n.to_vec(), inode));
                entries.len() > 400
            });
            for (n, inode) in entries {
                let _ = fs.stat(inode);
                // A changed entry may repeat a name; looking it up twice is not a second way in.
                if names.contains(&(d, n.clone())) {
                    continue;
                }
                let found = fs.lookup(d, &n).and_then(|x| Ok((x, fs.kind(x)?)));
                if let Ok((x, _)) = found {
                    assert!(seen.insert(x.0), "{x:?} reached twice from the root");
                }
                match found {
                    Ok((x, Kind::Dir)) => dirs.push(x),
                    Ok((x, Kind::File)) => files.push(x),
                    Err(_) => {}
                }
                names.push((d, n));
            }
        }
        // Names the image was built with, whose hash chains a changed entry may sit in.
        for i in 0..150 {
            let _ = fs.lookup(ROOT, format!("{i:0>40}").as_bytes());
        }
        let mut buf = vec![0; 30 * BLOCK_SIZE];
        for &f in files.iter().take(30) {
            let _ = fs.read(f, 0, &mut buf);
            for p in [0, 1, 5, 50] {
                let _ = fs.map(f, p);
            }
        }
        for (j, &f) in files.iter().take(10).enumerate() {
            let _ = fs.write(f, (j * 3000) as u64, &[j as u8; 5000]);
        }
        if let Some(&f) = files.get(10) {
            let _ = fs.truncate(f);
        }
        for (d, n) in names.iter().take(15) {
            let _ = fs.unlink(*d, n);
        }
        for (k, (d, n)) in names.iter().skip(15).take(10).enumerate() {
            let _ = fs.rename(*d, n, dirs[k % dirs.len()], format!("moved{k}").as_bytes());
        }
        for (k, &d) in dirs.iter().take(4).enumerate() {
            // Enough to write dirty nodes out before the commit.
            for i in 0..80 {
                let _ = fs.create(d, format!("{round}{k}-{i:0>100}").as_bytes());
            }
            let _ = fs.mkdir(d, format!("new{round}").as_bytes());
        }
        let _ = fs.commit();
        let _ = fs.mount();
    }
}

/// Names the seed of a mutation whose run panics.
struct Seed(u64);

impl Drop for Seed {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("mutation seed {}", self.0);
        }
    }
}

/// Decoded fields changed and resealed up to the superblock, then mount and every operation: no panic, no write to a
/// block a valid slot reaches, only `Ok` or a named error.
#[test]
fn mutated_images_never_panic_or_write_reachable_blocks() {
    let base = mutation_base();
    let (mut run, mut mounted, mut left_out) = (0, 0, 0);
    // `MUTATION_SEEDS=20000` for a deeper run.
    let seeds = std::env::var("MUTATION_SEEDS").map_or(1500u64, |s| s.parse().unwrap());
    for seed in 1..=seeds {
        let rng = &mut seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let mut image = base.clone();
        // One to three fields, so that changes that only matter together (a slot's size and its root) meet.
        if !(0..1 + next(rng, 3)).all(|_| mutate(&mut image, rng)) {
            left_out += 1;
            continue;
        }
        let _seed = Seed(seed);
        let mut disk = Guarded::new(image, false);
        let mut mem = Mem::new(base.len());
        let mut fs = mem.fs(&mut disk);
        if fs.mount().is_ok() {
            mounted += 1;
            exercise(&mut fs);
        }
        run += 1;
        assert_eq!(disk.violation, None, "seed {seed}");
    }
    assert!(
        left_out * 4 < run && mounted * 2 > run,
        "{run} run, {mounted} mounted, {left_out} left out"
    );
}
