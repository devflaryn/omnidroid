//! Finding a function that can be executed on its own, in a stripped 109 MB binary.
//!
//! # The problem this solves
//!
//! M2 has to run **real** `libroblox.so` code before the imported-symbol thunk boundary exists
//! (M3). That means a function that reaches nothing outside itself: no `BL`, no `BLR`, no branch
//! out of its own body, and no load of a pointer that a relocation supplied. The binary is
//! stripped, so there is no symbol table to look in; [`crate::eh_frame`] recovers **245,117 exact
//! function starts and lengths** instead, and this module decodes each body and says what it
//! reaches for.
//!
//! # How the classification is made, and what it is worth
//!
//! Every word is decoded against the ARM ARM's top-level `op0` field and a handful of exact
//! encodings. The result is deliberately **conservative in one direction**: anything this module
//! cannot place is counted as [`BodyFacts::undecodable`] and disqualifies the function. A
//! misclassification therefore loses a candidate, and cannot admit one that calls out — which
//! matters, because the consequence of admitting one is guest code branching into an unrelocated
//! PLT stub and faulting somewhere unrelated.
//!
//! The classification is static, so it is a claim about the *body*, not about a particular run.
//! What makes it load-bearing for M2 is that the two are checked against each other: the runtime
//! test runs the chosen function with a return sentinel armed and a thunk planted on every address
//! outside it that the body could reach, so a function that were not a leaf would produce a thunk
//! exit or a fault rather than a return. See the M2 gate in `omni-cpu`.
//!
//! # Three grades of leaf
//!
//! [`LeafKind`] distinguishes them because they need different amounts of guest state, and saying
//! "leaf" without saying which would hide that:
//!
//! | Kind | Reaches | What a caller must provide |
//! |---|---|---|
//! | [`LeafKind::PureRegister`] | registers only | nothing but the argument registers |
//! | [`LeafKind::StackOnly`] | its own stack frame | a stack, `SP` |
//! | [`LeafKind::StackAndThreadPointer`] | stack, and `[TPIDR_EL0, #0x28]` | a stack **and** a bionic TLS block (D13) |
//!
//! The third grade is the interesting one: 1,276 of `libroblox.so`'s 1,282 `MRS Xt, TPIDR_EL0`
//! instructions load `[Xt, #0x28]`, bionic's `TLS_SLOT_STACK_GUARD`, and a function of this grade
//! is one of them — real engine code whose correctness depends on the thread pointer being
//! programmed.

use std::collections::BTreeSet;

use crate::eh_frame::FunctionBounds;
use crate::error::{ElfError, Result};
use crate::segment::SegmentFlags;
use crate::ElfImage;

/// Bionic's `TLS_SLOT_STACK_GUARD`: slot 5, at `5 * 8` bytes from the thread pointer (D13).
pub const TLS_SLOT_STACK_GUARD_OFFSET: u64 = 0x28;

/// What one function body reaches for, decoded from its instructions.
///
/// Every field is a count or a set rather than a bool, because the question a caller asks is not
/// only "is it a leaf" but "what would I have to set up to run it", and "one `BL`, to this
/// address" is a different answer from "three, to three addresses".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodyFacts {
    /// Targets of `BL`. A direct call out of the body.
    pub direct_calls: BTreeSet<u64>,
    /// `BLR` and `BR`: an indirect transfer, whose target is not knowable statically.
    pub indirect_transfers: usize,
    /// `B`, `B.cond`, `CBZ`/`CBNZ`, `TBZ`/`TBNZ` whose target leaves `[start, end)`.
    pub escaping_branches: usize,
    /// `ADR` and `ADRP`: the body forms an address of something outside itself, which is how a
    /// relocated pointer is normally reached.
    pub pc_relative_addressing: usize,
    /// Base registers of every load and store, by encoding number — `31` is `SP`.
    pub memory_bases: BTreeSet<u8>,
    /// Base registers that were neither `SP` nor a live thread pointer when they were used.
    ///
    /// This is the field that says whether the body dereferences something a **caller** would have
    /// to supply. A body with any of these is not runnable from a bare register file, however few
    /// calls it makes.
    pub foreign_memory_bases: BTreeSet<u8>,
    /// `MRS Xt, TPIDR_EL0` and `MRS Xt, TPIDRRO_EL0`.
    pub thread_pointer_reads: usize,
    /// `LDR Xt, [Xn, #0x28]` where `Xn` last came from a thread-pointer read: bionic's stack guard.
    pub stack_guard_loads: usize,
    /// `SVC`, `HVC`, `BRK`, `MSR`, barriers, and every other exception-or-system instruction that
    /// is not a hint. A guest syscall is not something M2 can serve.
    pub system_instructions: usize,
    /// The hint space: `NOP`, `YIELD`, and the PAC/BTI forms, which D5 confirmed no-op correctly
    /// on this pin. Counted rather than refused, because refusing them would throw away every
    /// candidate that happens to be padded.
    pub hints: usize,
    /// `BL` instructions that appear **before** the body's last `RET`.
    ///
    /// The distinction earns its place on exactly one shape, and it is the shape M2 needs. A
    /// stack-protected AArch64 function ends
    /// `CMP; B.NE .fail; <epilogue>; RET; .fail: BL __stack_chk_fail`, with the call in a
    /// `noreturn` tail *past* the last return. Every path that returns is therefore call-free,
    /// while the body as a whole is not — and lumping the two together would either lose the
    /// function or claim more about it than is true.
    pub calls_before_last_return: usize,
    /// Words this module could not place in a class it reasons about.
    pub undecodable: usize,
    /// `RET X30`.
    pub returns: usize,
    /// `RET Xn` for some `Xn` other than `X30` — a return to a link register a caller did not set.
    pub returns_via_other_register: usize,
    /// Words of the body that a dynamic relocation writes to.
    ///
    /// Non-zero means the body is *patched at load time*, so its instructions in the file are not
    /// the instructions that execute. AArch64 shared objects normally have none at all, and
    /// `libroblox.so` has `DF_BIND_NOW` with no `DT_TEXTREL`, so this is expected to be zero — but
    /// expected-to-be-zero is exactly the kind of assumption that should be a measurement.
    pub relocated_words: usize,
}

impl BodyFacts {
    /// Whether anything in the body transfers control outside it other than by returning.
    #[must_use]
    pub fn leaves_the_body(&self) -> bool {
        !self.direct_calls.is_empty()
            || self.indirect_transfers > 0
            || self.escaping_branches > 0
            || self.returns_via_other_register > 0
    }

    /// Whether every path that *returns* is free of calls: no indirect transfers, no branches out,
    /// and no `BL` before the last `RET`.
    #[must_use]
    pub fn returning_paths_are_call_free(&self) -> bool {
        self.indirect_transfers == 0
            && self.escaping_branches == 0
            && self.returns_via_other_register == 0
            && self.calls_before_last_return == 0
            && self.returns > 0
    }

    /// Whether the body is one this module fully accounted for.
    #[must_use]
    pub fn fully_decoded(&self) -> bool {
        self.undecodable == 0
    }

    /// Which grade of leaf this body is, if any.
    #[must_use]
    pub fn kind(&self) -> LeafKind {
        if !self.fully_decoded()
            || !self.returning_paths_are_call_free()
            || self.system_instructions > 0
            || self.pc_relative_addressing > 0
            || self.relocated_words > 0
            || !self.foreign_memory_bases.is_empty()
        {
            return LeafKind::NotALeaf;
        }
        if !self.direct_calls.is_empty() {
            // Calls exist, but only past the last `RET`. That is a real claim and a narrow one:
            // it says nothing about what happens if one *is* reached, which is why the M2 gate
            // plants a thunk on every target and asserts that none of them was.
            return if self.stack_guard_loads > 0 {
                LeafKind::StackGuardProtected
            } else {
                LeafKind::NotALeaf
            };
        }
        if self.memory_bases.is_empty() && self.thread_pointer_reads == 0 {
            return LeafKind::PureRegister;
        }
        if self.thread_pointer_reads > 0 {
            // A thread-pointer read whose value is never dereferenced is not what D13 is about.
            return if self.stack_guard_loads > 0 {
                LeafKind::StackAndThreadPointer
            } else {
                LeafKind::NotALeaf
            };
        }
        LeafKind::StackOnly
    }
}

/// How self-contained a function is, and therefore what a caller must set up to run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LeafKind {
    /// Registers only: no memory access of any kind, no thread pointer.
    PureRegister,
    /// Its own stack frame, through `SP`, and nothing else.
    StackOnly,
    /// Its own stack frame and bionic's stack guard at `[TPIDR_EL0, #0x28]` (D13).
    StackAndThreadPointer,
    /// As [`LeafKind::StackAndThreadPointer`], plus a stack-protector failure tail past the last
    /// `RET` that calls `__stack_chk_fail`.
    ///
    /// Every path that returns is call-free; the call exists and is reached only when the guard
    /// comparison fails. This grade is what most real engine code looks like, so excluding it
    /// would mean M2 only ever ran code the compiler decided not to protect.
    StackGuardProtected,
    /// Reaches something a caller would have to provide, or something this module cannot account
    /// for. The default answer, so a body that cannot be decoded is never mistaken for a leaf.
    NotALeaf,
}

/// One function, its bounds and what its body reaches for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafFunction {
    /// Where it is, in `p_vaddr` space.
    pub bounds: FunctionBounds,
    /// What its body does.
    pub facts: BodyFacts,
    /// Its grade.
    pub kind: LeafKind,
}

/// Every address a dynamic relocation writes to, restricted to the executable segments.
///
/// Built once and consulted per function. It is a sorted `Vec` rather than a set because the only
/// query is "does anything land in this range", and 568,806 relocations make the allocation
/// difference worth having: the packed blob is streamed rather than materialised, and everything
/// outside an executable segment is dropped as it goes.
#[derive(Debug, Clone, Default)]
pub struct TextRelocations {
    offsets: Vec<u64>,
    /// How many relocations were examined, so "none in the text" can be told apart from "none
    /// examined".
    pub examined: u64,
}

impl TextRelocations {
    /// Collect every relocation target that lands in an executable `PT_LOAD`.
    ///
    /// # Errors
    ///
    /// Whatever decoding the object's relocation tables fails with.
    pub fn collect(elf: &ElfImage<'_>) -> Result<Self> {
        let executable: Vec<(u64, u64)> = elf
            .load_segments()
            .filter(|s| s.p_flags.contains(SegmentFlags::EXEC))
            .map(|s| (s.p_vaddr, s.vaddr_end()))
            .collect();
        let in_text = |offset: u64| executable.iter().any(|&(lo, hi)| offset >= lo && offset < hi);

        let mut offsets = Vec::new();
        let mut examined = 0u64;
        elf.decode_packed_with(|rela| {
            examined += 1;
            if in_text(rela.r_offset) {
                offsets.push(rela.r_offset);
            }
            Ok(())
        })?;
        let unpacked = elf.unpacked_relocations()?;
        for table in unpacked.general.iter().chain(unpacked.plt.iter()) {
            for rela in &table.relocations {
                examined += 1;
                if in_text(rela.r_offset) {
                    offsets.push(rela.r_offset);
                }
            }
        }
        offsets.sort_unstable();
        Ok(Self { offsets, examined })
    }

    /// How many relocation targets land in `[start, end)`.
    #[must_use]
    pub fn count_in(&self, start: u64, end: u64) -> usize {
        let lo = self.offsets.partition_point(|&o| o < start);
        let hi = self.offsets.partition_point(|&o| o < end);
        hi - lo
    }

    /// How many relocations land in an executable segment at all.
    #[must_use]
    pub fn total(&self) -> usize {
        self.offsets.len()
    }
}

/// Decode one function body and report what it reaches for.
///
/// `code` is the function's bytes, `start` its first address. `relocations` may be `None` when the
/// caller has already established that the object relocates nothing in its text.
#[must_use]
pub fn body_facts(
    code: &[u8],
    start: u64,
    relocations: Option<&TextRelocations>,
) -> BodyFacts {
    let mut facts = BodyFacts::default();
    if code.len() % 4 != 0 || code.is_empty() {
        facts.undecodable += 1;
        return facts;
    }
    let end = start + code.len() as u64;
    if let Some(r) = relocations {
        facts.relocated_words = r.count_in(start, end);
    }

    // Which registers currently hold a thread pointer. Cleared when a register is written by
    // anything else this module recognises as a definition, which is only the `MRS` itself and
    // the `LDR` that consumes it — a deliberately tiny dataflow, because its only job is to tell
    // `LDR X9, [X8, #0x28]` after an `MRS X8` apart from the same encoding after an `ADD X8, SP`.
    let mut thread_pointer_regs = [false; 32];

    // Index of the last `RET X30`, so a `BL` can be placed relative to it. Found in a first pass
    // because a forward scan cannot know where the last return is until it has seen the whole body.
    let last_return = code
        .chunks_exact(4)
        .enumerate()
        .filter(|(_, w)| {
            u32::from_le_bytes([w[0], w[1], w[2], w[3]]) & 0xFFFF_FC1F == 0xD65F_0000
        })
        .map(|(i, _)| i)
        .next_back();

    for (i, word) in code.chunks_exact(4).enumerate() {
        let w = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        let at = start + (i as u64) * 4;

        // Branch and system encodings, most specific first.
        if w & 0xFC00_0000 == 0x9400_0000 {
            facts.direct_calls.insert(branch_target(at, imm26(w)));
            if last_return.is_none_or(|last| i < last) {
                facts.calls_before_last_return += 1;
            }
            continue;
        }
        if w & 0xFFFF_FC1F == 0xD63F_0000 || w & 0xFFFF_FC1F == 0xD61F_0000 {
            facts.indirect_transfers += 1;
            continue;
        }
        if w & 0xFFFF_FC1F == 0xD65F_0000 {
            if (w >> 5) & 0x1F == 30 {
                facts.returns += 1;
            } else {
                facts.returns_via_other_register += 1;
            }
            continue;
        }
        if w & 0xFC00_0000 == 0x1400_0000 {
            escapes(&mut facts, branch_target(at, imm26(w)), start, end);
            continue;
        }
        if w & 0xFF00_0010 == 0x5400_0000 {
            escapes(&mut facts, branch_target(at, imm19(w)), start, end);
            continue;
        }
        if w & 0x7E00_0000 == 0x3400_0000 {
            escapes(&mut facts, branch_target(at, imm19(w)), start, end);
            continue;
        }
        if w & 0x7E00_0000 == 0x3600_0000 {
            escapes(&mut facts, branch_target(at, imm14(w)), start, end);
            continue;
        }
        // `MRS Xt, TPIDR_EL0` / `TPIDRRO_EL0`, the two D13 cares about.
        if w & 0xFFFF_FFE0 == 0xD53B_D040 || w & 0xFFFF_FFE0 == 0xD53B_D060 {
            facts.thread_pointer_reads += 1;
            thread_pointer_regs[(w & 0x1F) as usize] = true;
            continue;
        }
        // The hint space: `NOP`, `YIELD`, `SEV`, and the PAC/BTI forms, which D5 confirmed no-op
        // correctly on this pin. Harmless, and common enough as padding that refusing them would
        // throw away candidates for no reason.
        if w & 0xFFFF_F01F == 0xD503_201F {
            facts.hints += 1;
            continue;
        }
        // Everything else in the branch/exception/system class: `SVC`, `BRK`, `MSR`, barriers.
        if (w >> 26) & 0x7 == 0b101 {
            facts.system_instructions += 1;
            continue;
        }
        // `ADR` / `ADRP`.
        if w & 0x1F00_0000 == 0x1000_0000 {
            facts.pc_relative_addressing += 1;
            let rd = (w & 0x1F) as usize;
            thread_pointer_regs[rd] = false;
            continue;
        }

        match (w >> 25) & 0xF {
            // Data processing -- immediate, and data processing -- register.
            0b1000 | 0b1001 | 0b0101 | 0b1101 => {
                thread_pointer_regs[(w & 0x1F) as usize] = false;
            }
            // Loads and stores.
            0b0100 | 0b0110 | 0b1100 | 0b1110 => {
                let rn = ((w >> 5) & 0x1F) as u8;
                facts.memory_bases.insert(rn);
                if rn != 31 && !thread_pointer_regs[rn as usize] {
                    facts.foreign_memory_bases.insert(rn);
                }
                // `LDR Xt, [Xn, #imm12]`, 64-bit unsigned offset, off a thread pointer.
                if w & 0xFFC0_0000 == 0xF940_0000
                    && u64::from((w >> 10) & 0xFFF) * 8 == TLS_SLOT_STACK_GUARD_OFFSET
                    && thread_pointer_regs[rn as usize]
                {
                    facts.stack_guard_loads += 1;
                }
                // The destination of a load is no longer a thread pointer. Writebacks and stores
                // are handled by the same clear: over-clearing loses a candidate.
                thread_pointer_regs[(w & 0x1F) as usize] = false;
            }
            // Data processing -- SIMD and floating point. Allowed in a body; it writes a V
            // register, so no general-purpose register changes meaning, except for the transfer
            // forms, which this module does not separate. Clearing the low-numbered register is
            // the conservative choice for `FMOV Xd, Dn`.
            0b0111 | 0b1111 => {
                thread_pointer_regs[(w & 0x1F) as usize] = false;
            }
            _ => facts.undecodable += 1,
        }
    }
    facts
}

fn escapes(facts: &mut BodyFacts, target: u64, start: u64, end: u64) {
    if target < start || target >= end {
        facts.escaping_branches += 1;
    }
}

fn branch_target(at: u64, offset_words: i64) -> u64 {
    at.wrapping_add((offset_words * 4) as u64)
}

fn imm26(w: u32) -> i64 {
    let raw = w & 0x03FF_FFFF;
    ((raw as i32) << 6 >> 6) as i64
}

fn imm19(w: u32) -> i64 {
    let raw = (w >> 5) & 0x0007_FFFF;
    ((raw as i32) << 13 >> 13) as i64
}

fn imm14(w: u32) -> i64 {
    let raw = (w >> 5) & 0x0000_3FFF;
    ((raw as i32) << 18 >> 18) as i64
}

/// Refuse a function map that describes more code than the object contains.
///
/// **A derived bound, not a fitted one.** An object's functions do not overlap, so the sum of their
/// lengths cannot exceed the bytes of executable segment they live in. That makes this a property
/// of any honest map rather than a number somebody chose.
///
/// It is here because a forged or corrupted `.eh_frame` is not merely wrong, it is *expensive*:
/// 245,117 entries whose lengths decode to a few kilobytes each is several gigabytes of decoding
/// that produces nothing. Found the hard way — a mutation-testing row that changed one encoding
/// mask turned a quarter-second scan into a multi-minute one, which is a denial of service on the
/// analysis tool from exactly the kind of input D6 says to expect.
///
/// `libroblox.so`'s real margin: 69,943,828 bytes of function body against 103,645,584 bytes of
/// executable segment, so a genuine object sits comfortably inside.
fn require_map_fits(decoded: u64, executable_bytes: u64) -> Result<()> {
    if decoded > executable_bytes {
        return Err(ElfError::MalformedEhFrame {
            what: ".eh_frame function map",
            offset: decoded,
            reason: "the function bounds sum to more code than the object's executable segments \
                     hold, so they do not describe this object",
        });
    }
    Ok(())
}

/// Decode every function `.eh_frame_hdr` names and keep the ones that are leaves.
///
/// Returns them in address order, with the facts that justify the grade.
///
/// # Errors
///
/// Whatever [`ElfImage::eh_frame_functions`] and [`TextRelocations::collect`] fail with, plus
/// [`ElfError::MalformedEhFrame`] for a map that describes more code than the object holds: an
/// object's functions do not overlap, so their lengths cannot sum past its executable segments.
pub fn find_leaves(elf: &ElfImage<'_>) -> Result<Vec<LeafFunction>> {
    let Some(bounds) = elf.eh_frame_functions()? else {
        return Ok(Vec::new());
    };
    let executable_bytes: u64 = elf
        .load_segments()
        .filter(|s| s.p_flags.contains(SegmentFlags::EXEC))
        .map(|s| s.p_memsz)
        .sum();
    let relocations = TextRelocations::collect(elf)?;
    let mut out = Vec::new();
    let mut decoded = 0u64;
    for b in bounds {
        decoded = decoded.saturating_add(b.len);
        require_map_fits(decoded, executable_bytes)?;
        let Ok(code) = elf.slice_at_vaddr("function body", b.start, b.len) else {
            // A function whose bytes are not in the file image — `.eh_frame` can describe one, and
            // it is not a candidate.
            continue;
        };
        let facts = body_facts(code, b.start, Some(&relocations));
        let kind = facts.kind();
        if kind != LeafKind::NotALeaf {
            out.push(LeafFunction { bounds: b, facts, kind });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asm(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    const RET_X30: u32 = 0xD65F_03C0;

    #[test]
    fn a_register_only_body_with_a_return_is_a_pure_leaf() {
        // SUB W0, W1, #0x41 ; CMP W0, #0x1a ; B.HS +2 ; RET ; MOV W0, #-1 ; RET
        let body = asm(&[0x5101_0420, 0x7100_681F, 0x5400_0042, RET_X30, 0x1280_0000, RET_X30]);
        let facts = body_facts(&body, 0x1000, None);
        assert_eq!(facts.kind(), LeafKind::PureRegister, "{facts:?}");
        assert_eq!(facts.returns, 2);
        assert_eq!(facts.escaping_branches, 0);
        assert!(facts.memory_bases.is_empty());
    }

    #[test]
    fn a_call_a_jump_out_and_an_indirect_transfer_each_disqualify_a_body() {
        let base = 0x1000u64;
        // BL +0x100
        let with_call = asm(&[0x9400_0040, RET_X30]);
        let facts = body_facts(&with_call, base, None);
        assert_eq!(facts.direct_calls.iter().copied().collect::<Vec<_>>(), vec![base + 0x100]);
        assert_eq!(facts.kind(), LeafKind::NotALeaf);

        // B -0x40, which leaves a two-instruction body.
        let escaping = asm(&[0x17FF_FFF0, RET_X30]);
        let facts = body_facts(&escaping, base, None);
        assert_eq!(facts.escaping_branches, 1, "{facts:?}");
        assert_eq!(facts.kind(), LeafKind::NotALeaf);

        // B +1, which stays inside a three-instruction body. The `NOP` is in the hint space, which
        // is counted and allowed rather than refused: D5 confirmed the hint forms no-op correctly,
        // and refusing them would throw away every padded candidate.
        let internal = asm(&[0x1400_0001, 0xD503_201F, RET_X30]);
        let facts = body_facts(&internal, base, None);
        assert_eq!(facts.escaping_branches, 0);
        assert_eq!((facts.hints, facts.system_instructions), (1, 0));
        assert_eq!(facts.kind(), LeafKind::PureRegister);

        // `SVC #0`, which is not a hint and is not something M2 can serve.
        let facts = body_facts(&asm(&[0xD400_0001, RET_X30]), base, None);
        assert_eq!((facts.hints, facts.system_instructions), (0, 1));
        assert_eq!(facts.kind(), LeafKind::NotALeaf);

        // BLR X8 and BR X8.
        for word in [0xD63F_0100u32, 0xD61F_0100] {
            let facts = body_facts(&asm(&[word, RET_X30]), base, None);
            assert_eq!(facts.indirect_transfers, 1, "{word:#010x}");
            assert_eq!(facts.kind(), LeafKind::NotALeaf);
        }

        // RET X8: a return to a register the caller never set.
        let facts = body_facts(&asm(&[0xD65F_0100]), base, None);
        assert_eq!(facts.returns_via_other_register, 1);
        assert_eq!(facts.returns, 0);
        assert_eq!(facts.kind(), LeafKind::NotALeaf);
    }

    #[test]
    fn a_body_with_no_return_at_all_is_not_a_leaf() {
        // A tail of NOP-free arithmetic that simply runs off the end.
        let facts = body_facts(&asm(&[0x9100_0400, 0x9100_0400]), 0x1000, None);
        assert_eq!(facts.returns, 0);
        assert_eq!(facts.kind(), LeafKind::NotALeaf, "a body that never returns is not runnable");
    }

    #[test]
    fn stack_only_and_thread_pointer_bodies_are_graded_apart() {
        // STR X29, [SP, #8] ; LDR X29, [SP, #8] ; RET
        let stack = asm(&[0xF900_07FD, 0xF940_07FD, RET_X30]);
        let facts = body_facts(&stack, 0x1000, None);
        assert_eq!(facts.memory_bases.iter().copied().collect::<Vec<_>>(), vec![31]);
        assert_eq!(facts.kind(), LeafKind::StackOnly);

        // MRS X8, TPIDR_EL0 ; LDR X9, [X8, #0x28] ; STR X9, [SP, #8] ; RET
        let tls = asm(&[0xD53B_D048, 0xF940_1509, 0xF900_07E9, RET_X30]);
        let facts = body_facts(&tls, 0x1000, None);
        assert_eq!(facts.thread_pointer_reads, 1);
        assert_eq!(facts.stack_guard_loads, 1, "{facts:?}");
        assert_eq!(facts.kind(), LeafKind::StackAndThreadPointer);

        // The same `LDR` off a register that is *not* a thread pointer is not a stack-guard load.
        // Without this, `LDR X9, [X8, #0x28]` after `ADD X8, SP, #0x10` would be counted, and the
        // D13 grade would be handed to functions that never read the thread pointer.
        let not_tls = asm(&[0x9100_43E8, 0xF940_1509, RET_X30]);
        let facts = body_facts(&not_tls, 0x1000, None);
        assert_eq!(facts.thread_pointer_reads, 0);
        assert_eq!(facts.stack_guard_loads, 0);
        assert_eq!(facts.kind(), LeafKind::NotALeaf, "it loads through X8, which nobody set");

        // And an `MRS` whose value is overwritten before the load does not count either.
        let clobbered = asm(&[0xD53B_D048, 0x9100_43E8, 0xF940_1509, RET_X30]);
        let facts = body_facts(&clobbered, 0x1000, None);
        assert_eq!(facts.thread_pointer_reads, 1);
        assert_eq!(facts.stack_guard_loads, 0, "X8 was redefined by the ADD");
        assert_eq!(facts.kind(), LeafKind::NotALeaf);
    }

    /// The shape 45 of `libroblox.so`'s functions have, and the one M2 runs for D13: every path
    /// that returns is call-free, and the single `BL` sits past the last `RET`.
    #[test]
    fn a_stack_protector_tail_past_the_last_return_is_still_a_leaf_on_every_returning_path() {
        // This is `libroblox.so`'s function at 0x2872aac, word for word.
        let body = asm(&[
            0xD100_83FF, // SUB   SP, SP, #0x20
            0xA901_7BFD, // STP   X29, X30, [SP, #0x10]
            0x9100_43FD, // ADD   X29, SP, #0x10
            0xD53B_D048, // MRS   X8, TPIDR_EL0
            0xF940_1509, // LDR   X9, [X8, #0x28]
            0xF900_07E9, // STR   X9, [SP, #8]
            0xF940_1508, // LDR   X8, [X8, #0x28]
            0xF940_07E9, // LDR   X9, [SP, #8]
            0xEB09_011F, // CMP   X8, X9
            0x5400_00A1, // B.NE  +5
            0x5280_4000, // MOV   W0, #0x200
            0xA941_7BFD, // LDP   X29, X30, [SP, #0x10]
            0x9100_83FF, // ADD   SP, SP, #0x20
            RET_X30,     // RET
            0x9400_0000, // BL    __stack_chk_fail (offset stubbed to 0 for this test)
        ]);
        let facts = body_facts(&body, 0x2872ae8, None);
        assert_eq!(facts.kind(), LeafKind::StackGuardProtected, "{facts:?}");
        assert_eq!(facts.calls_before_last_return, 0, "the call is past the last RET");
        assert_eq!(facts.direct_calls.len(), 1);
        assert_eq!(facts.stack_guard_loads, 2, "the guard is read, stored, and read back");
        assert_eq!(facts.thread_pointer_reads, 1);
        assert!(facts.foreign_memory_bases.is_empty(), "only SP and the thread pointer");
        assert!(facts.returning_paths_are_call_free());

        // The same body with the call moved *before* the last return is a different claim
        // entirely, and must not get this grade. Swapping the last two words does it.
        let mut moved = body.clone();
        let n = moved.len();
        for i in 0..4 {
            moved.swap(n - 8 + i, n - 4 + i);
        }
        let facts = body_facts(&moved, 0x2872ae8, None);
        assert_eq!(facts.calls_before_last_return, 1);
        assert_eq!(facts.kind(), LeafKind::NotALeaf);
    }

    #[test]
    fn pc_relative_addressing_disqualifies_a_body() {
        // ADRP X8, #0 ; RET. This is how a relocated pointer is reached, so a body that does it is
        // not runnable before the relocation-consuming layer exists.
        let facts = body_facts(&asm(&[0x9000_0008, RET_X30]), 0x1000, None);
        assert_eq!(facts.pc_relative_addressing, 1);
        assert_eq!(facts.kind(), LeafKind::NotALeaf);
    }

    #[test]
    fn a_body_the_decoder_cannot_place_is_not_a_leaf() {
        // The UNALLOCATED top-level encoding space.
        let facts = body_facts(&asm(&[0x0000_0001, RET_X30]), 0x1000, None);
        assert_eq!(facts.undecodable, 1);
        assert!(!facts.fully_decoded());
        assert_eq!(facts.kind(), LeafKind::NotALeaf);

        // A body that is not a whole number of instructions.
        let facts = body_facts(&[0u8, 1, 2], 0x1000, None);
        assert_eq!(facts.kind(), LeafKind::NotALeaf);
        let facts = body_facts(&[], 0x1000, None);
        assert_eq!(facts.kind(), LeafKind::NotALeaf);
    }

    #[test]
    fn the_branch_offset_fields_are_sign_extended_at_the_right_width() {
        // B -1 word, B.cond -1 word, TBZ -1 word: the three immediate widths.
        assert_eq!(imm26(0x17FF_FFFF), -1);
        assert_eq!(imm26(0x1400_0001), 1);
        assert_eq!(imm26(0x1600_0000), -(1 << 25), "the most negative B offset");
        assert_eq!(imm19(0x54FF_FFE0), -1);
        assert_eq!(imm19(0x5400_0020), 1);
        assert_eq!(imm19(0x5480_0000), -(1 << 18), "the most negative B.cond offset");
        assert_eq!(imm14(0x36FF_FFE0), -1);
        assert_eq!(imm14(0x3600_0020), 1);
        assert_eq!(imm14(0x3604_0000), -(1 << 13), "the most negative TBZ offset");
        assert_eq!(branch_target(0x1000, -1), 0x0FFC);
    }

    /// **Global Constraint 11, on the classifier itself.** The bytes it decodes come from a 109 MB
    /// file this project already knows is adversarially modified (D6), and a classifier that
    /// panicked on a hostile body would turn a static scan into a crash.
    ///
    /// Two properties over a deterministic sweep of the whole encoding space at a stride that hits
    /// every top-level class many times over:
    ///
    /// 1. it never panics, whatever the bytes are;
    /// 2. **no body containing a call or an indirect transfer is ever graded a leaf**, which is the
    ///    one direction a misclassification must not go — a function graded a leaf that is not one
    ///    sends guest code into an unrelocated PLT stub.
    #[test]
    fn no_sequence_of_bytes_can_make_the_classifier_panic_or_admit_a_call() {
        const RET_X30: u32 = 0xD65F_03C0;
        let mut examined = 0u64;
        let mut leaves = 0u64;
        // A stride that is coprime with every power of two below 2^32, so the sweep walks the whole
        // space rather than a slice of it, and a fixed count so the test is deterministic.
        let mut word: u32 = 0;
        for _ in 0..200_000u32 {
            word = word.wrapping_add(0x0001_9E3F).rotate_left(7) ^ 0x5BF0_3635;
            for tail in [RET_X30, word.rotate_right(11), 0] {
                let body = asm(&[word, word.swap_bytes(), tail, RET_X30]);
                let facts = body_facts(&body, 0x1000, None);
                examined += 1;
                let kind = facts.kind();
                if kind != LeafKind::NotALeaf {
                    leaves += 1;
                    assert!(
                        facts.direct_calls.is_empty()
                            && facts.indirect_transfers == 0
                            && facts.escaping_branches == 0,
                        "{word:#010x} was graded {kind:?} while reaching outside its body: {facts:?}"
                    );
                }
            }
        }
        assert_eq!(examined, 600_000);
        // The sweep has to find *some* leaves, or the property above is vacuous.
        assert!(leaves > 1_000, "only {leaves} of {examined} bodies graded as leaves");
    }

    /// A body of arbitrary length, including lengths that are not whole instructions, is answered
    /// rather than indexed past the end.
    #[test]
    fn a_body_of_any_length_is_answered_rather_than_indexed_past_the_end() {
        let bytes: Vec<u8> = (0..=255u8).cycle().take(1024).collect();
        for len in 0..bytes.len() {
            let facts = body_facts(&bytes[..len], u64::MAX - 4096, None);
            // Only the claim matters here: it returns, and a partial instruction is not a leaf.
            if len % 4 != 0 || len == 0 {
                assert_eq!(facts.kind(), LeafKind::NotALeaf, "length {len}");
            }
        }
        // A body that starts at the very top of the address space: the end arithmetic must not
        // wrap into a range that contains everything.
        let facts = body_facts(&[0u8; 8], u64::MAX - 8, None);
        assert_eq!(facts.undecodable, 2, "0x0000_0000 is the reserved encoding, twice");
    }

    /// The derived bound on the whole map, and the margin a real object has under it.
    #[test]
    fn a_function_map_describing_more_code_than_the_object_holds_is_refused() {
        // `libroblox.so`'s real figures: the map must fit, with room to spare.
        require_map_fits(69_943_828, 103_645_584).expect("a real object");
        // Exactly full is fine; one byte more is not.
        require_map_fits(103_645_584, 103_645_584).expect("exactly full");
        let error = require_map_fits(103_645_585, 103_645_584).expect_err("one byte over");
        assert!(error.to_string().contains("do not describe this object"), "{error}");
        // The shape a corrupted encoding produces: lengths that are really addresses.
        assert!(require_map_fits(u64::MAX, 103_645_584).is_err());
        // An object with no executable segment has no room for any function at all.
        assert!(require_map_fits(1, 0).is_err());
        require_map_fits(0, 0).expect("an empty map in an empty object");
    }

    #[test]
    fn a_relocation_landing_in_the_body_disqualifies_it() {
        let relocations =
            TextRelocations { offsets: vec![0x0FF8, 0x1004, 0x2000], examined: 3 };
        assert_eq!(relocations.count_in(0x1000, 0x1010), 1);
        assert_eq!(relocations.count_in(0x1008, 0x1010), 0);
        assert_eq!(relocations.count_in(0x0000, 0x4000), 3);
        assert_eq!(relocations.total(), 3);

        let body = asm(&[0x9100_0400, RET_X30]);
        let facts = body_facts(&body, 0x1000, Some(&relocations));
        assert_eq!(facts.relocated_words, 1);
        assert_eq!(
            facts.kind(),
            LeafKind::NotALeaf,
            "a body a relocation patches is not the body in the file"
        );
        // And the same body one word further on, which nothing patches.
        let facts = body_facts(&body, 0x1008, Some(&relocations));
        assert_eq!(facts.relocated_words, 0);
        assert_eq!(facts.kind(), LeafKind::PureRegister);
    }
}
