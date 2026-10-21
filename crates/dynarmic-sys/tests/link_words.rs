//! **A link rewrites one word, and only that word is made visible to instruction fetch** (vendored
//! patch 0114, arm64).
//!
//! A block's links to other blocks are the only bytes of it that change after it is emitted. A
//! direct link is one instruction word (`B` linked, `NOP` not). The return-stack push was an `ADRL`
//! -- two instruction words, rewritten one at a time, which a thread running the block while
//! another relinked it (a cache shared between threads) could execute half old and half new. It is
//! now one `LDR` of an 8-byte data slot placed after the block's code: linking stores the predicted
//! block's entry point there in one aligned store, unlinking the dispatcher's. And a relink makes
//! each word it rewrote visible to instruction fetch, by its address, instead of every linking
//! block whole; a slot, being data, needs nothing.
//!
//! `od_jit_block_links` reports a block's link sites and how many bytes of code relinks have handed
//! to instruction-cache maintenance. The first test pins the shape: it fails with the `ADRL` put
//! back (the slot holds instructions) and with whole-block maintenance put back (240 bytes, not
//! 4). The second checks the per-word maintenance does its job: on the M4 it fails at its second
//! round with the maintenance left out, or aimed a line off, and passes with the `IC IVAU`s alone
//! (2026-10-20, by hand).
#![cfg(target_arch = "aarch64")]

mod harness;

use dynarmic_sys::{od_jit_block_links, od_jit_invalidate_range, OdBlockLinks, OD_LINK_BRANCH, OD_LINK_RSB_SLOT};
use harness::{a64, Vm, VmOptions, CODE_BASE, HALT_DONE};

/// A block as `od_jit_block_links` reports it, with its bytes as they are now.
struct Block {
    info: OdBlockLinks,
    /// `(offset, kind)` of each link site, ascending.
    sites: Vec<(u32, u32)>,
    bytes: Vec<u8>,
}

impl Block {
    fn of(vm: &Vm, pc: u64) -> Self {
        let mut info = OdBlockLinks::default();
        let mut offsets = [0u32; 16];
        let mut kinds = [0u32; 16];
        // SAFETY: the jit is live and not executing, on this thread; the arrays hold 16 each.
        unsafe { od_jit_block_links(vm.raw(), pc, &mut info, offsets.as_mut_ptr(), kinds.as_mut_ptr(), 16) };
        assert_ne!(info.entry_point, 0, "no block at {pc:#x}");
        assert!(info.count <= 16);
        let mut sites: Vec<(u32, u32)> = offsets.iter().copied().zip(kinds.iter().copied()).take(info.count as usize).collect();
        sites.sort_unstable();
        // SAFETY: the cache is readable (R-X) and holds `host_bytes` of this block at its entry.
        let bytes = unsafe { std::slice::from_raw_parts(info.entry_point as *const u8, info.host_bytes as usize) }.to_vec();
        Self { info, sites, bytes }
    }

    fn site(&self, kind: u32) -> u32 {
        let found: Vec<u32> = self.sites.iter().filter(|s| s.1 == kind).map(|s| s.0).collect();
        assert_eq!(found.len(), 1, "one site of kind {kind}: {:?}", self.sites);
        found[0]
    }

    fn word(&self, offset: u32) -> u32 {
        u32::from_le_bytes(self.bytes[offset as usize..offset as usize + 4].try_into().unwrap())
    }

    fn slot(&self, offset: u32) -> u64 {
        u64::from_le_bytes(self.bytes[offset as usize..offset as usize + 8].try_into().unwrap())
    }

    /// Where the branch word at `offset` goes: `Some(host address)` for `B`, `None` for `NOP`.
    fn branch_target(&self, offset: u32) -> Option<u64> {
        let w = self.word(offset);
        if w == a64::NOP {
            return None;
        }
        assert_eq!(w & 0xFC00_0000, 0x1400_0000, "a link word is B or NOP: {w:#010x}");
        let imm = (((w & 0x03FF_FFFF) << 6) as i32 >> 6) as i64 * 4;
        Some((self.info.entry_point + u64::from(offset)).wrapping_add(imm as u64))
    }

    /// The block's bytes with every link word and slot blanked: what must never change.
    fn fixed_bytes(&self) -> Vec<u8> {
        let mut b = self.bytes.clone();
        for &(offset, kind) in &self.sites {
            let len = if kind == OD_LINK_BRANCH { 4 } else { 8 };
            b[offset as usize..offset as usize + len].fill(0);
        }
        b
    }
}

/// FNV-1a, enough to say two byte strings differ.
fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn invalidate(vm: &Vm, word: u64, words: u64) {
    // SAFETY: a live jit that is not running; applied when it next runs.
    unsafe { od_jit_invalidate_range(vm.raw(), CODE_BASE + 4 * word, 4 * words) };
}

fn run_from(vm: &Vm, word: u64) {
    vm.start(u64::MAX);
    vm.set_pc(CODE_BASE + 4 * word);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE);
}

/// Block A (words 0-2) calls F (8-9) and so links to it twice: its `B` to F, and its return-stack
/// slot predicting R (3-4), where F returns. The test reads A linked (after a run), unlinked (F and
/// R invalidated), and linked again to the new translations of F and R. Outside its one link word
/// and its one slot A's bytes never change; the word is `B F` or `NOP`, the slot F's return site's
/// entry or the dispatcher; the slot is 8-aligned data after A's last instruction; and each relink
/// hands 4 bytes -- the word -- to instruction-cache maintenance, not A's whole block.
#[test]
fn a_link_rewrites_one_word_and_nothing_else() {
    assert_eq!(std::mem::size_of::<OdBlockLinks>(), 40, "od_block_links");
    let code = vec![
        a64::add_imm(2, 2, 3), // 0  A
        a64::add_imm(3, 3, 5), // 1
        a64::bl(6),            // 2  -> F
        a64::add_imm(1, 1, 1), // 3  R
        a64::svc(0),           // 4
        a64::NOP,              // 5
        a64::svc(0),           // 6  I: runs nothing of A's, so an invalidation is applied alone
        a64::NOP,              // 7
        a64::add_imm(0, 0, 1), // 8  F
        a64::ret(30),          // 9
    ];
    let vm = Vm::new(code, VmOptions::default());
    let (a, f, r) = (CODE_BASE, CODE_BASE + 4 * 8, CODE_BASE + 4 * 3);

    run_from(&vm, 0);
    assert_eq!((vm.reg(0), vm.reg(1)), (1, 1));
    let linked = Block::of(&vm, a);
    let (word, slot) = (linked.site(OD_LINK_BRANCH), linked.site(OD_LINK_RSB_SLOT));
    assert_eq!(linked.sites.len(), 2, "{:?}", linked.sites);
    let (f1, r1) = (Block::of(&vm, f).info.entry_point, Block::of(&vm, r).info.entry_point);
    assert_eq!(linked.branch_target(word), Some(f1), "A's word branches to F");
    assert_eq!(linked.slot(slot), r1, "A's slot holds R's entry");
    assert_eq!((linked.info.entry_point + u64::from(slot)) % 8, 0, "the slot is 8-aligned");
    assert_eq!(slot as usize + 8, linked.bytes.len(), "the slot ends the block");
    assert_eq!(linked.word(slot - 4), a64::BRK, "nothing runs into the slot");
    assert!(word < slot);

    // F and R invalidated; applied as I runs: A is unlinked from both.
    let before = linked.info.relink_maintained;
    invalidate(&vm, 8, 2);
    invalidate(&vm, 3, 2);
    run_from(&vm, 6);
    let unlinked = Block::of(&vm, a);
    assert_eq!(unlinked.info.entry_point, linked.info.entry_point);
    assert_eq!(unlinked.sites, linked.sites);
    assert_eq!(unlinked.branch_target(word), None, "unlinked, A's word is a NOP");
    assert_eq!(unlinked.slot(slot), unlinked.info.unlinked_slot, "unlinked, A's slot holds the dispatcher");
    assert_ne!(unlinked.info.unlinked_slot, 0);
    assert_eq!(
        unlinked.info.relink_maintained - before,
        4,
        "unlinking A from F maintains its one word, and from R nothing (the slot is data)"
    );

    // A runs again: F and R are translated anew, after everything else, and A linked to them.
    let before = unlinked.info.relink_maintained;
    run_from(&vm, 0);
    assert_eq!((vm.reg(0), vm.reg(1)), (2, 2));
    let relinked = Block::of(&vm, a);
    let (f2, r2) = (Block::of(&vm, f).info.entry_point, Block::of(&vm, r).info.entry_point);
    assert!(f2 > f1 && r2 > r1, "new translations: F {f1:#x} -> {f2:#x}, R {r1:#x} -> {r2:#x}");
    assert_eq!(relinked.branch_target(word), Some(f2));
    assert_eq!(relinked.slot(slot), r2);
    assert_eq!(relinked.info.relink_maintained - before, 4, "linking A to the new F maintains its one word");

    let fixed = hash(&linked.fixed_bytes());
    assert_eq!(hash(&unlinked.fixed_bytes()), fixed, "unlinking changed A outside its link word and slot");
    assert_eq!(hash(&relinked.fixed_bytes()), fixed, "relinking changed A outside its link word and slot");
    assert_ne!(hash(&linked.bytes), hash(&unlinked.bytes));
}

/// A block linked to F, F rewritten and translated again, the block relinked to the new F -- 2,000
/// times, the block running both before the relink (unlinked: its word a `NOP`, so the dispatcher)
/// and after it (its word `B` to the new F). A rewritten word that instruction fetch still saw as it
/// was -- `B` to the F of the round before, whose code is still in the cache -- would return that
/// round's value. Each round's value is checked.
#[test]
fn a_relinked_branch_never_runs_the_code_it_named_before() {
    const ROUNDS: u16 = 2_000;
    const NE: u32 = 1;
    let code = vec![
        a64::bl(5),             // 0  A: -> F
        a64::subs_imm(5, 5, 1), // 1  F returns here: twice a run, then on
        a64::b_cond(NE, -2),    // 2  -> A
        a64::svc(0),            // 3
        a64::NOP,               // 4
        a64::NOP,               // 5  F, rewritten every round: MOVZ X0, #round
        a64::ret(30),           // 6
    ];
    let vm = Vm::new(code, VmOptions::default());
    for round in 1..=ROUNDS {
        vm.with_ctx(|c| c.code[5] = a64::movz(0, round, 0));
        invalidate(&vm, 5, 1);
        vm.set_reg(5, 2);
        vm.set_reg(0, 0);
        run_from(&vm, 0);
        assert_eq!(vm.reg(0), u64::from(round), "round {round} ran the F of round {}", vm.reg(0));
    }
}
