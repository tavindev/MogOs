//! Per-process handle tables. A handle value is `generation << 32 | index`; closing bumps the entry's generation,
//! so a closed handle's value never reaches whatever reuses its entry. An entry is retired once its generation
//! reaches 2^31, so handle values stay positive (never read as an error) and generations never wrap.

use crate::mutex::Mutex;
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

/// A kernel object a handle reaches.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Object {
    Console,
    /// The process in this scheduler slot with this generation; a later process in the slot has another.
    Process {
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
    Pipe(End),
    Mutex(Mutex),
}

#[derive(Clone, Copy)]
pub struct Handles([(u32, Option<(Object, Rights)>); MAX_HANDLES]);

impl Handles {
    pub const fn new() -> Self {
        Self([(0, None); MAX_HANDLES])
    }

    /// init's handles: 0 is the console (write, duplicate, transfer), 1 is the process itself (kill), in `slot` with
    /// `generation`, 2 is the boot archive (read, exec).
    pub fn init(slot: usize, generation: u64) -> Self {
        let mut handles = Self::new();
        handles.0[0].1 = Some((Object::Console, WRITE | DUPLICATE | TRANSFER));
        handles.0[1].1 = Some((Object::Process { slot, generation }, KILL));
        handles.0[2].1 = Some((Object::Archive, READ | EXEC));
        handles
    }

    /// The object `handle` reaches, if it holds every right in `need`.
    pub fn get(&self, handle: u64, need: Rights) -> Result<Object, i64> {
        let (object, rights) = self.entry(handle)?;
        if rights & need != need {
            return Err(EACCES);
        }
        Ok(object)
    }

    /// A new handle to `handle`'s object with `rights`, a subset of its own, and the object; needs the duplicate right.
    pub fn dup(&mut self, handle: u64, rights: Rights) -> Result<(u64, Object), i64> {
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
    /// values 0, 1, ... in order; returns both, so a caller that fails later keeps this table unchanged.
    pub fn split(&self, list: &[u64]) -> Result<(Self, Self), i64> {
        let (mut rest, mut moved) = (*self, Self::new());
        for &handle in list {
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
    pub fn close(&mut self, handle: u64) -> Result<Object, i64> {
        let (object, _) = self.entry(handle)?;
        let entry = &mut self.0[handle as u32 as usize];
        *entry = (entry.0 + 1, None);
        Ok(object)
    }

    /// The objects the open handles reach.
    pub fn objects(&self) -> impl Iterator<Item = Object> + '_ {
        self.0.iter().filter_map(|e| Some(e.1?.0))
    }

    fn entry(&self, handle: u64) -> Result<(Object, Rights), i64> {
        match self.0.get(handle as u32 as usize) {
            Some(&(generation, Some(entry))) if generation == (handle >> 32) as u32 => Ok(entry),
            _ => Err(EBADF),
        }
    }
}

impl Default for Handles {
    fn default() -> Self {
        Self::new()
    }
}
