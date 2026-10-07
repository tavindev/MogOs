//! The file syscalls' work on MogFS and the boot archive, which the board runs with its file system. Paths are walked
//! here with one `lookup` per `/`-separated component, which must be a name (not `..`, `.` or empty), so a handle never
//! reaches above its directory through a path. The walk is bounded by the path's length: a crafted image can point an
//! entry at `ROOT` or an ancestor (a cycle), so nothing here recurses over the tree.

use mogfs::{Disk, Error, Fs, Inode, Kind};

use crate::cpio;
use crate::handle::Object;
use crate::syscall::{
    CREATE, EEXIST, EFBIG, EINVAL, EIO, EISDIR, ENAMETOOLONG, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY,
    TRUNC,
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
        Error::NotEmpty => ENOTEMPTY,
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

/// The file or directory at `path` under `dir`; with `CREATE` a missing file is made, with `TRUNC` the file is emptied
/// (`EISDIR` for a directory).
pub fn open<D: Disk>(fs: &mut Fs<D>, dir: Inode, path: &[u8], flags: u64) -> Result<Object, i64> {
    let (dir, name) = parent(fs, dir, path)?;
    let mut open = || {
        let inode = match flags & CREATE {
            0 => fs.lookup(dir, name)?,
            _ => fs.create(dir, name)?,
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

/// Removes the file or empty directory at `path` under `dir`.
pub fn unlink<D: Disk>(fs: &mut Fs<D>, dir: Inode, path: &[u8]) -> Result<(), i64> {
    let (dir, name) = parent(fs, dir, path)?;
    fs.unlink(dir, name).map_err(errno)
}

/// Moves the entry at `from` under `from_dir` to `to` under `to_dir`; `EEXIST` if `to` exists.
pub fn rename<D: Disk>(
    fs: &mut Fs<D>,
    (from_dir, from): (Inode, &[u8]),
    (to_dir, to): (Inode, &[u8]),
) -> Result<(), i64> {
    let (from_dir, from) = parent(fs, from_dir, from)?;
    let (to_dir, to) = parent(fs, to_dir, to)?;
    fs.rename(from_dir, from, to_dir, to).map_err(errno)
}

/// Writes `dir`'s entries from index `start` on into `out`, as many whole `name\n` (`name/\n` for a directory) as fit
/// up to `LIST`; returns the bytes written, `EINVAL` if the first entry does not fit. Resuming by index can skip or
/// repeat an entry if an unlink moved one between calls.
pub fn readdir<D: Disk>(
    fs: &mut Fs<D>,
    dir: Inode,
    start: u64,
    out: &mut [u8],
) -> Result<usize, i64> {
    let (mut len, mut count, mut full) = (0, 0, false);
    fs.readdir(dir, start as usize, |name, _, kind| {
        let slash = kind == Kind::Dir;
        let end = len + name.len() + slash as usize + 1;
        if count == LIST || end > out.len() {
            full = true;
            return true;
        }
        out[len..][..name.len()].copy_from_slice(name);
        if slash {
            out[end - 2] = b'/';
        }
        out[end - 1] = b'\n';
        (len, count) = (end, count + 1);
        false
    })
    .map_err(errno)?;
    match (full, len) {
        (true, 0) => Err(EINVAL),
        _ => Ok(len),
    }
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
