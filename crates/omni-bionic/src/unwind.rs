//! Walking an AArch64 frame-pointer chain, for diagnostics only.
//!
//! # Why this exists, and what it is allowed to claim
//!
//! A thread that has stopped is named by its *stack*, not by its last import. The boundary's
//! per-thread record answers "which symbol did it cross", which is one frame deep, and a thread
//! blocked in `pthread_cond_wait` crosses at a libc++ wrapper that a hundred unrelated call sites
//! share. **MEASURED:** a run stalled in `nativePostClientSettingsLoadedInitialization3` reported
//! `pthread_cond_wait from link 0x2320170` — a two-instruction helper with ten callers, which
//! narrowed the question to one part in ten and no further.
//!
//! AAPCS64 makes `X29` the frame pointer and stores `[fp] = caller's fp`, `[fp + 8] = return
//! address`, so the chain is walkable without unwind tables as long as the frames that matter were
//! compiled with one. **They may not have been.** A leaf, a `-fomit-frame-pointer` translation
//! unit, or a frame partway through its prologue breaks or skips a link, and this returns what it
//! could read rather than pretending otherwise.
//!
//! So a frame list here is **evidence, never control flow**: nothing may branch on it, and the
//! guest supplies every value in it. The walk defends itself accordingly — alignment, ascent,
//! a bound, and a read that is allowed to fail — because a guest that hands it a cycle or a
//! pointer into its own heap must get a short list, not a hang.

use crate::memory::GuestMemory;

/// How many frames [`frames`] will walk before it stops, however deep the stack is.
///
/// A bound rather than a stack-range check, because this crate has no map (D19) and cannot know
/// where the stack ends. 64 is past anything a reader reads and far short of a cost.
pub const MAX_FRAMES: usize = 64;

/// The return addresses on a thread's stack, innermost first.
///
/// `link` is `X30` — the call site of the function that is *running*, which has no frame record
/// yet — and is first in the list when it is non-zero. `fp` is `X29`. Each step reads the pair at
/// `[fp]` and `[fp + 8]`.
///
/// The walk stops, returning what it has, at the first of: `max` frames; a null or misaligned
/// frame pointer; a chain that does not ascend (which a cycle cannot escape); a null return
/// address; or a read that faults. None of these is an error — a truncated list is the honest
/// answer to "what could be read from here".
#[must_use]
pub fn frames(mem: &impl GuestMemory, fp: u64, link: u64, max: usize) -> Vec<u64> {
    let mut out = Vec::new();
    if link != 0 {
        out.push(link);
    }
    let mut fp = fp;
    let limit = max.min(MAX_FRAMES);
    while out.len() < limit {
        // A frame record is two 64-bit words, so an unaligned `fp` is not one. Checked before the
        // read rather than after, because on a host that faults on a misaligned load the read is
        // the thing that would go wrong.
        if fp == 0 || fp % 8 != 0 {
            break;
        }
        let mut record = [0u8; 16];
        if mem.read(fp, &mut record).is_err() {
            break;
        }
        let next = u64::from_le_bytes([
            record[0], record[1], record[2], record[3], record[4], record[5], record[6], record[7],
        ]);
        let ret = u64::from_le_bytes([
            record[8], record[9], record[10], record[11], record[12], record[13], record[14],
            record[15],
        ]);
        if ret == 0 {
            break;
        }
        out.push(ret);
        // **Strictly ascending, which is what makes a cycle terminate.** AArch64 stacks grow
        // down, so a caller's frame is always at a higher address. A guest that writes its own
        // frame pointer — and D6 says to assume one that does — can otherwise point `[fp]` at
        // `fp` and get a walk that never ends.
        if next <= fp {
            break;
        }
        fp = next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMemory;

    /// Build a stack of frame records at `base`, innermost first, each 16 bytes and each pointing
    /// at the next — the layout AAPCS64 prescribes and the only one this walker knows.
    fn stack(base: u64, returns: &[u64]) -> MockMemory {
        let mut bytes = Vec::new();
        for (index, ret) in returns.iter().enumerate() {
            let next = if index + 1 < returns.len() { base + (index as u64 + 1) * 16 } else { 0 };
            bytes.extend_from_slice(&next.to_le_bytes());
            bytes.extend_from_slice(&ret.to_le_bytes());
        }
        let mut mem = MockMemory::new();
        mem.map(base, &bytes);
        mem
    }

    #[test]
    fn the_link_register_is_the_innermost_frame_and_the_chain_follows_it() {
        let mem = stack(0x1000, &[0xAAAA, 0xBBBB, 0xCCCC]);
        assert_eq!(frames(&mem, 0x1000, 0x9999, 16), vec![0x9999, 0xAAAA, 0xBBBB, 0xCCCC]);
    }

    /// `X30` is zero on a thread that has not called anything yet, and a zero is not an address.
    #[test]
    fn a_zero_link_register_is_left_out_rather_than_reported_as_a_frame() {
        let mem = stack(0x1000, &[0xAAAA]);
        assert_eq!(frames(&mem, 0x1000, 0, 16), vec![0xAAAA]);
    }

    /// The walk ends where the chain does. A null frame pointer terminates it, as the last record
    /// written by a thread's entry stub does.
    #[test]
    fn the_walk_stops_at_the_end_of_the_chain_without_complaining() {
        let mem = stack(0x2000, &[0x11, 0x22]);
        assert_eq!(frames(&mem, 0x2000, 0, 16), vec![0x11, 0x22]);
    }

    /// **A cycle must terminate**, and the bound is not what terminates it. D6 says to assume a
    /// guest that writes its own frame pointer; a frame that points at itself, or backwards, is not
    /// a stack, and the ascent test is what says so in one step rather than `max` of them.
    #[test]
    fn a_frame_pointer_cycle_stops_at_the_frame_that_does_not_ascend() {
        let mut mem = MockMemory::new();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x1000u64.to_le_bytes()); // points at itself
        bytes.extend_from_slice(&0xDEADu64.to_le_bytes());
        mem.map(0x1000, &bytes);
        assert_eq!(frames(&mem, 0x1000, 0, 64), vec![0xDEAD]);
    }

    /// A chain that runs backwards down the stack is the same defect wearing a different hat.
    #[test]
    fn a_frame_pointer_that_descends_stops_the_walk() {
        let mut mem = MockMemory::new();
        let mut high = Vec::new();
        high.extend_from_slice(&0x1000u64.to_le_bytes()); // below itself
        high.extend_from_slice(&0xBEEFu64.to_le_bytes());
        mem.map(0x2000, &high);
        mem.map(0x1000, &[0u8; 16]);
        assert_eq!(frames(&mem, 0x2000, 0, 64), vec![0xBEEF]);
    }

    /// A frame pointer that is not eight-byte aligned is not a frame pointer, and the check comes
    /// before the read because a misaligned load is the thing that would go wrong.
    ///
    /// **The region is deliberately larger than the frames in it.** MEASURED: the first version
    /// mapped exactly one frame record and read at `base + 3`, so the sixteen-byte read ran off
    /// the end and faulted — and a walk that stops on a fault returns the same empty list as one
    /// that rejected the alignment. The mutation harness reported `unwind-B1` NOT CAUGHT, which
    /// was correct: the test could not fail. With room to read, the misaligned load *succeeds* and
    /// yields a garbage return address, so removing the check now produces a frame that the
    /// aligned walk never would.
    #[test]
    fn a_misaligned_frame_pointer_is_rejected_before_it_is_read() {
        let mut mem = MockMemory::new();
        // Sixty-four readable bytes: a well-formed chain, and slack after it so that a read three
        // bytes into the record is inside the region rather than off the end of it.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0xAAAAu64.to_le_bytes());
        bytes.resize(64, 0x5A);
        mem.map(0x1000, &bytes);

        assert!(
            frames(&mem, 0x1003, 0, 16).is_empty(),
            "a misaligned frame pointer is not a frame pointer, whatever reads from it",
        );
        // The detector's own precondition: the read the check prevents would have succeeded and
        // would have produced something. Without this the test could pass for the wrong reason
        // again, and nothing would say so.
        let mut probe = [0u8; 16];
        mem.read(0x1003, &mut probe).expect("the misaligned read is inside the region");
        assert_ne!(
            u64::from_le_bytes(probe[8..].try_into().expect("eight bytes")),
            0,
            "the bytes at the misaligned address must look like a return address, or this test              cannot tell the check from a fault",
        );
    }

    /// An unmapped frame pointer is a short list, not an error: what could be read is the answer.
    #[test]
    fn an_unreadable_frame_pointer_truncates_the_list_rather_than_failing() {
        let mem = stack(0x1000, &[0xAAAA]);
        assert_eq!(frames(&mem, 0x9_0000, 0x77, 16), vec![0x77]);
    }

    /// A null return address ends the chain: the outermost frame of a thread stores zero there.
    #[test]
    fn a_null_return_address_ends_the_chain_and_is_not_reported() {
        let mut mem = MockMemory::new();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x1010u64.to_le_bytes());
        bytes.extend_from_slice(&0xAAAAu64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        mem.map(0x1000, &bytes);
        assert_eq!(frames(&mem, 0x1000, 0, 16), vec![0xAAAA]);
    }

    /// The caller's bound is honoured, and so is this module's, whichever is smaller.
    #[test]
    fn the_list_is_bounded_by_the_smaller_of_the_two_caps() {
        let deep: Vec<u64> = (1..=40).collect();
        let mem = stack(0x1000, &deep);
        assert_eq!(frames(&mem, 0x1000, 0, 5).len(), 5);
        assert_eq!(frames(&mem, 0x1000, 0, usize::MAX).len(), 40.min(MAX_FRAMES));
        // The link register counts against the bound too, so a caller asking for five gets five.
        assert_eq!(frames(&mem, 0x1000, 0x99, 5), vec![0x99, 1, 2, 3, 4]);
    }
}
