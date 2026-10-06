//! Static AArch64 ELF64 executables: only `PT_LOAD` program headers matter.

use core::ops::Range;

const PAGE: u64 = 4096;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PHDR: usize = 56;

/// A `PT_LOAD` segment: `size` bytes at page-aligned `vaddr`, the first ones from `data` (byte offsets in the file),
/// the rest zero.
pub struct Segment {
    pub vaddr: u64,
    pub data: Range<usize>,
    pub size: u64,
    pub writable: bool,
}

pub struct Elf<'a> {
    file: &'a [u8],
    phdrs: &'a [[u8; PHDR]],
    pub entry: u64,
}

impl<'a> Elf<'a> {
    /// Checks every header of `file`: a little-endian ELF64 AArch64 executable whose entry and `PT_LOAD` segments lie
    /// in `region`, page-aligned, in address order, never sharing a page, none both writable and executable.
    pub fn parse(file: &'a [u8], region: Range<u64>) -> Option<Self> {
        let header = file.get(..64)?;
        let ident_ok = header[..7] == *b"\x7fELF\x02\x01\x01";
        if !ident_ok || u16_at(header, 16)? != 2 || u16_at(header, 18)? != 183 {
            return None;
        }
        if u16_at(header, 54)? as usize != PHDR {
            return None;
        }
        let phoff = usize::try_from(u64_at(header, 32)?).ok()?;
        let phnum = u16_at(header, 56)? as usize;
        let phdrs = file
            .get(phoff..phoff.checked_add(phnum * PHDR)?)?
            .as_chunks()
            .0;
        let elf = Self {
            file,
            phdrs,
            entry: u64_at(header, 24)?,
        };
        let mut free = region.start;
        for ph in phdrs {
            if u32_at(ph, 0)? != PT_LOAD {
                continue;
            }
            let s = segment(file, ph)?;
            if !s.vaddr.is_multiple_of(PAGE)
                || s.vaddr < free
                || s.size > region.end.checked_sub(s.vaddr)?
            {
                return None;
            }
            free = (s.vaddr + s.size).next_multiple_of(PAGE);
        }
        region.contains(&elf.entry).then_some(elf)
    }

    /// The `PT_LOAD` segments, as `parse` checked them.
    pub fn segments(self) -> impl Iterator<Item = Segment> + 'a {
        self.phdrs
            .iter()
            .filter(|ph| u32_at(*ph, 0) == Some(PT_LOAD))
            .filter_map(|ph| segment(self.file, ph))
    }
}

/// The segment `ph` describes, if its data lies in `file`, fits its size, and it is not writable and executable.
fn segment(file: &[u8], ph: &[u8]) -> Option<Segment> {
    let flags = u32_at(ph, 4)?;
    let offset = usize::try_from(u64_at(ph, 8)?).ok()?;
    let file_size = u64_at(ph, 32)?;
    let size = u64_at(ph, 40)?;
    let data = offset..offset.checked_add(usize::try_from(file_size).ok()?)?;
    file.get(data.clone())?;
    if file_size > size || flags & (PF_W | PF_X) == PF_W | PF_X {
        return None;
    }
    Some(Segment {
        vaddr: u64_at(ph, 16)?,
        data,
        size,
        writable: flags & PF_W != 0,
    })
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}
