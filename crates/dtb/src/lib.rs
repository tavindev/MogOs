#![cfg_attr(not(test), no_std)]

use core::ops::Range;

use mm::PhysAddr;

const MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;

/// Size of the whole blob from its header, or `None` if `header` is not an FDT header.
pub fn total_size(header: &[u8]) -> Option<usize> {
    if be32(header, 0)? != MAGIC {
        return None;
    }
    Some(be32(header, 4)? as usize)
}

/// A flattened device tree blob.
pub struct Dtb<'a> {
    structs: &'a [u8],
    strings: &'a [u8],
}

impl<'a> Dtb<'a> {
    pub fn new(blob: &'a [u8]) -> Option<Self> {
        let structs = be32(blob, 8)? as usize;
        let strings = be32(blob, 12)? as usize;
        Some(Self {
            structs: blob.get(structs..structs + be32(blob, 36)? as usize)?,
            strings: blob.get(strings..strings + be32(blob, 32)? as usize)?,
        })
    }

    /// First `reg` region of the top-level `memory` node.
    pub fn memory(&self) -> Option<Range<PhysAddr>> {
        self.find(|p| {
            let memory = p.node == b"memory" || p.node.starts_with(b"memory@");
            if p.depth != 2 || !memory || p.name != b"reg" {
                return None;
            }
            let (start, size) = p.reg(0)?;
            Some(PhysAddr(start)..PhysAddr(start + size))
        })
    }

    /// The first top-level GICv3 (`arm,gic-v3`) with its `#redistributor-regions`.
    pub fn gic(&self) -> Option<Gic<'a>> {
        let (mut node, mut reg, mut regions, mut found) = (0, None, None, false);
        self.find(|p| {
            if p.node_offset != node {
                (node, reg, regions, found) = (p.node_offset, None, None, false);
            }
            match p.name {
                b"reg" if p.depth == 2 => reg = Some(*p),
                b"#redistributor-regions" if p.depth == 2 => regions = be32(p.value, 0),
                b"compatible" => found = p.value.split(|&b| b == 0).any(|c| c == b"arm,gic-v3"),
                _ => {}
            }
            let gic = Gic {
                reg: reg?,
                regions: regions? as usize,
            };
            found.then_some(gic)
        })
    }

    /// Calls `f` with the `reg` (its MPIDR affinity on Arm) of each second-level node whose `device_type` is `cpu` (the
    /// `/cpus` children), in order, decoded with `/cpus`' `#address-cells`; returns how many.
    pub fn cpus(&self, mut f: impl FnMut(u64)) -> usize {
        let (mut cells, mut node, mut reg, mut cpu) = (1, 0, None, false);
        let mut count = 0;
        self.find(|p| {
            if p.depth == 2 && p.node == b"cpus" && p.name == b"#address-cells" {
                cells = be32(p.value, 0)? as usize;
            }
            if p.depth != 3 {
                return None::<()>;
            }
            if p.node_offset != node {
                (node, reg, cpu) = (p.node_offset, None, false);
            }
            match p.name {
                b"reg" => reg = self::cells(p.value, 0, cells),
                b"device_type" => cpu = p.value == b"cpu\0",
                _ => return None,
            }
            if let Some(mpidr) = reg.filter(|_| cpu) {
                f(mpidr);
                count += 1;
                // Once per node: neither property can complete it again.
                (reg, cpu) = (None, false);
            }
            None
        });
        count
    }

    /// `/chosen`'s `bootargs` (QEMU sets it from `-append`) and the first 16 bytes of its `rng-seed` (random on every
    /// boot), in one walk: the node is the blob's last, so each lookup walks it all.
    pub fn chosen(&self) -> (Option<&'a str>, Option<[u64; 2]>) {
        let (mut bootargs, mut seed) = (None, None);
        self.find(|p| {
            if p.depth != 2 || p.node != b"chosen" {
                return None;
            }
            match p.name {
                b"bootargs" => {
                    bootargs = p
                        .value
                        .strip_suffix(&[0])
                        .and_then(|v| core::str::from_utf8(v).ok())
                }
                b"rng-seed" => {
                    seed = p.value.split_first_chunk::<16>().map(|(s, _)| {
                        let (a, b) = s.split_at(8);
                        [a, b].map(|half| u64::from_be_bytes(half.try_into().unwrap()))
                    })
                }
                _ => {}
            }
            (bootargs.is_some() && seed.is_some()).then_some(())
        });
        (bootargs, seed)
    }

    /// Walks every property in order and returns the first `Some` from `f`.
    fn find<T>(&self, mut f: impl FnMut(&Prop<'a>) -> Option<T>) -> Option<T> {
        let (mut pos, mut depth, mut node, mut node_offset) = (0, 0, &[][..], 0);
        // Spec defaults, overridden by the root node's properties (which precede all subnodes).
        let (mut address_cells, mut size_cells) = (2, 1);
        loop {
            let token = be32(self.structs, pos)?;
            pos += 4;
            match token {
                BEGIN_NODE => {
                    let len = self.structs.get(pos..)?.iter().position(|&b| b == 0)?;
                    (node, node_offset) = (&self.structs[pos..pos + len], pos);
                    depth += 1;
                    pos = (pos + len + 1).next_multiple_of(4);
                }
                END_NODE => depth -= 1,
                PROP => {
                    let len = be32(self.structs, pos)? as usize;
                    let name_off = be32(self.structs, pos + 4)? as usize;
                    let value = self.structs.get(pos + 8..pos + 8 + len)?;
                    pos = (pos + 8 + len).next_multiple_of(4);
                    let name = self.strings.get(name_off..)?.split(|&b| b == 0).next()?;
                    match name {
                        b"#address-cells" if depth == 1 => address_cells = be32(value, 0)? as usize,
                        b"#size-cells" if depth == 1 => size_cells = be32(value, 0)? as usize,
                        _ => {}
                    }
                    let prop = Prop {
                        depth,
                        node,
                        node_offset,
                        name,
                        value,
                        address_cells,
                        size_cells,
                    };
                    if let Some(found) = f(&prop) {
                        return Some(found);
                    }
                }
                NOP => {}
                _ => return None,
            }
        }
    }
}

/// A GICv3's registers: `reg` holds the distributor, then each redistributor region.
pub struct Gic<'a> {
    reg: Prop<'a>,
    regions: usize,
}

impl Gic<'_> {
    pub fn distributor(&self) -> Option<PhysAddr> {
        Some(PhysAddr(self.reg.reg(0)?.0))
    }

    /// Each redistributor region's base and size.
    pub fn redistributors(&self) -> impl Iterator<Item = (PhysAddr, u64)> {
        (1..=self.regions).map_while(|i| self.reg.reg(i).map(|(base, size)| (PhysAddr(base), size)))
    }
}

#[derive(Clone, Copy)]
struct Prop<'a> {
    depth: usize,
    node: &'a [u8],
    /// Identifies the node: offset of its name in the struct block.
    node_offset: usize,
    name: &'a [u8],
    value: &'a [u8],
    address_cells: usize,
    size_cells: usize,
}

impl Prop<'_> {
    /// The `i`th (address, size) pair of a `reg` value, decoded with the root's cell counts.
    fn reg(&self, i: usize) -> Option<(u64, u64)> {
        let first = i * (self.address_cells + self.size_cells);
        let start = cells(self.value, first, self.address_cells)?;
        Some((
            start,
            cells(self.value, first + self.address_cells, self.size_cells)?,
        ))
    }
}

fn be32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Big-endian number spanning `count` 32-bit cells, starting at cell `first`.
fn cells(data: &[u8], first: usize, count: usize) -> Option<u64> {
    (first..first + count).try_fold(0, |acc, i| Some(acc << 32 | be32(data, i * 4)? as u64))
}
