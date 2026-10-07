//! Read-only lookup and listing in a cpio archive in the newc format (`cpio -H newc`).

use core::ops::Range;

const HEADER: usize = 110;

/// Where the data of the file named `name` lies in `archive`; `None` if it is absent or the archive is malformed.
pub fn find(archive: &[u8], name: &[u8]) -> Option<Range<usize>> {
    entries(archive).find(|e| e.0 == name).map(|e| e.1)
}

/// Each file's name and where its data lies, up to the trailer or the first malformed entry.
pub fn entries(archive: &[u8]) -> impl Iterator<Item = (&[u8], Range<usize>)> {
    let mut pos = 0;
    core::iter::from_fn(move || {
        let header = archive.get(pos..pos + HEADER)?;
        if &header[..6] != b"070701" {
            return None;
        }
        // Fields are 8 hex digits each after the magic: filesize is the 7th, namesize the 12th.
        let field = |i: usize| {
            let digits = core::str::from_utf8(&header[6 + 8 * i..14 + 8 * i]).ok()?;
            usize::from_str_radix(digits, 16).ok()
        };
        let (size, name_size) = (field(6)?, field(11)?);
        let entry = archive
            .get(pos + HEADER..pos + HEADER + name_size)?
            .strip_suffix(&[0])?;
        let data = (pos + HEADER + name_size).next_multiple_of(4);
        archive.get(data..data + size)?;
        if entry == b"TRAILER!!!" {
            return None;
        }
        pos = (data + size).next_multiple_of(4);
        Some((entry, data..data + size))
    })
}
