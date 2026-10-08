//! Per-process handle tables. A handle value is `generation << 32 | index`; closing bumps the entry's generation,
//! so a closed handle's value never reaches whatever reuses its entry. An entry is retired once its generation
//! reaches 2^31, so handle values stay positive (never read as an error) and generations never wrap.
//!
//! A process's live table is a `Table`, read without a lock (a seqlock per entry) and written under the process's
//! lock; `Handles` is a plain copy for building one (a spawned child's) or staging a change before it is committed.

use core::hint::spin_loop;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use core::sync::atomic::{AtomicU64, fence};

use mogfs::Inode;

use crate::Clamp;
use crate::Process;
use crate::mutex::Mutex;
use crate::network::Sock;
use crate::pipe::End;
use crate::syscall::{EACCES, EBADF, EMFILE};

/// Handles per process.
pub const MAX_HANDLES: usize = 16;

const RETIRED: u32 = 1 << 31;

/// A set of rights, one bit each.
pub type Rights = u64;
pub const READ: Rights = 1 << 0;
pub const WRITE: Rights = 1 << 1;
pub const MAP: Rights = 1 << 2;
pub const DUPLICATE: Rights = 1 << 3;
pub const TRANSFER: Rights = 1 << 4;
pub const EXEC: Rights = 1 << 5;
pub const WAIT: Rights = 1 << 6;
pub const KILL: Rights = 1 << 7;
/// A NetStack handle's: open connections, and listen.
pub const CONNECT: Rights = 1 << 8;
pub const LISTEN: Rights = 1 << 9;

/// An init's boot archive handle: it spawns from it.
pub const INIT_ARCHIVE: Rights = READ | EXEC;
/// msh's (`test=shell`): it also hands the archive to `sh`, which spawns from it.
pub const SHELL_ARCHIVE: Rights = INIT_ARCHIVE | DUPLICATE | TRANSFER;

/// A kernel object a handle reaches.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Object {
    Console,
    /// The process at this index of the process table with this generation; a later process there has another.
    Process {
        index: usize,
        generation: u64,
    },
    /// The thread in this scheduler slot with this generation; a later thread in the slot has another.
    Thread {
        slot: usize,
        generation: u64,
    },
    /// The boot archive, a directory.
    Archive,
    /// A file in the boot archive, its data at these byte offsets.
    File {
        start: usize,
        end: usize,
    },
    /// A MogFS directory.
    Dir(Inode),
    /// A MogFS file.
    Node(Inode),
    Pipe(End),
    Mutex(Mutex),
    /// Network access: sockets, with the `CONNECT` and `LISTEN` rights it holds.
    NetStack,
    Socket(Sock),
}

/// A handle value from user space and its index, bounded under speculation by a `Clamp`.
#[derive(Clone, Copy)]
pub struct Handle {
    value: u64,
    index: usize,
}

impl Handle {
    /// The handle `value`, its index clamped by `C` behind its own barrier.
    pub fn new<C: Clamp>(value: u64) -> Self {
        let [index] = C::clamp([value as u32 as u64], [MAX_HANDLES as u64 - 1]);
        Self::clamped(value, index)
    }

    /// Whether the value names the entry at its index with `generation`. Checked after the load through the clamped
    /// index, so no branch picks between the clamped index and another.
    #[inline(always)]
    fn valid(self, generation: u32) -> bool {
        (self.value as u32 as usize) < MAX_HANDLES && (self.value >> 32) as u32 == generation
    }

    /// The handle `value` with `index`, its index as `dispatch` clamped it with the call's other values.
    #[inline]
    pub(crate) fn clamped(value: u64, index: u64) -> Self {
        Self {
            value,
            index: index as usize,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Handles([(u32, Option<(Object, Rights)>); MAX_HANDLES]);

impl Handles {
    pub const fn new() -> Self {
        Self([(0, None); MAX_HANDLES])
    }

    /// init's handles: 0 is the console (read, write, duplicate, transfer), 1 is the process itself (kill), at `index`
    /// with `generation`, 2 is the boot archive with `archive` (`INIT_ARCHIVE`, or `SHELL_ARCHIVE` for msh).
    pub fn init(index: usize, generation: u64, archive: Rights) -> Self {
        let mut handles = Self::new();
        handles.0[0].1 = Some((Object::Console, READ | WRITE | DUPLICATE | TRANSFER));
        handles.0[1].1 = Some((Object::Process { index, generation }, KILL));
        handles.0[2].1 = Some((Object::Archive, archive));
        handles
    }

    /// The object `handle` reaches, if it holds every right in `need`.
    #[inline(always)]
    pub fn get(&self, handle: Handle, need: Rights) -> Result<Object, i64> {
        let (object, rights) = self.entry(handle)?;
        if rights & need != need {
            return Err(EACCES);
        }
        Ok(object)
    }

    /// A new handle to `handle`'s object with `rights`, a subset of its own, and the object; needs the duplicate right.
    #[inline(always)]
    pub fn dup(&mut self, handle: Handle, rights: Rights) -> Result<(u64, Object), i64> {
        let (object, held) = self.entry(handle)?;
        if held & DUPLICATE == 0 || rights & !held != 0 {
            return Err(EACCES);
        }
        Ok((self.insert(object, rights)?, object))
    }

    /// A new handle to `object` with `rights`.
    pub fn insert(&mut self, object: Object, rights: Rights) -> Result<u64, i64> {
        let index = self
            .0
            .iter()
            .position(|e| e.1.is_none() && e.0 < RETIRED)
            .ok_or(EMFILE)?;
        self.0[index].1 = Some((object, rights));
        Ok((self.0[index].0 as u64) << 32 | index as u64)
    }

    /// Moves the handles in `list` (each needs the transfer right) out of a copy of this table into a new table, at
    /// values 0, 1, ... in order; returns both, so a caller that fails later keeps this table unchanged. `C` clamps
    /// each index, read from user memory after `dispatch`.
    pub fn split<C: Clamp>(&self, list: &[u64]) -> Result<(Self, Self), i64> {
        let (mut rest, mut moved) = (*self, Self::new());
        for &value in list {
            let handle = Handle::new::<C>(value);
            let (object, rights) = rest.entry(handle)?;
            if rights & TRANSFER == 0 {
                return Err(EACCES);
            }
            rest.close(handle)?;
            moved.insert(object, rights)?;
        }
        Ok((rest, moved))
    }

    /// Closes `handle`; returns the object it reached.
    #[inline(always)]
    pub fn close(&mut self, handle: Handle) -> Result<Object, i64> {
        let entry = &mut self.0[handle.index];
        match entry.1 {
            Some((object, _)) if handle.valid(entry.0) => {
                *entry = (entry.0 + 1, None);
                Ok(object)
            }
            _ => Err(EBADF),
        }
    }

    /// The objects the open handles reach.
    pub fn objects(&self) -> impl Iterator<Item = Object> + '_ {
        self.0.iter().filter_map(|e| Some(e.1?.0))
    }

    /// The object `handle` reaches and its rights.
    #[inline(always)]
    pub fn entry(&self, handle: Handle) -> Result<(Object, Rights), i64> {
        match self.0[handle.index] {
            (generation, Some(entry)) if handle.valid(generation) => Ok(entry),
            _ => Err(EBADF),
        }
    }
}

impl Default for Handles {
    fn default() -> Self {
        Self::new()
    }
}

/// Words an entry's object is stored in: its tag, rights and handle generation, then two words of fields.
type Words = [u64; 3];

/// The tags the typed lookups (`Table::mutex`, `Table::io`) test alone.
const CONSOLE: u8 = 1;
const DIR: u8 = 6;
const NODE: u8 = 7;
const PIPE: u8 = 8;
const MUTEX: u8 = 9;

/// A handle table read without a lock: each entry is its words behind a 64-bit sequence (no wrap), a seqlock. Writers
/// (every method taking `&mut Process`, the proof that the caller holds a process lock, this table's) store the
/// sequence odd, then the words, then the sequence even; a lookup that sees the sequence change or odd retries, so it
/// stores nothing and sibling threads' lookups share the line.
pub struct Table([Entry; MAX_HANDLES]);

struct Entry {
    sequence: AtomicU64,
    words: [AtomicU64; 3],
}

/// The entry a lookup read and its sequence, to recheck once the object's own lock is held: unchanged, a sibling's
/// `close` comes after the call, never during it. A call whose lookups are not rechecked may make several (the last
/// is kept); one that is makes one. An index past the table is no entry.
#[derive(Clone, Copy)]
pub struct Seen(usize, u64);

impl Default for Seen {
    fn default() -> Self {
        Self(MAX_HANDLES, 0)
    }
}

impl Table {
    pub const fn new() -> Self {
        Self(
            [const {
                Entry {
                    sequence: AtomicU64::new(0),
                    words: [const { AtomicU64::new(0) }; 3],
                }
            }; MAX_HANDLES],
        )
    }

    /// The object `handle` reaches and its rights, read without a lock; recorded in `seen`.
    #[inline(always)]
    pub fn entry(&self, handle: Handle, seen: &mut Seen) -> Result<(Object, Rights), i64> {
        let words = self.read(handle, seen)?;
        Ok(decode(words).1.expect("read checks the tag"))
    }

    /// The mutex `handle` reaches (`EACCES` for another object), decoding only a mutex's fields; recorded in `seen`.
    #[inline(always)]
    pub fn mutex(&self, handle: Handle, seen: &mut Seen) -> Result<Mutex, i64> {
        match self.read(handle, seen)? {
            [head, index, generation] if (head >> 32) as u8 == MUTEX => Ok(Mutex {
                index: index as u32,
                generation,
            }),
            _ => Err(EACCES),
        }
    }

    /// The object an `io` on `handle` reaches if it holds every right in `need`, decoding only the console, a pipe end,
    /// a file and a directory (`None` for another object); recorded in `seen`.
    #[inline(always)]
    pub fn io(&self, handle: Handle, need: Rights, seen: &mut Seen) -> Result<Option<Object>, i64> {
        let [head, a, b] = self.read(handle, seen)?;
        if (head >> 40) & need != need {
            return Err(EACCES);
        }
        Ok(match (head >> 32) as u8 {
            CONSOLE => Some(Object::Console),
            PIPE => Some(Object::Pipe(pipe_end(a, b))),
            NODE => Some(Object::Node(Inode::from_raw(a))),
            DIR => Some(Object::Dir(Inode::from_raw(a))),
            _ => None,
        })
    }

    /// The words of the entry `handle` names, read without a lock (`EBADF` if it is empty or another generation's);
    /// recorded in `seen`.
    #[inline(always)]
    fn read(&self, handle: Handle, seen: &mut Seen) -> Result<Words, i64> {
        let entry = &self.0[handle.index];
        let (sequence, words) = loop {
            let sequence = entry.sequence.load(Acquire);
            let words = entry.words.each_ref().map(|w| w.load(Relaxed));
            // `dmb ishld`: the words are read before the sequence is read again.
            fence(Acquire);
            if sequence & 1 == 0 && entry.sequence.load(Relaxed) == sequence {
                break (sequence, words);
            }
            spin_loop();
        };
        *seen = Seen(handle.index, sequence);
        if words[0] >> 32 & 0xff == 0 || !handle.valid(words[0] as u32) {
            return Err(EBADF);
        }
        Ok(words)
    }

    /// The object `handle` reaches, if it holds every right in `need`; recorded in `seen`.
    #[inline(always)]
    pub fn get(&self, handle: Handle, need: Rights, seen: &mut Seen) -> Result<Object, i64> {
        let (object, rights) = self.entry(handle, seen)?;
        if rights & need != need {
            return Err(EACCES);
        }
        Ok(object)
    }

    /// Whether the entry `seen` recorded is unchanged since.
    #[inline]
    pub fn unchanged(&self, seen: &Seen) -> bool {
        (self.0.get(seen.0)).is_none_or(|e| e.sequence.load(Acquire) == seen.1)
    }

    /// A copy of the table. Writers are serialized by the process lock, so this reads without retrying.
    pub fn snapshot(&self, _: &mut Process) -> Handles {
        Handles(core::array::from_fn(|i| decode(self.words(i))))
    }

    /// Writes every entry of `handles` (a `snapshot`, changed) that differs from the table.
    pub fn commit(&self, _: &mut Process, handles: &Handles) {
        for (i, new) in handles.0.iter().enumerate() {
            let new = encode(*new);
            if (self.0[i].words.iter())
                .zip(new)
                .any(|(w, n)| w.load(Relaxed) != n)
            {
                self.store(i, new);
            }
        }
    }

    /// The first `N` free entries, to `fill` once every step that can fail is done; `EMFILE` with fewer.
    pub fn reserve<const N: usize>(&self, _: &mut Process) -> Result<[usize; N], i64> {
        let (mut found, mut n) = ([0; N], 0);
        for (i, entry) in self.0.iter().enumerate() {
            if n == N {
                break;
            }
            if free(entry.words[0].load(Relaxed)) {
                (found[n], n) = (i, n + 1);
            }
        }
        if n < N {
            return Err(EMFILE);
        }
        Ok(found)
    }

    /// A handle to `object` with `rights` in the free entry `i` (from `reserve`); returns its value.
    pub fn fill(&self, _: &mut Process, i: usize, object: Object, rights: Rights) -> u64 {
        let generation = self.0[i].words[0].load(Relaxed) as u32;
        self.store(i, encode((generation, Some((object, rights)))));
        u64::from(generation) << 32 | i as u64
    }

    /// A new handle to `object` with `rights`.
    pub fn insert(
        &self,
        process: &mut Process,
        object: Object,
        rights: Rights,
    ) -> Result<u64, i64> {
        let [i] = self.reserve(process)?;
        Ok(self.fill(process, i, object, rights))
    }

    /// Closes `handle`; returns the object it reached.
    pub fn close(&self, _: &mut Process, handle: Handle) -> Result<Object, i64> {
        match decode(self.words(handle.index)) {
            (generation, Some((object, _))) if handle.valid(generation) => {
                self.store(handle.index, encode((generation + 1, None)));
                Ok(object)
            }
            _ => Err(EBADF),
        }
    }

    /// Entry `i`'s words, for a writer (the process lock holds every other writer off).
    fn words(&self, i: usize) -> Words {
        self.0[i].words.each_ref().map(|w| w.load(Relaxed))
    }

    /// Empties the table for the next process at its index; returns what it held.
    pub fn take(&self, _: &mut Process) -> Handles {
        Handles(core::array::from_fn(|i| {
            let words = self.words(i);
            if words.iter().any(|&w| w != 0) {
                self.store(i, [0; 3]);
            }
            decode(words)
        }))
    }

    fn store(&self, i: usize, words: Words) {
        let entry = &self.0[i];
        let sequence = entry.sequence.load(Relaxed);
        entry.sequence.store(sequence + 1, Relaxed);
        // `dmb ishst`: the odd sequence is seen before any of the new words.
        fence(Release);
        for (word, value) in entry.words.iter().zip(words) {
            word.store(value, Relaxed);
        }
        entry.sequence.store(sequence + 2, Release);
    }
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the entry whose first word is `head` is empty and not retired.
fn free(head: u64) -> bool {
    head >> 32 == 0 && (head as u32) < RETIRED
}

/// An entry as words: its handle generation (bits 0-31), object tag (32-39, 0 for none) and rights (40-63), then the
/// object's fields.
fn encode((generation, entry): (u32, Option<(Object, Rights)>)) -> Words {
    let Some((object, rights)) = entry else {
        return [u64::from(generation), 0, 0];
    };
    let (tag, a, b) = match object {
        Object::Console => (CONSOLE, 0, 0),
        Object::Process { index, generation } => (2, index as u64, generation),
        Object::Thread { slot, generation } => (3, slot as u64, generation),
        Object::Archive => (4, 0, 0),
        Object::File { start, end } => (5, start as u64, end as u64),
        Object::Dir(inode) => (DIR, inode.raw(), 0),
        Object::Node(inode) => (NODE, inode.raw(), 0),
        Object::Pipe(end) => (
            PIPE,
            u64::from(end.index) | u64::from(end.write) << 32,
            end.generation,
        ),
        Object::Mutex(mutex) => (MUTEX, mutex.index.into(), mutex.generation),
        Object::NetStack => (10, 0, 0),
        Object::Socket(sock) => (11, sock.index.into(), sock.generation),
    };
    let tag = u64::from(tag);
    [u64::from(generation) | tag << 32 | rights << 40, a, b]
}

#[inline(always)]
fn pipe_end(a: u64, generation: u64) -> End {
    End {
        index: a as u32,
        write: a >> 32 != 0,
        generation,
    }
}

#[inline(always)]
fn decode([head, a, b]: Words) -> (u32, Option<(Object, Rights)>) {
    let object = match (head >> 32) as u8 {
        CONSOLE => Object::Console,
        2 => Object::Process {
            index: a as usize,
            generation: b,
        },
        3 => Object::Thread {
            slot: a as usize,
            generation: b,
        },
        4 => Object::Archive,
        5 => Object::File {
            start: a as usize,
            end: b as usize,
        },
        DIR => Object::Dir(Inode::from_raw(a)),
        NODE => Object::Node(Inode::from_raw(a)),
        PIPE => Object::Pipe(pipe_end(a, b)),
        MUTEX => Object::Mutex(Mutex {
            index: a as u32,
            generation: b,
        }),
        10 => Object::NetStack,
        11 => Object::Socket(Sock {
            index: a as u32,
            generation: b,
        }),
        _ => return (head as u32, None),
    };
    (head as u32, Some((object, head >> 40)))
}
