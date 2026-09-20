//! Unit-test mock: an in-memory [`GuestMemory`] made of mapped regions backed by `Vec<u8>`.
//!
//! Everything in the crate is tested through this mock; testing it exhaustively *first*
//! (see `tests/mock_tests.rs`) is what makes the rest of the suite trustworthy.

use crate::memory::GuestMemory;

/// One mapped guest region: `start .. start + bytes.len()`.
#[derive(Debug, Clone)]
pub struct Region {
    /// First mapped guest address.
    pub start: u64,
    /// Region contents, `bytes[i]` lives at `start + i`.
    pub bytes: Vec<u8>,
}

impl Region {
    /// Whether `addr` is inside this region. Overflow-free by construction:
    /// `addr - start < len` with the `addr >= start` guard first.
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.start && (addr - self.start) < self.bytes.len() as u64
    }
}

/// An in-memory guest address space: a sorted set of disjoint [`Region`]s.
///
/// Any access to an address outside every region — including an access that straddles a
/// region boundary — returns a [`crate::memory::Fault`] naming the first unmapped byte
/// touched. Two regions may sit adjacent; adjacency never merges semantics: a read crossing
/// the seam is served from both sides, exactly as a real address space would.
///
/// A faulting access may have already touched the bytes before the faulting one (the mock
/// accesses byte-at-a-time); callers that need all-or-nothing behaviour validate the whole
/// range first, as the crate's own functions do with [`crate::memory::checked_range`].
#[derive(Debug, Clone, Default)]
pub struct MockMemory {
    regions: Vec<Region>,
}

impl MockMemory {
    /// An empty address space (nothing mapped).
    pub fn new() -> Self {
        Self::default()
    }

    /// Map `bytes` at `start`, replacing any overlapping region (test convenience; no test
    /// relies on replacement semantics).
    pub fn map(&mut self, start: u64, bytes: &[u8]) {
        let end = start.saturating_add(bytes.len() as u64);
        self.regions.retain(|r| r.end_exclusive() <= start || r.start >= end);
        self.regions.push(Region { start, bytes: bytes.to_vec() });
        self.regions.sort_by_key(|r| r.start);
    }

    /// Convenience: map `s.as_bytes()` plus a NUL terminator at `start`, returning `start`.
    /// Models how a guest C string sits in memory.
    pub fn map_str(&mut self, start: u64, s: &str) -> u64 {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        self.map(start, &bytes);
        start
    }

    fn region_containing(&self, addr: u64) -> Option<&Region> {
        self.regions.iter().find(|r| r.contains(addr))
    }


}

impl Region {
    /// Exclusive end of the region; `None` if it would exceed `u64::MAX` (a region whose
    /// last byte is at `u64::MAX`). Only informational; `contains` is the authority.
    pub fn end_exclusive(&self) -> u64 {
        self.start.saturating_add(self.bytes.len() as u64)
    }
}

impl GuestMemory for MockMemory {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), crate::memory::Fault> {
        let mut cursor = addr;
        for slot in buf.iter_mut() {
            let region = self
                .region_containing(cursor)
                .ok_or(crate::memory::Fault(cursor))?;
            *slot = region.bytes[(cursor - region.start) as usize];
            cursor = cursor.wrapping_add(1);
        }
        Ok(())
    }

    fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), crate::memory::Fault> {
        let mut cursor = addr;
        for &byte in buf {
            // Only one region can contain `cursor` because `map` keeps the set disjoint.
            let idx = self
                .regions
                .iter()
                .position(|r| r.contains(cursor))
                .ok_or(crate::memory::Fault(cursor))?;
            let region = &mut self.regions[idx];
            region.bytes[(cursor - region.start) as usize] = byte;
            cursor = cursor.wrapping_add(1);
        }
        Ok(())
    }
}
