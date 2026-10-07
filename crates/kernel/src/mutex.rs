//! Mutexes: a fixed table. A handle reaches an entry by index and generation; each entry counts its handles and is
//! freed once none is left. The kernel tracks the owner by scheduler slot; an exiting owner releases what it holds.

use crate::syscall::{EBADF, EDEADLK, EPERM};

/// The mutex at `index` with `generation`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mutex {
    /// `u32` for the same reason as `pipe::End::index`.
    pub index: u32,
    pub generation: u64,
}

#[derive(Clone, Copy)]
struct Entry {
    generation: u64,
    /// Handles to it; 0 while the entry is free.
    handles: usize,
    owner: Option<usize>,
}

pub struct Mutexes<const N: usize>([Entry; N]);

impl<const N: usize> Mutexes<N> {
    pub const fn new() -> Self {
        Self(
            [Entry {
                generation: 0,
                handles: 0,
                owner: None,
            }; N],
        )
    }

    /// A new unlocked mutex with one handle, if an entry is free.
    pub fn create(&mut self) -> Option<Mutex> {
        let index = self.0.iter().position(|e| e.handles == 0)?;
        let entry = &mut self.0[index];
        *entry = Entry {
            generation: entry.generation + 1,
            handles: 1,
            owner: None,
        };
        Some(Mutex {
            index: index as u32,
            generation: entry.generation,
        })
    }

    /// Counts one more handle to `mutex`.
    pub fn open(&mut self, mutex: Mutex) {
        if let Some(entry) = self.get(mutex) {
            entry.handles += 1;
        }
    }

    /// Drops a handle to `mutex`; the last one frees it.
    pub fn close(&mut self, mutex: Mutex) {
        if let Some(entry) = self.get(mutex) {
            entry.handles -= 1;
            if entry.handles == 0 {
                entry.owner = None;
            }
        }
    }

    /// Makes `slot` the owner if `mutex` is free; otherwise returns the owner to wait for. `EDEADLK` if `slot` owns it.
    pub fn lock(&mut self, mutex: Mutex, slot: usize) -> Result<Option<usize>, i64> {
        let entry = self.get(mutex).ok_or(EBADF)?;
        match entry.owner {
            None => {
                entry.owner = Some(slot);
                Ok(None)
            }
            Some(owner) if owner == slot => Err(EDEADLK),
            Some(owner) => Ok(Some(owner)),
        }
    }

    /// Frees `mutex`, which `slot` must own (else `EPERM`).
    pub fn unlock(&mut self, mutex: Mutex, slot: usize) -> Result<(), i64> {
        let entry = self.get(mutex).ok_or(EBADF)?;
        if entry.owner != Some(slot) {
            return Err(EPERM);
        }
        entry.owner = None;
        Ok(())
    }

    /// The owner of the mutex at `index`.
    pub fn owner(&self, index: usize) -> Option<usize> {
        self.0[index].owner
    }

    /// Frees every mutex `slot` owns; yields their indices.
    pub fn release(&mut self, slot: usize) -> impl Iterator<Item = usize> + '_ {
        self.0
            .iter_mut()
            .enumerate()
            .filter_map(move |(index, entry)| {
                (entry.owner == Some(slot)).then(|| {
                    entry.owner = None;
                    index
                })
            })
    }

    fn get(&mut self, mutex: Mutex) -> Option<&mut Entry> {
        let entry = self.0.get_mut(mutex.index as usize)?;
        (entry.handles > 0 && entry.generation == mutex.generation).then_some(entry)
    }
}

impl<const N: usize> Default for Mutexes<N> {
    fn default() -> Self {
        Self::new()
    }
}
