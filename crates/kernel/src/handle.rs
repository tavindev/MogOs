//! Per-process handle tables. A handle value is `generation << 32 | index`; closing bumps the entry's generation,
//! so a closed handle's value never reaches whatever reuses its entry. An entry is retired once its generation
//! reaches 2^31, so handle values stay positive (never read as an error) and generations never wrap.

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
    /// The process in this scheduler slot.
    Process(usize),
}

#[derive(Clone, Copy)]
pub struct Handles([(u32, Option<(Object, Rights)>); MAX_HANDLES]);

impl Handles {
    pub const fn new() -> Self {
        Self([(0, None); MAX_HANDLES])
    }

    /// init's handles: 0 is the console (write, duplicate), 1 is process `slot` itself (kill).
    pub fn init(slot: usize) -> Self {
        let mut handles = Self::new();
        handles.0[0].1 = Some((Object::Console, WRITE | DUPLICATE));
        handles.0[1].1 = Some((Object::Process(slot), KILL));
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

    /// A new handle to `handle`'s object with `rights`, a subset of its own; needs the duplicate right.
    pub fn dup(&mut self, handle: u64, rights: Rights) -> Result<u64, i64> {
        let (object, held) = self.entry(handle)?;
        if held & DUPLICATE == 0 || rights & !held != 0 {
            return Err(EACCES);
        }
        let index = self
            .0
            .iter()
            .position(|e| e.1.is_none() && e.0 < RETIRED)
            .ok_or(EMFILE)?;
        self.0[index].1 = Some((object, rights));
        Ok((self.0[index].0 as u64) << 32 | index as u64)
    }

    pub fn close(&mut self, handle: u64) -> Result<(), i64> {
        self.entry(handle)?;
        let entry = &mut self.0[handle as u32 as usize];
        *entry = (entry.0 + 1, None);
        Ok(())
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
