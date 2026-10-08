use std::collections::{HashMap, HashSet};

use super::*;

/// Blocks a valid slot reaches through references that verify, superblocks excluded; restates the format.
pub(crate) fn reachable(disk: &[Buf]) -> Vec<bool> {
    let mut reached = vec![false; disk.len()];
    for slot in 0..2 {
        for b in slot_blocks(disk, slot).into_iter().flatten() {
            reached[b as usize] = true;
        }
    }
    reached
}

/// Slot `slot`'s superblock fields, if they keep the format's rules.
fn superblock_fields(disk: &[Buf], slot: u64) -> Option<[u64; 18]> {
    let sb = &disk[slot as usize];
    let f: [u64; 18] = std::array::from_fn(|i| le64(sb, 8 * i));
    let blocks = f[2];
    let lp = 2 * pages(blocks.min(MAX_BLOCKS));
    let h = ix_height(lp);
    let list = if h == 0 { lp } else { 1 };
    let end = SB_HDR as u128 + 16 * (list as u128 + f[13] as u128);
    let ok = le64(sb, END) == checksum(Block(slot), &sb[..END]).0
        && f[0] == MAGIC
        && f[1] % 2 == slot
        && (MIN_BLOCKS..=disk.len() as u64).contains(&blocks)
        && f[4] >= 1
        && (f[6], f[7], f[8]) == (0, 0, 1)
        && f[12] < MAX_HEIGHT as u64
        && f[11] <= f[1]
        && (2..blocks).contains(&f[9])
        && f[14] == h as u64
        && f[1] <= OFFSET
        && f[15] < f[1]
        && (h == 0 || (2..blocks).contains(&f[16]))
        && end <= END as u128
        && sb[end as usize..END].iter().all(|&b| b == 0);
    ok.then_some(f)
}

/// Where a pointer sits: `None` for the superblock's list (the index root, or page `p` inline), else a block and
/// offset.
type At = Option<(u64, usize)>;

/// Each page's (block, sum) with where it sits, and each index block with where its pointer sits and its entry bytes,
/// if every index block verifies.
#[allow(clippy::type_complexity)]
fn page_list(
    disk: &[Buf],
    f: &[u64; 18],
) -> Option<(Vec<((u64, u64), At)>, Vec<(u64, At, usize)>)> {
    let (pages, h) = (2 * pages(f[2]), f[14] as usize);
    let sb = &disk[(f[1] % 2) as usize];
    if h == 0 {
        let list = (0..pages).map(|p| {
            (
                (le64(sb, SB_HDR + 16 * p), le64(sb, SB_HDR + 16 * p + 8)),
                None,
            )
        });
        return Some((list.collect(), vec![]));
    }
    let (mut level, mut ix): (Vec<((u64, u64), At)>, _) = (vec![((f[16], f[17]), None)], vec![]);
    for l in (1..=h).rev() {
        let mut next = vec![];
        for (i, &((b, sum), at)) in level.iter().enumerate() {
            let len = 16 * ix_children(pages, l, i);
            if (b, sum) == (0, 0) {
                next.extend((0..len / 16).map(|_| ((0, 0), Some((0, 0)))));
                continue;
            }
            let n = disk.get(b as usize).filter(|_| (2..f[2]).contains(&b))?;
            if le64(n, END) != sum
                || checksum(Block(b), &n[..len]).0 != sum
                || n[len..END].iter().any(|&x| x != 0)
            {
                return None;
            }
            ix.push((b, at, len));
            next.extend(
                (0..len / 16).map(|j| ((le64(n, 16 * j), le64(n, 16 * j + 8)), Some((b, 16 * j)))),
            );
        }
        level = next;
    }
    Some((level, ix))
}

/// The bitmap page and index blocks of a slot, if they verify, its log is well formed and the bitmap marks them and
/// the root.
fn bitmap_blocks(disk: &[Buf], slot: u64, f: &[u64; 18]) -> Option<Vec<u64>> {
    let (sb, blocks) = (&disk[slot as usize], f[2]);
    let (pages, words) = (pages(blocks), blocks.div_ceil(64) as usize);
    let ws = pages * PAGE_WORDS;
    let (list, ix) = page_list(disk, f)?;
    let mut bits = vec![0u64; 2 * ws];
    let mut held: Vec<u64> = ix.iter().map(|x| x.0).collect();
    for (p, &((b, sum), _)) in list.iter().enumerate() {
        if (b, sum) == (0, 0) {
            continue;
        }
        let n = page_words(blocks, p);
        if !(2..blocks).contains(&b)
            || checksum(Block(b), &disk[b as usize][..8 * n]).0 != sum
            || disk[b as usize][8 * n..].iter().any(|&b| b != 0)
        {
            return None;
        }
        for i in 0..n {
            bits[p * PAGE_WORDS + i] = le64(&disk[b as usize], 8 * i);
        }
        held.push(b);
    }
    let at = SB_HDR + 16 * if f[14] == 0 { 2 * pages } else { 1 };
    let mut last = None;
    for j in 0..f[13] as usize {
        let (i, v) = (le64(sb, at + 16 * j), le64(sb, at + 16 * j + 8));
        if i as usize >= 2 * ws || i as usize % ws >= words || last.is_some_and(|l| l >= i) {
            return None;
        }
        bits[i as usize] = v;
        last = Some(i);
    }
    let has = |b: u64| bits[(b / 64) as usize] >> (b % 64) & 1 != 0;
    let pinned = |b: u64| bits[ws + (b / 64) as usize] >> (b % 64) & 1 != 0;
    let tail = (blocks..(words as u64 * 64)).any(|b| has(b) || pinned(b));
    (!tail && [0, 1, f[9]].iter().chain(&held).all(|&b| has(b))).then_some(held)
}

/// Each node a root reaches through pointers that verify, with its parent node and entry (`None` for the root).
fn tree_nodes(disk: &[Buf], root: u64, sum: u64) -> Vec<(u64, Option<(u64, usize)>)> {
    let (mut out, mut seen) = (vec![], HashSet::new());
    let mut stack = vec![(root, sum, None)];
    while let Some((b, sum, parent)) = stack.pop() {
        if b >= disk.len() as u64 || !seen.insert(b) {
            continue;
        }
        let n = &disk[b as usize];
        if le64(n, END) != sum || checksum(Block(b), &n[..END]).0 != sum {
            continue;
        }
        out.push((b, parent));
        // A node whose count breaks the format holds no entries.
        if n[0] > 0 && count(n) <= FANOUT {
            for i in 0..count(n) {
                let e = eptr(n, i);
                stack.push((e.block.0, e.sum.0, Some((b, i))));
            }
        }
    }
    out
}

/// The extents (first block, pages, sums offset) a leaf's items hold; none if its items break the format's layout
/// (bytes past the count, as a removed item leaves, are not items).
fn extents(n: &Buf) -> Vec<(u64, usize, usize)> {
    let (mut out, c) = (vec![], count(n));
    if n[0] != 0 || c * ITEM > CAP || !packed(n) {
        return out;
    }
    for i in 0..c {
        let (k, off, len) = (ikey(n, i).0, voff(n, i), vlen(n, i));
        if (k as u64) >> 62 == ItemKind::Extent as u64 && len >= 16 {
            out.push((le64(n, off), (len - 8) / 8, off + 8));
        }
    }
    out
}

/// The blocks slot `slot` reaches, if it is valid: its bitmap, nodes and data pages that verify, and those of each
/// snapshot its tree lists (their trees, and their bitmaps' index blocks and pages).
fn slot_blocks(disk: &[Buf], slot: u64) -> Option<HashSet<u64>> {
    let f = superblock_fields(disk, slot)?;
    let mut set: HashSet<u64> = bitmap_blocks(disk, slot, &f)?.into_iter().collect();
    let mut snaps = vec![];
    tree_blocks(disk, f[9], f[10], &mut set, Some(&mut snaps));
    let (pages, h) = (pages(f[2]), fan_height(pages(f[2])));
    for (root, sum, ix, ix_sum) in snaps {
        tree_blocks(disk, root, sum, &mut set, None);
        let mut level = vec![(ix, ix_sum)];
        for l in (1..=h).rev() {
            let mut next = vec![];
            for (i, &(b, sum)) in level.iter().enumerate() {
                let len = 16 * ix_children(pages, l, i);
                let Some(n) = disk.get(b as usize) else {
                    continue;
                };
                if b < 2 || checksum(Block(b), &n[..len]).0 != sum {
                    continue;
                }
                set.insert(b);
                next.extend((0..len / 16).map(|j| (le64(n, 16 * j), le64(n, 16 * j + 8))));
            }
            level = next;
        }
        set.extend(
            level
                .iter()
                .map(|e| e.0)
                .filter(|&b| b >= 2 && (b as usize) < disk.len()),
        );
    }
    Some(set)
}

/// Adds the nodes and data pages that verify under `root` to `set`; with `snaps`, collects the snapshot items (tree
/// root and sum, bitmap index root and sum) its leaves hold under `ROOT`.
fn tree_blocks(
    disk: &[Buf],
    root: u64,
    sum: u64,
    set: &mut HashSet<u64>,
    mut snaps: Option<&mut Vec<(u64, u64, u64, u64)>>,
) {
    for (b, _) in tree_nodes(disk, root, sum) {
        set.insert(b);
        let n = &disk[b as usize];
        for (start, pages, at) in extents(n) {
            for j in 0..pages {
                let p = start.wrapping_add(j as u64);
                if p < disk.len() as u64
                    && checksum(Block(p), &disk[p as usize]).0 == le64(n, at + 8 * j)
                {
                    set.insert(p);
                }
            }
        }
        if let Some(snaps) = snaps.as_deref_mut()
            && n[0] == 0
            && count(n) * ITEM <= CAP
            && packed(n)
        {
            for i in 0..count(n) {
                let (k, v) = (ikey(n, i).0, value(n, i));
                if k >> 64 == 0 && (k as u64) >> 62 == 3 && v.len() == SNAP_LEN {
                    snaps.push((le64(v, 0), le64(v, 8), le64(v, 32), le64(v, 40)));
                }
            }
        }
    }
}

/// An in-memory disk that refuses (and records) a write to a block a valid slot reaches.
pub(crate) struct Guarded {
    pub(crate) blocks: Vec<Buf>,
    reached: Vec<bool>,
    pub(crate) violation: Option<u64>,
    /// Panic on a violation instead of failing the write.
    strict: bool,
}

impl Guarded {
    pub(crate) fn new(blocks: Vec<Buf>, strict: bool) -> Self {
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
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error> {
        let b = block as usize;
        if b + bufs.len() > self.blocks.len() {
            return Err(Error::Io);
        }
        bufs.copy_from_slice(&self.blocks[b..][..bufs.len()]);
        Ok(())
    }

    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error> {
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
    cache: Vec<Buf>,
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
        if p.slot().is_none() {
            assert!(blocks.insert(p.block.0), "node {} reached twice", p.block.0);
        }
        let s = fs.node(p, level, Key(0), NONE).unwrap();
        let n = fs.cache[s];
        if level > 0 {
            for i in (0..count(&n)).rev() {
                stack.push((eptr(&n, i), level - 1));
            }
            continue;
        }
        for i in 0..count(&n) {
            let (k, v) = (ikey(&n, i).0, value(&n, i).to_vec());
            if (k as u64) >> 62 == ItemKind::Extent as u64 {
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
        .filter(|(k, _)| (*k as u64) >> 62 == ItemKind::Inode as u64)
        .map(|(k, v)| ((k >> 64) as u64, v[0]))
        .collect();
    let mut seen = HashSet::from([0]);
    for (k, v) in items
        .iter()
        .filter(|(k, _)| (*k as u64) >> 62 == ItemKind::Entry as u64)
    {
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
    // Listings and lookups, twice so the second runs from the memos, agree with the items.
    for (&dir, _) in inodes.iter().filter(|(_, k)| **k == DIR) {
        let want: Vec<(Vec<u8>, u64)> = items
            .iter()
            .filter(|(k, _)| (k >> 64) as u64 == dir && (*k as u64) >> 62 == ItemKind::Entry as u64)
            .map(|(_, v)| (v[9..].to_vec(), le64(v, 0)))
            .collect();
        for _ in 0..2 {
            let mut got = vec![];
            fs.readdir(Inode(dir), 0, |n, i, _| {
                got.push((n.to_vec(), i.0));
                false
            })
            .unwrap();
            assert_eq!(got, want, "{ctx}: listing of {dir}");
            for (n, i) in &want {
                assert_eq!(
                    fs.lookup(Inode(dir), n),
                    Ok(Inode(*i)),
                    "{ctx}: lookup in {dir}"
                );
            }
        }
    }
    blocks.extend([0, 1]);
    for p in 0..fs.lpages() {
        let (s, at) = fs.page_entry(p);
        blocks.insert(le64(&fs.cache[s], at));
    }
    for l in 1..=fs.ix_h {
        for i in 0..ix_count(fs.lpages(), l) {
            blocks.insert(match fs.ix_parent(l, i) {
                Some((s, at)) => le64(&fs.cache[s], at),
                None => fs.ix.0.0,
            });
        }
    }
    for b in 0..fs.blocks {
        assert_eq!(
            fs.has(LIVE, Block(b)),
            blocks.contains(&b),
            "{ctx}: live bit {b}"
        );
    }
    let used = (0..fs.blocks).filter(|&b| fs.used(Block(b))).count() as u64;
    assert_eq!(fs.free, fs.blocks - used, "{ctx}: free count");
    assert!((0..fs.hint.0).all(|b| fs.used(Block(b))), "{ctx}: hint");
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
        // Each snapshot with its files; deletes not yet committed come back at a mount.
        let mut snaps: Vec<(Snapshot, Files)> = vec![];
        let mut deleted: Vec<(Snapshot, Files)> = vec![];
        for step in 0..300 {
            let ctx = format!("seed {seed} step {step}");
            let op = next(rng, 18);
            let r = match op {
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
                            fs.unlink(d, &name, |_| false).map(|()| {
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
                    // Half write the pages, which small disks otherwise do only at format.
                    fs.full |= next(rng, 2) == 0;
                    fs.commit().unwrap();
                    deleted.clear();
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
                    snaps.append(&mut deleted);
                }),
                15 => fs.snapshot().map(|s| {
                    deleted.clear();
                    snaps.push((s, files_of(&mut fs).unwrap()));
                }),
                16 if !snaps.is_empty() => {
                    let i = next(rng, snaps.len() as u64) as usize;
                    fs.delete_snapshot(snaps[i].0)
                        .map(|()| deleted.push(snaps.swap_remove(i)))
                }
                10 => {
                    let d = dirs[next(rng, dirs.len() as u64) as usize];
                    match pick(&mut fs, d, rng) {
                        Some((name, i)) => {
                            let _ = fs.lookup(d, &name);
                            fs.unlink(d, &name, |_| false).map(|()| {
                                assert_eq!(fs.lookup(d, &name), Err(Error::NotFound), "{ctx}");
                                files.retain(|&f| f != i);
                                dirs.retain(|&d| d != i);
                            })
                        }
                        None => Ok(()),
                    }
                }
                11 => {
                    let from = dirs[next(rng, dirs.len() as u64) as usize];
                    let to = dirs[next(rng, dirs.len() as u64) as usize];
                    match pick(&mut fs, from, rng) {
                        Some((name, i)) => {
                            let _ = fs.lookup(from, &name);
                            match fs.rename(from, &name, to, format!("r{step}").as_bytes()) {
                                Err(Error::InvalidName) => {
                                    assert!(
                                        subtree_has(&mut fs, i, to),
                                        "{ctx}: refused a legal move"
                                    );
                                    Ok(())
                                }
                                Ok(())
                                    if (from, name.as_slice())
                                        != (to, format!("r{step}").as_bytes()) =>
                                {
                                    assert_eq!(
                                        fs.lookup(from, &name),
                                        Err(Error::NotFound),
                                        "{ctx}"
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
            if matches!(op, 9 | 15 | 16) || step % 25 == 0 {
                let ids: Vec<u64> = snaps.iter().map(|s| s.0.0).collect();
                check_pinned(&mut fs, &ids, &ctx);
                for (s, model) in &snaps {
                    assert_eq!(
                        view_files(&mut fs, *s).as_ref(),
                        Ok(model),
                        "{ctx}: snapshot {s:?}"
                    );
                }
            }
        }
    }
}

/// An image both slots reach parts of: a root of 150 long-named files (a tree of height 2), a directory with files, a
/// 20-page file and a sparse one; then a snapshot, under which the last commit overwrites a page, unlinks and renames
/// (so it pins blocks).
fn mutation_base() -> Vec<Buf> {
    // Six bitmap pages under the unit tests' sizes: two index levels.
    let blocks = 3000;
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
    fs.snapshot().unwrap();
    fs.write(big, 5 * BLOCK_SIZE as u64, &[1; 10]).unwrap();
    for i in 0..10 {
        fs.unlink(ROOT, format!("{i:0>40}").as_bytes(), |_| false)
            .unwrap();
    }
    fs.rename(ROOT, b"sparse", sub, b"moved").unwrap();
    fs.commit().unwrap();
    assert!(fs.height() >= 2);
    disk.blocks
}

fn reseal_superblock(disk: &mut [Buf], slot: usize) {
    let sum = checksum(Block(slot as u64), &disk[slot][..END]);
    disk[slot][END..].copy_from_slice(&sum.0.to_le_bytes());
}

/// Writes `sum` into the pointer at `at` (`None`: the superblock's list entry `p`), then reseals each index block
/// above it and the superblock.
fn reseal_list(
    disk: &mut [Buf],
    slot: usize,
    ix: &[(u64, At, usize)],
    mut at: At,
    p: usize,
    mut sum: u64,
) {
    while let Some((b, off)) = at {
        disk[b as usize][off + 8..off + 16].copy_from_slice(&sum.to_le_bytes());
        let &(_, up, len) = ix.iter().find(|x| x.0 == b).unwrap();
        sum = seal(Block(b), &mut disk[b as usize], len).0;
        at = up;
    }
    let off = SB_HDR + 16 * p;
    disk[slot][off + 8..off + 16].copy_from_slice(&sum.to_le_bytes());
    reseal_superblock(disk, slot);
}

/// Reseals node `b` and each parent up to slot `slot`'s superblock.
fn reseal_up(
    disk: &mut [Buf],
    slot: usize,
    parents: &HashMap<u64, Option<(u64, usize)>>,
    mut b: u64,
) {
    loop {
        let sum = seal(Block(b), &mut disk[b as usize], END);
        match parents[&b] {
            None => {
                disk[slot][80..88].copy_from_slice(&sum.0.to_le_bytes());
                return reseal_superblock(disk, slot);
            }
            Some((p, i)) => {
                disk[p as usize][HDR + ENTRY * i + 24..][..8].copy_from_slice(&sum.0.to_le_bytes());
                b = p;
            }
        }
    }
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

fn read(disk: &[Buf], b: u64, at: usize, width: usize) -> u128 {
    let mut v = [0; 16];
    v[..width].copy_from_slice(&disk[b as usize][at..at + width]);
    u128::from_le_bytes(v)
}

fn write(disk: &mut [Buf], b: u64, at: usize, width: usize, rng: &mut u64) {
    let v = mutated(read(disk, b, at, width), width, rng);
    disk[b as usize][at..at + width].copy_from_slice(&v.to_le_bytes()[..width]);
}

/// Changes one decoded field of a slot's superblock (its header, page list or log), bitmap page or tree node, resealing every sum
/// above it. Returns false, leaving the change out, for an extent moved onto blocks a slot reaches: catching that
/// takes a walk of every root (step 41's scrub), not a check when decoded.
fn mutate(disk: &mut [Buf], rng: &mut u64) -> bool {
    let slot = next(rng, 2) as usize;
    // A slot an earlier change already made invalid takes no more.
    let Some(f) = superblock_fields(disk, slot as u64)
        .filter(|f| bitmap_blocks(disk, slot as u64, f).is_some())
    else {
        return true;
    };
    let (list, ix) = page_list(disk, &f).unwrap();
    let inline = if f[14] == 0 { 2 * pages(f[2]) } else { 1 };
    match next(rng, 9) {
        0 => {
            let field = next(rng, 16) as usize;
            write(disk, slot as u64, 8 * field, 8, rng);
            reseal_superblock(disk, slot);
            // A newest-snapshot field below a snapshot item leaves that snapshot unloaded, so its blocks may be
            // reused: mount reads no item to check it (it costs a descent), scrub does.
            field != 15
        }
        1 => {
            // A page list or log field; a log value may then free a reachable block, which only scrub can catch.
            let fields = 2 * (inline as u64 + f[13]);
            let at = SB_HDR + 8 * next(rng, fields) as usize;
            write(disk, slot as u64, at, 8, rng);
            reseal_superblock(disk, slot);
            // A log entry still well formed may free a reachable block: a scrub finding, left out.
            at < SB_HDR + 16 * inline
                || superblock_fields(disk, slot as u64)
                    .is_none_or(|f| bitmap_blocks(disk, slot as u64, &f).is_none())
        }
        2 => {
            // Only marking a block used: a bitmap that frees a reachable block is also a scrub finding.
            let ((page, _), at) = list[0];
            if page == 0 {
                return true;
            }
            let bit = next(rng, f[2].min(PAGE_BITS));
            disk[page as usize][(bit / 8) as usize] |= 1 << (bit % 8);
            let sum = checksum(
                Block(page),
                &disk[page as usize][..8 * f[2].div_ceil(64).min(PAGE_WORDS as u64) as usize],
            );
            reseal_list(disk, slot, &ix, at, 0, sum.0);
            true
        }
        3 if !ix.is_empty() => {
            // An index block's field, resealed up to the superblock.
            let (b, at, len) = ix[next(rng, ix.len() as u64) as usize];
            write(disk, b, 8 * next(rng, len as u64 / 8) as usize, 8, rng);
            let sum = seal(Block(b), &mut disk[b as usize], len).0;
            reseal_list(disk, slot, &ix, at, 0, sum);
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
                bottom(n).min(END)
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
                let _ = fs.map(f, Page(p));
            }
        }
        for (j, &f) in files.iter().take(10).enumerate() {
            let _ = fs.write(f, (j * 3000) as u64, &[j as u8; 5000]);
        }
        if let Some(&f) = files.get(10) {
            let _ = fs.truncate(f);
        }
        for (d, n) in names.iter().take(15) {
            let _ = fs.unlink(*d, n, |_| false);
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
        // Every snapshot read through a view, then one taken and the oldest deleted.
        let mut snaps = vec![];
        let _ = fs.snapshots(|s| snaps.push(s));
        for &s in &snaps {
            if let Ok(mut v) = fs.view(s) {
                let mut entries = vec![];
                let _ = v.readdir(ROOT, 0, |n, inode, _| {
                    entries.push((n.to_vec(), inode));
                    entries.len() > 200
                });
                for (n, inode) in entries {
                    let _ = v.lookup(ROOT, &n);
                    let _ = v.stat(inode);
                    let _ = v.read(inode, 0, &mut buf);
                    let _ = v.map(inode, Page(1));
                }
            }
        }
        let _ = fs.snapshot();
        if let Some(&s) = snaps.first() {
            let _ = fs.delete_snapshot(s);
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

/// A `Guarded` disk that fails one request in `rate` with `Io`, changing nothing, while `armed`.
struct Faulty {
    inner: Guarded,
    rng: u64,
    rate: u64,
    armed: bool,
}

impl Faulty {
    fn fails(&mut self) -> bool {
        self.armed && next(&mut self.rng, self.rate) == 0
    }
}

impl Disk for &mut Faulty {
    fn read(&mut self, block: u64, bufs: &mut [Buf]) -> Result<(), Error> {
        if self.fails() {
            return Err(Error::Io);
        }
        (&mut self.inner).read(block, bufs)
    }

    fn write(&mut self, block: u64, bufs: &[Buf]) -> Result<(), Error> {
        if self.fails() {
            return Err(Error::Io);
        }
        (&mut self.inner).write(block, bufs)
    }

    fn flush(&mut self) -> Result<(), Error> {
        if self.fails() {
            return Err(Error::Io);
        }
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.inner.blocks.len() as u64
    }
}

type Files = Vec<(String, Option<Vec<u8>>)>;

/// Every file and directory as seen through `readdir`, `lookup`, `stat` and `read`, sorted.
fn files<D: Disk>(fs: &mut Fs<D>) -> Result<Files, Error> {
    let (mut out, mut stack) = (vec![], vec![(ROOT, String::new())]);
    while let Some((dir, path)) = stack.pop() {
        let mut entries = vec![];
        fs.readdir(dir, 0, |n, i, k| {
            entries.push((n.to_vec(), i, k));
            false
        })?;
        for (n, i, k) in entries {
            let p = format!("{path}/{}", String::from_utf8_lossy(&n));
            assert_eq!(fs.lookup(dir, &n)?, i, "{p}");
            if k == Kind::Dir {
                out.push((p.clone(), None));
                stack.push((i, p));
            } else {
                let mut b = vec![0; fs.stat(i)?.size as usize];
                fs.read(i, 0, &mut b)?;
                out.push((p, Some(b)));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// As `files`, through a view of snapshot `s`.
fn view_files<D: Disk>(fs: &mut Fs<D>, s: Snapshot) -> Result<Files, Error> {
    let mut v = fs.view(s)?;
    let (mut out, mut stack) = (vec![], vec![(ROOT, String::new())]);
    while let Some((dir, path)) = stack.pop() {
        let mut entries = vec![];
        v.readdir(dir, 0, |n, i, k| {
            entries.push((n.to_vec(), i, k));
            false
        })?;
        for (n, i, k) in entries {
            let p = format!("{path}/{}", String::from_utf8_lossy(&n));
            assert_eq!(v.lookup(dir, &n)?, i, "{p}");
            if k == Kind::Dir {
                out.push((p.clone(), None));
                stack.push((i, p));
            } else {
                let mut b = vec![0; v.stat(i)?.size as usize];
                v.read(i, 0, &mut b)?;
                out.push((p, Some(b)));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The live files, as `files` reads them.
fn files_of<D: Disk>(fs: &mut Fs<D>) -> Result<Files, Error> {
    files(fs)
}

/// Random changes on a disk that fails requests at random, and on disks that fill, against a model of the files: every
/// view (listing, lookup, stat, read) shows what the changes so far should leave, checked only now and then so that
/// the memos and the unwritten data page live across changes; `NoSpace` changes nothing; after a failed commit reads
/// still show the state in memory, after any other error every call fails with `Io`, and a remount gives exactly the
/// last committed state (either side of a commit whose outcome is unknown) with a consistent tree and bitmap.
#[test]
fn disk_errors_and_full_disks_never_leave_stale_state() {
    type Model = std::collections::BTreeMap<String, Option<Vec<u8>>>;
    let seeds = std::env::var("FAULT_SEEDS").map_or(300u64, |s| s.parse().unwrap());
    for seed in 1..=seeds {
        let rng = &mut seed.wrapping_mul(0x2545_f491_4f6c_dd1d);
        let blocks = [64, 200, 3000][seed as usize % 3];
        let mut disk = Faulty {
            inner: Guarded::new(vec![[0; BLOCK_SIZE]; blocks], true),
            rng: seed,
            rate: 40,
            armed: false,
        };
        let mut mem = Mem::new(blocks);
        let mut fs = mem.fs(&mut disk);
        fs.format(seed).unwrap();
        let (mut model, mut committed) = (Model::new(), Model::new());
        let view = |m: &Model| m.clone().into_iter().collect::<Files>();
        for step in 0..300 {
            let ctx = format!("seed {seed} step {step}");
            let has_d = model.contains_key("/d");
            let (dir, dp) = match next(rng, 2) {
                1 if has_d => {
                    fs.disk().armed = false;
                    (fs.lookup(ROOT, b"d").unwrap(), "/d")
                }
                _ => (ROOT, ""),
            };
            let name = format!("n{}", next(rng, 12));
            let other = format!("n{}", next(rng, 12));
            let path = format!("{dp}/{name}");
            let (len, at) = (
                next(rng, 3 * BLOCK_SIZE as u64) as usize,
                next(rng, 3 * BLOCK_SIZE as u64) as usize,
            );
            fs.disk().armed = true;
            let op = next(rng, 16);
            let r = match op {
                0 | 1 => fs.create(dir, name.as_bytes()).map(|_| ()),
                2 => fs.mkdir(ROOT, b"d").map(|_| ()),
                3..=5 => fs
                    .lookup(dir, name.as_bytes())
                    .and_then(|f| fs.write(f, at as u64, &vec![step as u8; len])),
                6 => fs.lookup(dir, name.as_bytes()).and_then(|f| fs.truncate(f)),
                7 => fs.unlink(dir, name.as_bytes(), |_| false),
                8 => fs.rename(dir, name.as_bytes(), ROOT, other.as_bytes()),
                9 | 10 => fs.commit(),
                11 => fs.mount(),
                // A read of one file, through the memos.
                12 => fs.lookup(dir, name.as_bytes()).and_then(|f| {
                    let mut b = vec![0; 7 * BLOCK_SIZE];
                    let n = fs.read(f, 0, &mut b)?;
                    if let Some(Some(want)) = model.get(&path) {
                        assert_eq!(&b[..n], &want[..], "{ctx}: read {path}");
                    }
                    Ok(())
                }),
                // A snapshot commits first; the oldest is deleted.
                14 => fs.snapshot().map(|_| ()),
                15 => {
                    let mut oldest = None;
                    fs.snapshots(|s| _ = oldest.get_or_insert(s))
                        .and_then(|()| oldest.map_or(Ok(()), |s| fs.delete_snapshot(s)))
                }
                _ => files(&mut fs).map(|got| assert_eq!(got, view(&model), "{ctx}")),
            };
            fs.disk().armed = false;
            if fs.broken {
                // A failed commit, or a read that failed to write the unwritten page out, leaves the tree in memory
                // whole; an error in the middle of a change, or a failed mount, refuses reads too.
                if matches!(op, 9 | 10 | 12 | 13) {
                    assert_eq!(
                        files(&mut fs),
                        Ok(view(&model)),
                        "{ctx}: after op {op}: {r:?}"
                    );
                } else if op == 14 {
                    // Its commits leave the tree whole; an error between them, in its item and bitmap, does not.
                    let r = files(&mut fs);
                    assert!(
                        r == Ok(view(&model)) || r == Err(Error::Io),
                        "{ctx}: after a snapshot: {r:?}"
                    );
                } else {
                    assert_eq!(
                        files(&mut fs),
                        Err(Error::Io),
                        "{ctx}: reads after op {op}: {r:?}"
                    );
                }
                assert_eq!(fs.create(ROOT, b"refused"), Err(Error::Io), "{ctx}");
                assert_eq!(fs.commit(), Err(Error::Io), "{ctx}");
                fs.mount().unwrap();
                let now = files(&mut fs).unwrap();
                let unknown = matches!(op, 9 | 10 | 14) && now == view(&model);
                assert!(
                    now == view(&committed) || unknown,
                    "{ctx}: remount after op {op}: {r:?}"
                );
                model = now.into_iter().collect();
                committed = model.clone();
                check(&mut fs, &ctx);
                continue;
            }
            let put = |m: &mut Model, at: usize, len: usize| {
                if let Some(Some(b)) = m.get_mut(&path)
                    && len > 0
                {
                    if b.len() < at + len {
                        b.resize(at + len, 0);
                    }
                    b[at..at + len].fill(step as u8);
                }
            };
            // The outcome the model predicts, `NoSpace` aside.
            let exists = matches!(model.get(&path), Some(Some(_)));
            let target = format!("/{other}");
            let want = match op {
                3..=8 | 12 if !exists => Err(Error::NotFound),
                8 if target != path && model.contains_key(&target) => Err(Error::Exists),
                2 if has_d => Err(Error::Exists),
                _ => Ok(()),
            };
            // A read that fails before any change leaves the file system whole.
            let refused = matches!(r, Err(Error::NoSpace | Error::Io));
            assert!(r == want || refused, "{ctx}: op {op} on {path} gave {r:?}");
            match (op, r) {
                // `NoSpace` changes nothing; other refusals (`Exists`, `IsDir`, `NotFound`...) neither.
                (_, Err(_)) => {}
                (0 | 1, Ok(())) => _ = model.entry(path.clone()).or_insert(Some(vec![])),
                (2, Ok(())) => _ = model.insert("/d".into(), None),
                (3..=5, Ok(())) => put(&mut model, at, len),
                (6, Ok(())) => _ = model.insert(path.clone(), Some(vec![])),
                (7, Ok(())) => _ = model.remove(&path),
                (8, Ok(())) => {
                    if let Some(v) = model.remove(&path) {
                        model.insert(format!("/{other}"), v);
                    }
                }
                (9 | 10 | 14, Ok(())) => committed = model.clone(),
                (11, Ok(())) => {
                    model = committed.clone();
                    assert_eq!(files(&mut fs), Ok(view(&model)), "{ctx}: mount");
                }
                _ => {}
            }
        }
    }
}

/// Commit moves an unwritten page whose next block is taken to a free run; the extent rewrite that moves it can write
/// dirty nodes out early, which must not take the page's new block.
#[test]
fn a_moved_data_page_keeps_its_block_when_its_extent_rewrite_spills() {
    let blocks = 3000;
    let mut disk = Guarded::new(vec![[0; BLOCK_SIZE]; blocks], true);
    let mut mem = Mem::new(blocks);
    let mut fs = mem.fs(&mut disk);
    fs.format(1).unwrap();
    let files: Vec<Inode> = (0..2000)
        .map(|i| fs.create(ROOT, format!("f{i}").as_bytes()).unwrap())
        .collect();
    let (a, b) = (
        fs.create(ROOT, b"a").unwrap(),
        fs.create(ROOT, b"b").unwrap(),
    );
    fs.write(a, 0, &[1; BLOCK_SIZE]).unwrap();
    fs.write(b, 0, &[2; BLOCK_SIZE]).unwrap();
    fs.commit().unwrap();
    fs.unlink(ROOT, b"a", |_| false).unwrap();
    fs.commit().unwrap();
    fs.commit().unwrap();
    // `c`'s page takes `a`'s old block, followed by `b`'s.
    let c = fs.create(ROOT, b"c").unwrap();
    fs.write(c, 0, &[3; BLOCK_SIZE]).unwrap();
    let d = fs.cached.unwrap().0;
    // Leave the pool short of slots, so the next path made dirty writes the dirty nodes out first.
    for &f in files.iter().step_by(7) {
        if fs.top - fs.base - fs.ndirty < 3 * (fs.height + 2) {
            break;
        }
        fs.cow(
            Key::new(f, ItemKind::Inode, Offset(0)),
            &mut Path::default(),
        )
        .unwrap();
    }
    assert!(fs.top - fs.base - fs.ndirty < 3 * (fs.height + 2));
    fs.commit().unwrap();
    assert_ne!(fs.cached.unwrap().0, d, "moved");
    fs.mount().unwrap();
    check(&mut fs, "remount");
    let mut got = [0; BLOCK_SIZE];
    assert_eq!(fs.read(c, 0, &mut got), Ok(BLOCK_SIZE));
    assert_eq!(got, [3; BLOCK_SIZE]);
}

/// Commits that write the pages (forced), on a disk whose index and pages were written by earlier ones: releasing an old page or
/// index block changes a page the pass already went by, which must be written too, every old block freed.
#[test]
fn pages_commits_on_an_aged_disk_release_every_old_block() {
    for seed in 1..=4u64 {
        let rng = &mut seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let blocks = 20000;
        let mut disk = Guarded::new(vec![[0; BLOCK_SIZE]; blocks], true);
        let mut mem = Mem::new(blocks);
        let mut fs = mem.fs(&mut disk);
        fs.format(seed).unwrap();
        let f = fs.create(ROOT, b"f").unwrap();
        fs.write(f, 0, &vec![1; 10000 * BLOCK_SIZE]).unwrap();
        fs.commit().unwrap();
        for step in 0..60 {
            for _ in 0..1 + next(rng, 4) {
                let at = next(rng, 10000) * BLOCK_SIZE as u64;
                fs.write(f, at, &[step as u8; BLOCK_SIZE]).unwrap();
            }
            fs.full = true;
            fs.commit().unwrap();
            fs.mount().unwrap();
            check(&mut fs, &format!("seed {seed} step {step}"));
        }
    }
}

/// The blocks snapshot `g` reaches: its tree's nodes and data, and its bitmap's index blocks and pages, through the
/// disk (a restatement of the format).
fn snapshot_reach(disk: &[Buf], fs: &mut Fs<&mut Guarded>, g: u64) -> HashSet<u64> {
    let (root, _, ix) = fs.snap_item(g).unwrap();
    let mut set = HashSet::new();
    for (b, _) in tree_nodes(disk, root.block.0, root.sum.0) {
        set.insert(b);
        for (start, pages, _) in extents(&disk[b as usize]) {
            set.extend((0..pages as u64).map(|j| start + j));
        }
    }
    let (pages, h) = (fs.pages, fan_height(fs.pages));
    let mut level = vec![ix];
    for l in (1..=h).rev() {
        let mut next = vec![];
        for (i, &(b, _)) in level.iter().enumerate() {
            set.insert(b.0);
            let n = &disk[b.0 as usize];
            for j in 0..ix_children(pages, l, i) {
                next.push((Block(le64(n, 16 * j)), Sum(le64(n, 16 * j + 8))));
            }
        }
        level = next;
    }
    set.extend(level.iter().map(|e| e.0.0).filter(|&b| b != 0));
    set
}

/// The blocks snapshot `g`'s bitmap marks, read through its index from the disk.
fn snapshot_bitmap(disk: &[Buf], fs: &mut Fs<&mut Guarded>, g: u64) -> HashSet<u64> {
    let (_, _, ix) = fs.snap_item(g).unwrap();
    let (pages, h) = (fs.pages, fan_height(fs.pages));
    let mut level = vec![ix];
    for l in (1..=h).rev() {
        let mut next = vec![];
        for (i, &(b, _)) in level.iter().enumerate() {
            let n = &disk[b.0 as usize];
            for j in 0..ix_children(pages, l, i) {
                next.push((Block(le64(n, 16 * j)), Sum(le64(n, 16 * j + 8))));
            }
        }
        level = next;
    }
    let mut set = HashSet::new();
    for (p, &(b, _)) in level.iter().enumerate() {
        if b != Block(0) {
            let page = &disk[b.0 as usize];
            for i in 0..page_words(fs.blocks, p) {
                let w = le64(page, 8 * i);
                set.extend(
                    (0..64)
                        .filter(|k| w >> k & 1 != 0)
                        .map(|k| (p * PAGE_WORDS + i) as u64 * 64 + k),
                );
            }
        }
    }
    set
}

/// Each snapshot's bitmap marks every block it reaches; pinned holds exactly the blocks some snapshot's bitmap marks
/// and the live tree does not (a snapshot's bitmap may also mark a live page it keeps its own copy of).
fn check_pinned(fs: &mut Fs<&mut Guarded>, snaps: &[u64], ctx: &str) {
    let disk = fs.disk().blocks.clone();
    let mut held = HashSet::new();
    for &g in snaps {
        let marks = snapshot_bitmap(&disk, fs, g);
        let reach = snapshot_reach(&disk, fs, g);
        assert!(
            reach.is_subset(&marks),
            "{ctx}: snapshot {g} reaches unmarked blocks"
        );
        held.extend(marks);
    }
    for b in 0..fs.blocks {
        assert_eq!(
            fs.has(PINNED, Block(b)),
            held.contains(&b) && !fs.has(LIVE, Block(b)),
            "{ctx}: pinned bit {b}"
        );
    }
}

/// Snapshots taken between changes keep exactly their blocks pinned; deleting one in the middle, the newest and the
/// last unpins exactly what no other root holds, and after two commits the free space is a fresh mount's.
#[test]
fn deleting_a_snapshot_frees_exactly_the_blocks_no_other_root_reaches() {
    let blocks = 3000;
    let mut disk = Guarded::new(vec![[0; BLOCK_SIZE]; blocks], true);
    let mut mem = Mem::new(blocks);
    let mut fs = mem.fs(&mut disk);
    fs.format(3).unwrap();
    let mut files = vec![];
    let mut snaps = vec![];
    for round in 0..4u64 {
        for i in 0..30 {
            let f = fs
                .create(ROOT, format!("r{round}-{i:0>30}").as_bytes())
                .unwrap();
            fs.write(f, 0, &vec![round as u8; 1 + i * 700]).unwrap();
            files.push(f);
        }
        for &f in files.iter().step_by(3) {
            // Some were unlinked in an earlier round.
            let r = fs.write(f, 100, &[0xee; 5000]);
            assert!(matches!(r, Ok(()) | Err(Error::NotFound)), "{r:?}");
        }
        for i in (0..30).step_by(4) {
            let _ = fs.unlink(
                ROOT,
                format!("r{}-{i:0>30}", round.saturating_sub(1)).as_bytes(),
                |_| false,
            );
        }
        snaps.push(fs.snapshot().unwrap().0);
        check(&mut fs, &format!("round {round}"));
        check_pinned(&mut fs, &snaps, &format!("round {round}"));
    }
    fs.mount().unwrap();
    check_pinned(&mut fs, &snaps, "remount");
    for g in [snaps[1], snaps[3], snaps[0], snaps[2]] {
        fs.delete_snapshot(Snapshot(g)).unwrap();
        snaps.retain(|&s| s != g);
        check_pinned(&mut fs, &snaps, &format!("delete {g}"));
        fs.commit().unwrap();
        fs.commit().unwrap();
        check(&mut fs, &format!("delete {g}"));
        let free = fs.free;
        let mut d = Guarded::new(fs.disk().blocks.clone(), true);
        let mut m = Mem::new(blocks);
        let mut fresh = m.fs(&mut d);
        fresh.mount().unwrap();
        assert_eq!(fresh.free, free, "delete {g}: free");
        check_pinned(&mut fresh, &snaps, &format!("delete {g}, fresh"));
        assert_eq!(fs.view(Snapshot(g)).map(|_| ()), Err(Error::NotFound));
    }
    assert!((0..fs.words).all(|i| fs.bits[PINNED * fs.words + i] == 0));
}
