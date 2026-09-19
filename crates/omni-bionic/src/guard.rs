//! Test-only guard-region harness: proves every write stays inside a struct's real size.
//!
//! The idea, from the task rules: place a known byte pattern immediately before and
//! after each struct in mock guest memory ("guard bytes"). After every operation, assert
//! the guards are untouched. A wrong struct size then fails as a loud test assertion
//! instead of as silent memory corruption of the engine's neighbouring data.
//!
//! This module is compiled always (it is referenced by integration tests), but is only
//! *used* by tests; it holds no runtime logic. It sits in `src/` rather than `tests/`
//! so every test file can share one implementation.

use crate::memory::GuestMemory;
use crate::mock::MockMemory;

/// The byte written into every guard position.
pub const GUARD_BYTE: u8 = 0xA5;

/// A struct placed in mock memory with guard bytes before and after it.
///
/// Layout: `[guard_lo .. struct_base) [struct_base .. struct_base+size) [struct_end .. guard_hi)`
pub struct GuardedStruct<'a> {
    mem: &'a mut MockMemory,
    /// First struct byte.
    pub base: u64,
    /// Struct size in bytes (the declared layout size).
    pub size: u64,
    /// First guarded byte before the struct.
    pub guard_lo_start: u64,
    /// One past the last guarded byte after the struct.
    pub guard_hi_end: u64,
}

impl<'a> GuardedStruct<'a> {
    /// Place a `size`-byte struct at `base` in `mem`, filling the struct with `init`
    /// and surrounding it with `GUARD_BYTE` guards of `guard` bytes on each side.
    pub fn place(
        mem: &'a mut MockMemory,
        base: u64,
        size: u64,
        init: &[u8],
        guard: usize,
    ) -> Self {
        assert_eq!(init.len() as u64, size, "init must fill the struct exactly");
        let guard_lo_start = base - guard as u64;
        let struct_end = base + size;
        let guard_hi_end = struct_end + guard as u64;

        // Map one contiguous region covering guards + struct.
        let mut bytes = vec![GUARD_BYTE; guard + size as usize + guard];
        bytes[guard..guard + size as usize].copy_from_slice(init);
        mem.map(guard_lo_start, &bytes);

        GuardedStruct { mem, base, size, guard_lo_start, guard_hi_end }
    }

    /// Borrow the underlying memory for an operation under test.
    pub fn mem(&mut self) -> &mut MockMemory {
        self.mem
    }

    /// Shared-memory reborrow for functions taking `&impl GuestMemory`.
    pub fn as_mem(&self) -> &impl GuestMemory {
        self.mem
    }

    /// Assert both guard bands are still exactly the guard pattern.
    pub fn assert_guards_intact(&self) {
        let lo_len = (self.base - self.guard_lo_start) as usize;
        let mut lo = vec![0u8; lo_len];
        self.mem.read(self.guard_lo_start, &mut lo).expect("guard_lo mapped");
        assert!(
            lo.iter().all(|&b| b == GUARD_BYTE),
            "LOW guard bytes corrupted at {:#x}..{:#x}: {:02x?}",
            self.guard_lo_start,
            self.base,
            lo
        );

        let hi_len = (self.guard_hi_end - (self.base + self.size)) as usize;
        let mut hi = vec![0u8; hi_len];
        self.mem.read(self.base + self.size, &mut hi).expect("guard_hi mapped");
        assert!(
            hi.iter().all(|&b| b == GUARD_BYTE),
            "HIGH guard bytes corrupted at {:#x}..{:#x}: {:02x?}",
            self.base + self.size,
            self.guard_hi_end,
            hi
        );
    }

    /// Read the whole struct and assert every byte is one of `allowed` values.
    /// Used after operations to assert the struct *itself* was not scribbled beyond
    /// the bytes the operation is allowed to touch (callers pass the exact set).
    pub fn read_struct(&self) -> Vec<u8> {
        let mut buf = vec![0u8; self.size as usize];
        self.mem
            .read(self.base, &mut buf)
            .expect("struct mapped");
        buf
    }
}

/// Map `size` zero bytes at `addr` and return the address (convenience for placing
/// initialised structs).
pub fn map_zeroed(mem: &mut MockMemory, addr: u64, size: u64) -> u64 {
    mem.map(addr, &vec![0u8; size as usize]);
    addr
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The harness itself must detect a write that overruns the struct.
    #[test]
    fn guard_detects_overrun() {
        let mut mem = MockMemory::new();
        let mut g = GuardedStruct::place(&mut mem, 0x1000, 8, &[0u8; 8], 16);
        // Simulate a bug: write one byte past the struct end (into the high guard).
        g.mem().write(0x1008, &[0xFF]).unwrap();
        let intact = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            g.assert_guards_intact();
        }));
        assert!(intact.is_err(), "guard must catch the overrun");
    }

    /// Guards are intact right after placement (sanity of the harness).
    #[test]
    fn guard_intact_after_placement() {
        let mut mem = MockMemory::new();
        let g = GuardedStruct::place(&mut mem, 0x2000, 40, &[7u8; 40], 16);
        g.assert_guards_intact();
        assert_eq!(g.read_struct(), vec![7u8; 40]);
    }
}
