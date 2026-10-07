//! The file syscalls' work on MogFS and the boot archive, which the board runs with its file system. Paths are walked
//! here with one `lookup` per `/`-separated component, which must be a name (not `..`, `.` or empty), so a handle never
//! reaches above its directory through a path. The walk is bounded by the path's length: a crafted image can point an
//! entry at `ROOT` or an ancestor (a cycle), so nothing here recurses over the tree.

use mogfs::{Disk, Error, Fs, Inode, Kind, ROOT};

use crate::cpio;
use crate::handle::Object;
use crate::syscall::{
    CREATE, EEXIST, EFBIG, EINVAL, EIO, EISDIR, ENAMETOOLONG, ENOENT, ENOSPC, ENOTDIR, TRUNC,
};

/// Entries one `readdir` call lists at most.
const LIST: usize = 64;
/// Components a path has at most, which bounds a walk's lookups (each at most a directory's 14 blocks) with IRQs masked.
pub const MAX_DEPTH: usize = 16;

pub fn errno(error: Error) -> i64 {
    match error {
        Error::NotFound => ENOENT,
        Error::Exists => EEXIST,
        Error::NotDir => ENOTDIR,
        Error::IsDir => EISDIR,
        Error::InvalidName => EINVAL,
        Error::TooBig => EFBIG,
        Error::NoSpace => ENOSPC,
        Error::Io | Error::Corrupt => EIO,
    }
}

/// The directory under `dir` that holds `path`'s last component, and that component.
fn parent<'a, D: Disk>(
    fs: &mut Fs<D>,
    mut dir: Inode,
    path: &'a [u8],
) -> Result<(Inode, &'a [u8]), i64> {
    if path.iter().filter(|&&b| b == b'/').count() >= MAX_DEPTH {
        return Err(ENAMETOOLONG);
    }
    let mut components = path.split(|&b| b == b'/');
    let mut name = components.next().unwrap_or_default();
    for next in components {
        dir = fs.lookup(dir, name).map_err(errno)?;
        name = next;
    }
    Ok((dir, name))
}

/// The file or directory at `path` under `dir`; with `CREATE` a missing file is made, with `TRUNC` the file is emptied.
pub fn open<D: Disk>(fs: &mut Fs<D>, dir: Inode, path: &[u8], flags: u64) -> Result<Object, i64> {
    let (dir, name) = parent(fs, dir, path)?;
    let mut open = || {
        let inode = match fs.lookup(dir, name) {
            Err(Error::NotFound) if flags & CREATE != 0 => fs.create(dir, name)?,
            found => found?,
        };
        if flags & TRUNC != 0 {
            fs.truncate(inode)?;
        }
        Ok(match fs.kind(inode)? {
            Kind::Dir => Object::Dir(inode),
            Kind::File => Object::Node(inode),
        })
    };
    open().map_err(errno)
}

pub fn mkdir<D: Disk>(fs: &mut Fs<D>, dir: Inode, path: &[u8]) -> Result<(), i64> {
    let (dir, name) = parent(fs, dir, path)?;
    fs.mkdir(dir, name).map(|_| ()).map_err(errno)
}

/// Writes `dir`'s entries from index `start` on into `out`, as many whole `name\n` (`name/\n` for a directory) as fit
/// up to `LIST`; returns the bytes written, `EINVAL` if the next entry does not fit.
pub fn readdir<D: Disk>(
    fs: &mut Fs<D>,
    dir: Inode,
    start: u64,
    out: &mut [u8],
) -> Result<usize, i64> {
    // `fs.kind` cannot run inside `fs.readdir`, so names go in first, each keeping room for a `/` (`len + count`).
    let mut found = [(ROOT, 0, false); LIST];
    let (mut index, mut count, mut len, mut more) = (0, 0, 0, false);
    fs.readdir(dir, |name, inode| {
        index += 1;
        if index <= start || more {
            return;
        }
        if count == LIST || len + count + name.len() + 2 > out.len() {
            more = true;
            return;
        }
        out[len..][..name.len()].copy_from_slice(name);
        len += name.len() + 1;
        out[len - 1] = b'\n';
        found[count] = (inode, len, false);
        count += 1;
    })
    .map_err(errno)?;
    if more && count == 0 {
        return Err(EINVAL);
    }
    let mut shift = 0;
    for (inode, _, dir) in &mut found[..count] {
        *dir = fs.kind(*inode) == Ok(Kind::Dir);
        shift += *dir as usize;
    }
    let written = len + shift;
    // From the last entry back, each moves right by the `/`s before and in it.
    for k in (0..count).rev() {
        let (_, end, dir) = found[k];
        let begin = if k == 0 { 0 } else { found[k - 1].1 };
        if dir {
            out.copy_within(begin..end - 1, begin + shift - 1);
            out[end + shift - 2] = b'/';
            out[end + shift - 1] = b'\n';
            shift -= 1;
        } else {
            out.copy_within(begin..end, begin + shift);
        }
    }
    Ok(written)
}

/// As `readdir`, for the boot archive, whose entries are all files.
pub fn list_archive(archive: &[u8], start: u64, out: &mut [u8]) -> Result<usize, i64> {
    let mut len = 0;
    for (name, _) in cpio::entries(archive).skip(start as usize) {
        if len + name.len() + 1 > out.len() {
            return if len == 0 { Err(EINVAL) } else { Ok(len) };
        }
        out[len..][..name.len()].copy_from_slice(name);
        len += name.len() + 1;
        out[len - 1] = b'\n';
    }
    Ok(len)
}
