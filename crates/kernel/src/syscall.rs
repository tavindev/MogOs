//! Native syscalls: `x8` = number, `x0`-`x6` = arguments, `x0` = result (negative = error), `svc #0`.

use core::ops::Range;

use mogfs::Inode;

use crate::Clamp;
use crate::handle::{
    CONNECT, EXEC, Handles, KILL as KILL_RIGHT, LISTEN, MAX_HANDLES, Object, READ, Rights, WRITE,
};
use crate::mutex::Mutex;
use crate::network::{OP_ACCEPT, OP_CONNECT, OP_RECEIVE, OP_SEND, Sock};
use crate::pipe::End;

/// `exit(code)`: ends the calling process, every thread; `wait` reports the low 8 bits of `code`.
const EXIT: u64 = 0;
/// `io_submit_wait(handle, op, ptr, len, offset)`: submits I/O on `handle` and waits for it to complete; returns the
/// bytes moved. `op` is `IO_READ` (into `ptr`, read right) or `IO_WRITE` (from `ptr`, write right). libc's `pread` and
/// `pwrite`; a file is read or written at `offset` (a read at or past its end returns 0), which the console and pipes
/// ignore. A `len` over `MAX_BUFFER` moves at most `MAX_BUFFER` bytes (a short read or write). Reading the console
/// waits for a line (`console::Line`); with two readers, whichever runs first gets it.
const IO: u64 = 1;
/// `dup(handle, rights)`: returns a new handle to the same object with `rights`, a subset of `handle`'s (duplicate right).
const DUP: u64 = 2;
/// `close(handle)`: returns 0.
const CLOSE: u64 = 3;
/// `map(len)`: maps `len` bytes (rounded up to pages) of zeroed read-write memory at an address the kernel picks,
/// charged to the caller's budget; returns the address. The kernel picking the address leaves nothing to overlap, and
/// it is what musl's `mmap(NULL, ...)` needs (its malloc falls back from `brk` to `mmap`).
const MAP: u64 = 4;
/// `open(dir, path_ptr, path_len, flags)`: returns a handle with `dir`'s rights to the file or directory at `path`
/// under the directory `dir` (read right). `flags`: `CREATE` makes a missing file, `TRUNC` empties the file; either
/// needs the write right, and on the boot archive is `EROFS`. Paths resolve only below a directory handle: each
/// `/`-separated component must be a name, so `..`, `.`, an empty component (`/x`, `a//b`) is `EINVAL`; more than
/// `file::MAX_DEPTH` (16) components is `ENAMETOOLONG`.
const OPEN: u64 = 5;
/// `spawn(exe, handles_ptr, handles_len, budget, priority, args_ptr, args_len)`: starts the executable `exe` (exec
/// right) as a new process at `priority`, capped at the caller's own (so no process escalates), moving it the
/// `handles_len` handles at `handles_ptr` (transfer right; values 0, 1, ... in the child) and `budget` frames of the
/// caller's budget; returns a handle to the process (wait, kill). On failure nothing moves. `args` holds the child's
/// arguments, each ending in a NUL (`EINVAL` otherwise), at most `MAX_ARGS` and `MAX_BUFFER` bytes (`E2BIG`); the
/// kernel copies them to the top of the child's stack, on a page charged to the child's budget below which the stack
/// gets its usual page, and the child starts with x0 = their count, x1 = their address, x2 = their length.
const SPAWN: u64 = 6;
/// `pipe()`: returns a handle to a new pipe's read end (read, duplicate, transfer), and in `x1` one to its write end
/// (write, duplicate, transfer). Its one-page buffer is charged to the caller's budget until the last handle to it
/// closes. Reading it empty waits for data, or returns 0 once no write end is left; a write waits until all of it fits
/// (so every write is atomic, as `MAX_BUFFER` is the buffer size), or fails with `EPIPE` once no read end is left.
const PIPE: u64 = 7;
/// `wait(handle)`: waits for the process (wait right) to exit; returns its exit code (`KILLED` if a fault killed it)
/// and moves what is left of its budget back to the caller. An exited process keeps its index until waited for or its
/// handle closes; after that, `EBADF` once a newer process took the index. On a thread handle it is a join: waits for
/// the thread to end and returns its exit code, with the same rules for its slot.
const WAIT: u64 = 8;
/// `mutex()`: returns a handle (duplicate, transfer) to a new unlocked mutex; the table slot is fixed, so nothing is
/// charged.
const MUTEX: u64 = 9;
/// `lock(mutex)`: waits until the mutex is free, then makes the caller its owner; meanwhile the owner runs at least
/// at the caller's priority. `EDEADLK` if the caller owns it. Lock and unlock need no right.
const LOCK: u64 = 10;
/// `unlock(mutex)`: frees the mutex, which the caller must own (`EPERM`). An exiting owner frees what it holds.
const UNLOCK: u64 = 11;
/// `kill(handle)`: ends the process (kill right) as a fault would, or the thread (its process ends with its last
/// thread); `wait` reports `KILLED`. 0 if it already ended.
const KILL: u64 = 12;
/// `mkdir(dir, path_ptr, path_len)`: makes a directory at `path` under `dir` (write right), resolved as by `open`;
/// returns 0. `EROFS` on the boot archive.
const MKDIR: u64 = 13;
/// `readdir(dir, ptr, len, start)`: fills `ptr` with whole `name\n` entries (`name/\n` for a directory) of `dir` (read
/// right) from entry `start` on; returns the bytes written, 0 past the last entry. The caller advances `start` by the
/// newlines it got; an unlink between calls moves an entry, so a resumed listing can skip or repeat one. `EINVAL` if
/// the first entry does not fit in `len`.
const READDIR: u64 = 14;
/// `sync(handle)`: makes every change to the file system the directory or file `handle` (no right needed: it changes
/// nothing a handle reaches) is on durable, atomically; it holds the core for its writes and two flushes. `EIO` means unknown: the changes may or may not be durable.
const SYNC: u64 = 15;
/// `unlink(dir, path_ptr, path_len)`: removes the file or empty directory (`ENOTEMPTY` otherwise) at `path` under `dir`
/// (write right), resolved as by `open`; returns 0. `EBUSY` while any process holds a handle to it, so a freed inode
/// is never reached through an old handle. `EROFS` on the boot archive.
const UNLINK: u64 = 16;
/// `rename(from_dir, from_ptr, from_len, to_dir, to_ptr, to_len)`: moves the entry at the path `from` under `from_dir`
/// to the path `to` under `to_dir` (both write right), resolved as by `open`; returns 0. `EEXIST` if `to` exists,
/// `EINVAL` if a directory would move below itself, `EROFS` on the boot archive.
const RENAME: u64 = 17;
/// `thread(entry, stack, tls, arg)`: starts a thread of the caller's process at `entry` with SP = `stack`, TPIDR_EL0 =
/// `tls` and x0 = `arg`, at the caller's own priority; its kernel stack is charged to the process's budget (`ENOMEM`),
/// its user stack is the caller's own memory. Returns a thread handle (wait, kill, duplicate, transfer); `EAGAIN` if
/// no slot is free. On failure nothing changes.
const THREAD: u64 = 18;
/// `thread_exit(code)`: ends the calling thread; its process ends with its last thread, with this code. A join
/// reports the low 8 bits of `code`.
const THREAD_EXIT: u64 = 19;
/// `socket(net)`: returns a handle (read, write, duplicate, transfer) to a new TCP socket on the NetStack `net`, which
/// needs `CONNECT` or `LISTEN` and passes the socket those of the two it holds. Its buffers are charged to the
/// caller's budget until the last handle closes (`ENOBUFS`); `ENFILE` when the socket table is full.
const SOCKET: u64 = 20;
/// `bind(socket, port, ip)`: sets the local port `listen` and `connect` use (0, the default, picks an ephemeral one
/// for `connect`; write right) and, for `listen`, the address: 0 listens on every interface, 127.0.0.1 (a big-endian
/// `u32`) on loopback only, anything else is `EADDRNOTAVAIL`; returns 0. `EINVAL` once listening or connected.
const BIND: u64 = 21;
/// `listen(socket)`: listens on the bound port (`EINVAL` without one) on every interface (write right and `LISTEN`, else
/// `EACCES`; `EADDRINUSE`); returns 0.
const LISTEN_CALL: u64 = 22;
/// `io_submit(socket, op, ptr, len, tag)`: starts `op` and returns 0 at once; `io_wait` reports its result with `tag`.
/// `OP_RECEIVE` reads at most `len` bytes into `ptr` (read right; 0 is the end of the stream), `OP_SEND` queues up to
/// `len` bytes from `ptr` (write right) and reports how many, `OP_ACCEPT` (read right) reports a handle to the next
/// connection on a listening socket (its buffers charged to the caller's budget), `OP_CONNECT` (write right and
/// `CONNECT`) opens a connection to the IPv4 address `ptr` (a big-endian `u32`), port `len`, and reports 0 once it is
/// established. A buffer must lie in user space and stays the caller's until the result is reported; `len` over
/// `MAX_BUFFER` moves at most `MAX_BUFFER`. A socket takes one op that receives (receive, accept, connect) and one send
/// at a time (`EBUSY`). Closing the last handle drops its ops unreported.
const IO_SUBMIT: u64 = 23;
/// `io_wait()`: waits until an op the caller submitted finishes; returns its result, and its tag in x1. `EINVAL` if
/// none is in flight.
const IO_WAIT: u64 = 24;
/// `shutdown(socket)`: ends the send side (write right): a FIN follows the queued data; returns 0. `ENOTCONN` unless
/// connected.
const SHUTDOWN: u64 = 25;

/// Most arguments a `spawn` passes.
pub const MAX_ARGS: usize = 32;

/// The exit code `wait` reports for a process a fault killed: outside `exit`'s 0..=255.
pub const KILLED: u64 = 256;

pub const IO_READ: u64 = 0;
pub const IO_WRITE: u64 = 1;

/// `open` flags.
pub const CREATE: u64 = 1 << 0;
pub const TRUNC: u64 = 1 << 1;

// Errors are negated musl errno values.

/// Unlocking a mutex the caller does not own.
pub const EPERM: i64 = -1;
/// No such file in the directory.
pub const ENOENT: i64 = -2;
/// The disk failed a request, or the file system is corrupt.
pub const EIO: i64 = -5;
/// Over `spawn`'s argument limits.
pub const E2BIG: i64 = -7;
/// Not a valid executable.
pub const ENOEXEC: i64 = -8;

/// Bad, closed or stale handle.
pub const EBADF: i64 = -9;
/// No free process index or thread slot.
pub const EAGAIN: i64 = -11;
/// Over the memory budget, or out of frames.
pub const ENOMEM: i64 = -12;
/// The handle lacks a right the call needs.
pub const EACCES: i64 = -13;
/// Bad address: outside user space, unmapped, or (except for `io_submit_wait`) longer than `MAX_BUFFER`.
pub const EFAULT: i64 = -14;
/// Unlinking a file or directory that a handle reaches.
pub const EBUSY: i64 = -16;
/// The name exists.
pub const EEXIST: i64 = -17;
/// A path component, or a handle a call needs to be a directory, is a file.
pub const ENOTDIR: i64 = -20;
/// Reading, writing or truncating a directory.
pub const EISDIR: i64 = -21;
/// Invalid argument: a `map` of zero bytes or more than `MAX_MAP`, a `spawn` of more than `MAX_HANDLES` handles, an
/// unknown I/O op or `open` flag, a path component that is not a name.
pub const EINVAL: i64 = -22;
/// The pipe or mutex table is full.
pub const ENFILE: i64 = -23;
/// The handle table is full.
pub const EMFILE: i64 = -24;
/// Past the largest file or directory.
pub const EFBIG: i64 = -27;
/// The file system is full.
pub const ENOSPC: i64 = -28;
/// Changing the boot archive.
pub const EROFS: i64 = -30;
/// Writing a pipe with no read end left.
pub const EPIPE: i64 = -32;
/// Locking a mutex the caller owns.
pub const EDEADLK: i64 = -35;
/// A path of more than `file::MAX_DEPTH` components.
pub const ENAMETOOLONG: i64 = -36;
/// No such syscall.
const ENOSYS: i64 = -38;
/// Unlinking a directory that has entries.
pub const ENOTEMPTY: i64 = -39;
/// Listening on a port that is taken.
pub const EADDRINUSE: i64 = -98;
/// Binding to an address other than any or 127.0.0.1.
pub const EADDRNOTAVAIL: i64 = -99;
/// 127.0.0.1, as `bind` takes it.
const LOCALHOST: u64 = 0x7f00_0001;
/// Connecting off the loopback network without a NIC, or with no route.
pub const ENETUNREACH: i64 = -101;
/// The peer reset the connection.
pub const ECONNRESET: i64 = -104;
/// A socket's buffers are over the budget.
pub const ENOBUFS: i64 = -105;
/// Connecting a socket that is connected or listening.
pub const EISCONN: i64 = -106;
/// Using a socket that is not connected.
pub const ENOTCONN: i64 = -107;
/// The peer stopped answering.
pub const ETIMEDOUT: i64 = -110;
/// The peer refused the connection.
pub const ECONNREFUSED: i64 = -111;
/// An ICMP error answered the connection request.
pub const EHOSTUNREACH: i64 = -113;

/// User virtual addresses: 4 GiB up to the 39-bit VA limit.
const USER: Range<u64> = 1 << 32..1 << 39;
/// Longest user buffer a syscall reads or writes (I/O data, `open` name, `spawn` handles), so its IRQs-masked work
/// stays bounded.
pub const MAX_BUFFER: u64 = 4096;
const _: () = assert!(
    MAX_BUFFER as usize <= crate::pipe::SIZE,
    "a longer pipe write would never fit"
);
/// Longest `map` (16 pages), so its IRQs-masked zeroing stays bounded.
const MAX_MAP: u64 = 16 * 4096;

pub enum Call {
    /// End the caller's process with this code.
    Exit(u64),
    /// End the calling thread with this code.
    ThreadExit(u64),
    /// Start a thread of the caller's process.
    Thread {
        entry: u64,
        stack: u64,
        tls: u64,
        arg: u64,
    },
    /// Write to the console; `ptr..ptr + len` lies in `USER` unless empty, but may be unmapped.
    Write {
        ptr: u64,
        len: usize,
    },
    /// Read a line from the console into `ptr..ptr + len`, as for `Write`.
    Read {
        ptr: u64,
        len: usize,
    },
    /// Read from (or, for a write end, write to) the pipe `end` reaches; `ptr..ptr + len` as for `Write`.
    Pipe {
        end: End,
        ptr: u64,
        len: usize,
    },
    /// Create a pipe.
    NewPipe,
    /// Wait for the process at `index` with `generation`.
    Wait {
        index: usize,
        generation: u64,
    },
    /// Wait for the thread in `slot` with `generation`.
    Join {
        slot: usize,
        generation: u64,
    },
    /// `handle` is a new handle to `object`.
    Dup {
        handle: u64,
        object: Object,
    },
    /// A handle to this object was closed.
    Close(Object),
    /// Map this many pages into the caller's address space.
    Map {
        pages: usize,
    },
    /// Read (or write) the file `inode` at `offset` into (from) `ptr..ptr + len`, as for `Write`.
    File {
        inode: Inode,
        write: bool,
        offset: u64,
        ptr: u64,
        len: usize,
    },
    /// Open the path at `ptr..ptr + len` (in `USER` unless empty, maybe unmapped) under `dir` (the boot archive, or a
    /// directory, then with `flags`) with `rights`.
    Open {
        dir: Object,
        ptr: u64,
        len: usize,
        flags: u64,
        rights: u64,
    },
    /// Make a directory at the path at `ptr..ptr + len` (as for `Open`) under `dir`.
    Mkdir {
        dir: Inode,
        ptr: u64,
        len: usize,
    },
    /// List `dir` (the boot archive or a directory) from entry `start` into `ptr..ptr + len`, as for `Read`.
    Readdir {
        dir: Object,
        ptr: u64,
        len: usize,
        start: u64,
    },
    /// Commit the file system.
    Sync,
    /// Unlink the path at `ptr..ptr + len` (as for `Open`) under `dir`.
    Unlink {
        dir: Inode,
        ptr: u64,
        len: usize,
    },
    /// Rename the path `from` to the path `to`, each a directory and a buffer as for `Unlink`.
    Rename {
        from: (Inode, u64, usize),
        to: (Inode, u64, usize),
    },
    /// Spawn the boot archive's file `file`, moving the `len` handles at `ptr` (in `USER` unless empty, maybe unmapped)
    /// and `budget`, with the arguments at `args..args + args_len` (as for `ptr`). The small fields keep `Call` at 56 bytes.
    Spawn {
        file: Range<usize>,
        ptr: u64,
        len: u8,
        budget: usize,
        priority: u8,
        args: u64,
        args_len: u16,
    },
    /// Create a mutex.
    NewMutex,
    Lock(Mutex),
    Unlock(Mutex),
    /// Kill the process at `index` with `generation`.
    Kill {
        index: usize,
        generation: u64,
    },
    /// Kill the thread in `slot` with `generation`.
    KillThread {
        slot: usize,
        generation: u64,
    },
    /// A socket call: one variant, so the board handles them all out of its hot path.
    Net(NetCall),
}

pub enum NetCall {
    /// Create a socket with these NetStack rights.
    Socket(Rights),
    Bind {
        sock: Sock,
        port: u16,
        loopback: bool,
    },
    Listen(Sock),
    /// Submit `op` on `sock`; for a receive or send `ptr..ptr + len` is in `USER` unless empty, but may be unmapped.
    /// `rights` are the handle's: an accepted connection's handle gets no more.
    Submit {
        sock: Sock,
        op: u8,
        rights: u16,
        ptr: u64,
        len: u32,
        tag: u64,
    },
    IoWait,
    Shutdown(Sock),
}

const _: () = assert!(size_of::<Call>() == 56, "every syscall returns a Call");

/// Runs syscall `nr` with arguments `args` (`x0`-`x6`) against the caller's `handles`, leaving the board the parts
/// that touch hardware or tasks; `Err` holds the result to return.
pub fn dispatch<C: Clamp>(nr: u64, args: &[u64; 7], handles: &mut Handles<C>) -> Result<Call, i64> {
    if nr > SHUTDOWN {
        return Err(ENOSYS);
    }
    let nr = C::clamp(nr as usize, SHUTDOWN as usize + 1) as u64;
    match nr {
        EXIT => Ok(Call::Exit(args[0] & 0xff)),
        THREAD_EXIT => Ok(Call::ThreadExit(args[0] & 0xff)),
        THREAD => Ok(Call::Thread {
            entry: args[0],
            stack: args[1],
            tls: args[2],
            arg: args[3],
        }),
        IO => {
            let (handle, op, ptr, len) = (args[0], args[1], args[2], args[3]);
            let need = match op {
                IO_READ => READ,
                IO_WRITE => WRITE,
                _ => return Err(EINVAL),
            };
            let object = handles.get(handle, need)?;
            let len = len.min(MAX_BUFFER);
            user_buffer(ptr, len)?;
            let len = len as usize;
            match object {
                Object::Console if op == IO_WRITE => Ok(Call::Write { ptr, len }),
                Object::Console => Ok(Call::Read { ptr, len }),
                Object::Pipe(end) if end.write == (op == IO_WRITE) => {
                    Ok(Call::Pipe { end, ptr, len })
                }
                Object::Node(inode) => Ok(Call::File {
                    inode,
                    write: op == IO_WRITE,
                    offset: args[4],
                    ptr,
                    len,
                }),
                Object::Dir(_) => Err(EISDIR),
                _ => Err(EACCES),
            }
        }
        DUP => {
            let (handle, object) = handles.dup(args[0], args[1])?;
            Ok(Call::Dup { handle, object })
        }
        CLOSE => handles.close(args[0]).map(Call::Close),
        MAP => match args[0] {
            0 => Err(EINVAL),
            len if len > MAX_MAP => Err(EINVAL),
            len => Ok(Call::Map {
                pages: len.div_ceil(4096) as usize,
            }),
        },
        OPEN => {
            let (ptr, len, flags) = (args[1], args[2], args[3]);
            if flags & !(CREATE | TRUNC) != 0 {
                return Err(EINVAL);
            }
            let (dir, rights) = handles.entry(args[0])?;
            match dir {
                Object::Archive if flags != 0 => return Err(EROFS),
                Object::Archive | Object::Dir(_) => {}
                _ => return Err(ENOTDIR),
            }
            if rights & READ == 0 || (flags != 0 && rights & WRITE == 0) {
                return Err(EACCES);
            }
            user_buffer(ptr, len)?;
            Ok(Call::Open {
                dir,
                ptr,
                len: len as usize,
                flags,
                rights,
            })
        }
        SPAWN => {
            let (exe, ptr, len, budget) = (args[0], args[1], args[2], args[3]);
            let Object::File { start, end } = handles.get(exe, EXEC)? else {
                return Err(EACCES);
            };
            if len > MAX_HANDLES as u64 {
                return Err(EINVAL);
            }
            user_buffer(ptr, len * 8)?;
            let (argv, argv_len) = (args[5], args[6]);
            if argv_len > MAX_BUFFER {
                return Err(E2BIG);
            }
            user_buffer(argv, argv_len)?;
            Ok(Call::Spawn {
                file: start..end,
                ptr,
                len: len as u8,
                budget: budget as usize,
                priority: args[4].min(u8::MAX.into()) as u8,
                args: argv,
                args_len: argv_len as u16,
            })
        }
        PIPE => Ok(Call::NewPipe),
        WAIT => match handles.get(args[0], crate::handle::WAIT)? {
            Object::Process { index, generation } => Ok(Call::Wait { index, generation }),
            Object::Thread { slot, generation } => Ok(Call::Join { slot, generation }),
            _ => Err(EACCES),
        },
        MUTEX => Ok(Call::NewMutex),
        LOCK | UNLOCK => match handles.get(args[0], 0)? {
            Object::Mutex(mutex) if nr == LOCK => Ok(Call::Lock(mutex)),
            Object::Mutex(mutex) => Ok(Call::Unlock(mutex)),
            _ => Err(EACCES),
        },
        KILL => match handles.get(args[0], KILL_RIGHT)? {
            Object::Process { index, generation } => Ok(Call::Kill { index, generation }),
            Object::Thread { slot, generation } => Ok(Call::KillThread { slot, generation }),
            _ => Err(EACCES),
        },
        MKDIR | UNLINK => {
            let (dir, ptr, len) = path(handles, args[0], args[1], args[2])?;
            Ok(match nr {
                MKDIR => Call::Mkdir { dir, ptr, len },
                _ => Call::Unlink { dir, ptr, len },
            })
        }
        RENAME => Ok(Call::Rename {
            from: path(handles, args[0], args[1], args[2])?,
            to: path(handles, args[3], args[4], args[5])?,
        }),
        READDIR => {
            let (ptr, len) = (args[1], args[2]);
            let dir = handles.get(args[0], READ)?;
            let (Object::Archive | Object::Dir(_)) = dir else {
                return Err(ENOTDIR);
            };
            user_buffer(ptr, len)?;
            Ok(Call::Readdir {
                dir,
                ptr,
                len: len as usize,
                start: args[3],
            })
        }
        SYNC => match handles.get(args[0], 0)? {
            Object::Dir(_) | Object::Node(_) => Ok(Call::Sync),
            _ => Err(ENOTDIR),
        },
        SOCKET => match handles.entry(args[0])? {
            (Object::NetStack, rights) if rights & (CONNECT | LISTEN) != 0 => {
                Ok(Call::Net(NetCall::Socket(rights)))
            }
            _ => Err(EACCES),
        },
        BIND => {
            let sock = socket(handles, args[0], WRITE)?;
            let port = u16::try_from(args[1]).map_err(|_| EINVAL)?;
            let loopback = match args[2] {
                0 => false,
                LOCALHOST => true,
                _ => return Err(EADDRNOTAVAIL),
            };
            Ok(Call::Net(NetCall::Bind {
                sock,
                port,
                loopback,
            }))
        }
        LISTEN_CALL => Ok(Call::Net(NetCall::Listen(socket(handles, args[0], WRITE)?))),
        IO_SUBMIT => {
            let (op, ptr, len, tag) = (args[1], args[2], args[3], args[4]);
            let need = match op {
                OP_RECEIVE | OP_ACCEPT => READ,
                OP_SEND | OP_CONNECT => WRITE,
                _ => return Err(EINVAL),
            };
            let sock = socket(handles, args[0], need)?;
            let (_, rights) = handles.entry(args[0])?;
            let len = match op {
                OP_RECEIVE | OP_SEND => {
                    let len = len.min(MAX_BUFFER);
                    user_buffer(ptr, len)?;
                    len
                }
                _ => len.min(u32::MAX.into()),
            };
            Ok(Call::Net(NetCall::Submit {
                sock,
                op: op as u8,
                rights: rights as u16,
                ptr,
                len: len as u32,
                tag,
            }))
        }
        IO_WAIT => Ok(Call::Net(NetCall::IoWait)),
        SHUTDOWN => Ok(Call::Net(NetCall::Shutdown(socket(
            handles, args[0], WRITE,
        )?))),
        _ => Err(ENOSYS),
    }
}

/// The socket `handle` reaches, if it holds the rights in `need`.
fn socket<C: Clamp>(handles: &Handles<C>, handle: u64, need: Rights) -> Result<Sock, i64> {
    match handles.get(handle, need)? {
        Object::Socket(sock) => Ok(sock),
        _ => Err(EACCES),
    }
}

/// The directory `handle` (write right) and the path buffer `ptr..ptr + len` a call that changes it names.
fn path<C: Clamp>(
    handles: &Handles<C>,
    handle: u64,
    ptr: u64,
    len: u64,
) -> Result<(Inode, u64, usize), i64> {
    let dir = match handles.entry(handle)? {
        (Object::Archive, _) => return Err(EROFS),
        (Object::Dir(_), rights) if rights & WRITE == 0 => return Err(EACCES),
        (Object::Dir(dir), _) => dir,
        _ => return Err(ENOTDIR),
    };
    user_buffer(ptr, len)?;
    Ok((dir, ptr, len as usize))
}

/// The number of arguments in `args`, each ending in a NUL; `E2BIG` over `MAX_ARGS` or `MAX_BUFFER` bytes, `EINVAL`
/// if the last one has no NUL.
pub fn argc(args: &[u8]) -> Result<usize, i64> {
    let count = args.iter().filter(|&&b| b == 0).count();
    match args.last() {
        _ if count > MAX_ARGS || args.len() > MAX_BUFFER as usize => Err(E2BIG),
        Some(&last) if last != 0 => Err(EINVAL),
        _ => Ok(count),
    }
}

/// `EFAULT` unless `len` is 0 (any `ptr`, as Rust passes empty slices) or `ptr..ptr + len` lies in `USER` and `len`
/// is at most `MAX_BUFFER`.
#[inline]
fn user_buffer(ptr: u64, len: u64) -> Result<(), i64> {
    if len == 0 {
        return Ok(());
    }
    let end = ptr.checked_add(len).ok_or(EFAULT)?;
    if len > MAX_BUFFER || ptr < USER.start || end > USER.end {
        return Err(EFAULT);
    }
    Ok(())
}
