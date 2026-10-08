//! The file syscalls' work on MogFS and the boot archive, which the board runs with its file system. Paths are walked
//! here with one `lookup` per `/`-separated component, which must be a name (not `..`, `.` or empty), so a handle never
//! reaches above its directory through a path. The walk is bounded by the path's length: a crafted image can point an
//! entry at `ROOT` or an ancestor (a cycle), so nothing here recurses over the tree.

use mogfs::{Disk, Error, Fs, Inode, Kind, ROOT};

use crate::cpio;
use crate::handle::Object;
use crate::syscall::{
    CREATE, EBUSY, EEXIST, EFBIG, EINVAL, EIO, EISDIR, ENAMETOOLONG, ENOENT, ENOSPC, ENOTDIR,
    ENOTEMPTY, TRUNC,
};

/// Entries one `readdir` call lists at most.
const LIST: usize = 64;
/// Components a path has at most, which bounds a walk's lookups (each a descent of at most 8 tree levels) with IRQs
/// masked.
pub const MAX_DEPTH: usize = 16;

pub fn errno(error: Error) -> i64 {
    match error {
        Error::NotFound => ENOENT,
        Error::Exists => EEXIST,
        Error::NotDir => ENOTDIR,
        Error::IsDir => EISDIR,
        Error::InvalidName => EINVAL,
        Error::TooBig => EFBIG,
        Error::NoSpace | Error::Collision => ENOSPC,
        Error::NotEmpty => ENOTEMPTY,
        Error::Busy => EBUSY,
        Error::Io | Error::Corrupt | Error::Unsupported => EIO,
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

/// Removes the file or empty directory at `path` under `dir`; `EBUSY` if `held` says a handle reaches it.
pub fn unlink<D: Disk>(
    fs: &mut Fs<D>,
    dir: Inode,
    path: &[u8],
    held: impl Fn(Inode) -> bool,
) -> Result<(), i64> {
    let (dir, name) = parent(fs, dir, path)?;
    fs.unlink(dir, name, held).map_err(errno)
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

/// Writes `dir`'s entries from `cursor` on (0: the first) into `out`, as many whole `name\n` (`name/\n` for a directory)
/// as fit up to `LIST`; returns the bytes written and the cursor to resume from (`u64::MAX` past the end), `EINVAL` if
/// the first entry does not fit.
pub fn readdir<D: Disk>(
    fs: &mut Fs<D>,
    dir: Inode,
    cursor: u64,
    out: &mut [u8],
) -> Result<(usize, u64), i64> {
    let (mut len, mut count) = (0, 0);
    let next = fs
        .readdir(dir, cursor, |name, _, kind| {
            let slash = kind == Kind::Dir;
            let end = len + name.len() + slash as usize + 1;
            if count == LIST || end > out.len() {
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
    match (next, len) {
        (u64::MAX, _) | (_, 1..) => Ok((len, next)),
        _ => Err(EINVAL),
    }
}

/// As `readdir`, for the boot archive, whose entries are all files; its cursor is an entry index.
pub fn list_archive(archive: &[u8], cursor: u64, out: &mut [u8]) -> Result<(usize, u64), i64> {
    let mut len = 0;
    for (i, (name, _)) in cpio::entries(archive).enumerate().skip(cursor as usize) {
        if len + name.len() + 1 > out.len() {
            return if len == 0 {
                Err(EINVAL)
            } else {
                Ok((len, i as u64))
            };
        }
        out[len..][..name.len()].copy_from_slice(name);
        len += name.len() + 1;
        out[len - 1] = b'\n';
    }
    Ok((len, u64::MAX))
}

/// Open handles per inode, counted as handles to a `Dir` or `Node` open and close, so `unlink` asks it instead of
/// scanning every process's table (each behind its own lock). `N` bounds the handles open at once (every process's
/// table), so it never runs out of entries and allocates nothing.
pub struct Opens<const N: usize> {
    counts: [(Inode, u32); N],
    len: usize,
}

impl<const N: usize> Opens<N> {
    pub const fn new() -> Self {
        Self {
            counts: [(ROOT, 0); N],
            len: 0,
        }
    }

    /// A handle to `object` opened; one to anything but a `Dir` or `Node` is not counted.
    pub fn open(&mut self, object: Object) {
        let (Object::Dir(inode) | Object::Node(inode)) = object else {
            return;
        };
        match self.counts[..self.len]
            .iter_mut()
            .find(|(i, _)| *i == inode)
        {
            Some((_, count)) => *count += 1,
            None => {
                self.counts[self.len] = (inode, 1);
                self.len += 1;
            }
        }
    }

    /// A handle to `object` closed.
    pub fn close(&mut self, object: Object) {
        let (Object::Dir(inode) | Object::Node(inode)) = object else {
            return;
        };
        let at = self.counts[..self.len]
            .iter()
            .position(|(i, _)| *i == inode);
        let at = at.expect("a close without its open");
        self.counts[at].1 -= 1;
        if self.counts[at].1 == 0 {
            self.len -= 1;
            self.counts[at] = self.counts[self.len];
        }
    }

    /// Whether a handle reaches `inode`.
    pub fn held(&self, inode: Inode) -> bool {
        self.counts[..self.len].iter().any(|(i, _)| *i == inode)
    }
}

impl<const N: usize> Default for Opens<N> {
    fn default() -> Self {
        Self::new()
    }
}
