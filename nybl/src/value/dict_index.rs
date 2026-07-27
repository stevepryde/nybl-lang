//! Compact insertion-order sidecar for dictionary key lookup.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{string::String, vec::Vec};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::{string::String, vec::Vec};

#[cfg(test)]
use core::cell::Cell;

use super::Value;
use crate::error::NyblError;

const EMPTY: usize = usize::MAX;

/// Open-addressed lookup into the owning dictionary's entry vector.
///
/// Slots contain entry indices; the insertion-ordered vector remains the sole
/// observable store. Dictionaries do not remove keys, so tombstones are
/// unnecessary. A maximum 50% load keeps ordinary probes short, and geometric
/// rehashing makes incremental construction amortized O(1).
#[derive(Debug, Clone)]
pub(super) struct DictKeyIndex {
    slots: Vec<usize>,
    len: usize,
    #[cfg(test)]
    probes: Cell<usize>,
    #[cfg(test)]
    rehash_moves: Cell<usize>,
}

impl DictKeyIndex {
    pub(super) fn try_from_entries(
        entries: &[(String, Value)],
        line: u32,
    ) -> Result<Self, NyblError> {
        let mut index = Self {
            slots: Vec::new(),
            len: 0,
            #[cfg(test)]
            probes: Cell::new(0),
            #[cfg(test)]
            rehash_moves: Cell::new(0),
        };
        if entries.is_empty() {
            return Ok(index);
        }

        let capacity = Self::capacity_for_len(entries.len(), line)?;
        index
            .slots
            .try_reserve_exact(capacity)
            .map_err(|_| NyblError::fatal("Memory limit exceeded", line))?;
        index.slots.resize(capacity, EMPTY);
        for entry_index in 0..entries.len() {
            let key = entries[entry_index].0.as_str();
            if index.get(entries, key).is_none() {
                index.insert_new(key, entry_index);
            }
        }
        #[cfg(test)]
        index.probes.set(0);
        Ok(index)
    }

    fn capacity_for_len(len: usize, line: u32) -> Result<usize, NyblError> {
        let required = len
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .ok_or_else(|| NyblError::fatal("Memory limit exceeded", line))?;
        Ok(required.max(8))
    }

    pub(super) fn tracked_bytes(&self) -> usize {
        core::mem::size_of::<Self>() + self.slots.capacity() * core::mem::size_of::<usize>()
    }

    pub(super) fn get(&self, entries: &[(String, Value)], key: &str) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut slot = hash(key) & mask;
        for _ in 0..self.slots.len() {
            #[cfg(test)]
            self.probes.set(self.probes.get().saturating_add(1));
            let entry_index = self.slots[slot];
            if entry_index == EMPTY {
                return None;
            }
            if entries[entry_index].0 == key {
                return Some(entry_index);
            }
            slot = (slot + 1) & mask;
        }
        None
    }

    /// Fallibly prepare room for one absent key. Rehashing is built in a
    /// temporary vector and committed only after allocation and population
    /// succeed, so an error leaves the existing index mapping intact.
    pub(super) fn ensure_insert_capacity(
        &mut self,
        entries: &[(String, Value)],
        line: u32,
    ) -> Result<(), NyblError> {
        if !self.slots.is_empty() && self.len.saturating_add(1) <= self.slots.len() / 2 {
            return Ok(());
        }
        let new_capacity = if self.slots.is_empty() {
            8
        } else {
            self.slots
                .len()
                .checked_mul(2)
                .ok_or_else(|| NyblError::fatal("Memory limit exceeded", line))?
        };
        let mut new_slots = Vec::new();
        new_slots
            .try_reserve_exact(new_capacity)
            .map_err(|_| NyblError::fatal("Memory limit exceeded", line))?;
        new_slots.resize(new_capacity, EMPTY);
        for entry_index in self.slots.iter().copied() {
            if entry_index != EMPTY {
                Self::insert_into_slots(&mut new_slots, &entries[entry_index].0, entry_index);
            }
        }
        #[cfg(test)]
        self.rehash_moves
            .set(self.rehash_moves.get().saturating_add(self.len));
        self.slots = new_slots;
        Ok(())
    }

    /// Insert a key already proven absent after capacity has been prepared.
    pub(super) fn insert_new(&mut self, key: &str, entry_index: usize) {
        Self::insert_into_slots(&mut self.slots, key, entry_index);
        self.len += 1;
    }

    fn insert_into_slots(slots: &mut [usize], key: &str, entry_index: usize) {
        let mask = slots.len() - 1;
        let mut slot = hash(key) & mask;
        for _ in 0..slots.len() {
            if slots[slot] == EMPTY {
                slots[slot] = entry_index;
                return;
            }
            slot = (slot + 1) & mask;
        }
        unreachable!("prepared dictionary index always contains an empty slot");
    }

    #[cfg(test)]
    pub(super) fn reset_probes(&self) {
        self.probes.set(0);
    }

    #[cfg(test)]
    pub(super) fn probes(&self) -> usize {
        self.probes.get()
    }

    #[cfg(all(test, any(feature = "std", not(feature = "no_std"))))]
    pub(super) fn rehash_moves(&self) -> usize {
        self.rehash_moves.get()
    }

    pub(super) fn entry_count(&self) -> usize {
        self.len
    }

    #[cfg(all(test, any(feature = "std", not(feature = "no_std"))))]
    pub(super) fn slot_count(&self) -> usize {
        self.slots.len()
    }
}

fn hash(key: &str) -> usize {
    // Deterministic FNV-1a keeps this core-only without a hash dependency or
    // per-dictionary random state.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash as usize
}
