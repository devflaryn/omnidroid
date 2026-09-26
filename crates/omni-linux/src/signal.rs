//! The arm64 signal frame (milestone A5), laid out as the Linux kernel lays it out
//! (`arch/arm64/kernel/signal.c`, `uapi/asm/sigcontext.h`, `uapi/asm/ucontext.h`), so bionic's
//! `__restore_rt`, its unwinder and ART's fault handler read what they expect:
//!
//! ```text
//! rt_sigframe:  siginfo (128) | ucontext
//! ucontext:     uc_flags (8) | uc_link (8) | uc_stack (24) | uc_sigmask (8) | pad to 176 | uc_mcontext
//! sigcontext:   fault_address | regs[31] | sp | pc | pstate | pad to 288 | __reserved[4096]
//! __reserved:   fpsimd_context { magic 0x46508001, size 528, fpsr, fpcr, vregs[32] } | null record
//! then a frame record { x29, x30 } the handler's x29 points at
//! ```

/// `siginfo_t`.
pub const SIGINFO_BYTES: usize = 128;
/// `uc_mcontext` within `ucontext`.
pub const MCONTEXT_OFFSET: usize = 176;
/// `__reserved` within `sigcontext`.
pub const RESERVED_OFFSET: usize = 288;
const RESERVED_BYTES: usize = 4096;
const UCONTEXT_BYTES: usize = MCONTEXT_OFFSET + RESERVED_OFFSET + RESERVED_BYTES;
/// The whole frame: `rt_sigframe` and the frame record after it.
pub const FRAME_BYTES: usize = SIGINFO_BYTES + UCONTEXT_BYTES + 16;
/// `ucontext` within the frame.
pub const UCONTEXT_OFFSET: usize = SIGINFO_BYTES;
/// The frame record within the frame.
pub const RECORD_OFFSET: usize = SIGINFO_BYTES + UCONTEXT_BYTES;

const FPSIMD_MAGIC: u32 = 0x4650_8001;
const FPSIMD_BYTES: u32 = 528;
const SS_ONSTACK: i32 = 1;
const SS_DISABLE: i32 = 2;

/// A task's registers, as a signal frame saves and restores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Regs {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub pstate: u64,
    pub v: [u128; 32],
    pub fault_address: u64,
}

/// What `siginfo_t` carries here: the fault address for a fault, the sender for a kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SigInfo {
    pub signo: i32,
    pub code: i32,
    pub addr: u64,
    pub pid: i32,
    pub uid: u32,
}

/// `si_code` values used here.
pub const SI_USER: i32 = 0;
pub const SI_TKILL: i32 = -6;
pub const SEGV_MAPERR: i32 = 1;
pub const SEGV_ACCERR: i32 = 2;
pub const ILL_ILLOPC: i32 = 1;

/// A `stack_t` (24 bytes) with `SS_DISABLE`: no alternate stack.
#[must_use]
pub fn altstack_disabled() -> [u8; 24] {
    let mut s = [0u8; 24];
    s[8..12].copy_from_slice(&SS_DISABLE.to_le_bytes());
    s
}

/// A `stack_t` for an enabled alternate stack at `[sp, sp + size)`.
#[must_use]
pub fn altstack(sp: u64, size: u64) -> [u8; 24] {
    let mut s = [0u8; 24];
    s[0..8].copy_from_slice(&sp.to_le_bytes());
    s[16..24].copy_from_slice(&size.to_le_bytes());
    s
}

fn stack_parts(stack: [u8; 24]) -> (u64, i32, u64) {
    (
        u64::from_le_bytes(stack[0..8].try_into().expect("8")),
        i32::from_le_bytes(stack[8..12].try_into().expect("4")),
        u64::from_le_bytes(stack[16..24].try_into().expect("8")),
    )
}

/// Where the frame goes: the top of the alternate stack for an `SA_ONSTACK` handler when there is
/// one and the task is not already on it; otherwise below `sp`. 16-byte aligned.
#[must_use]
pub fn placement(sp: u64, altstack: [u8; 24], onstack: bool) -> u64 {
    let (ss_sp, flags, size) = stack_parts(altstack);
    let on_it = sp > ss_sp && sp <= ss_sp.wrapping_add(size);
    let top = if onstack && flags & SS_DISABLE == 0 && size != 0 && !on_it { ss_sp.wrapping_add(size) } else { sp };
    top.wrapping_sub(FRAME_BYTES as u64) & !15
}

/// The `uc_stack` a frame records: the alternate stack, marked `SS_ONSTACK` when `sp` is on it.
fn uc_stack(altstack: [u8; 24], sp: u64) -> [u8; 24] {
    let (ss_sp, flags, size) = stack_parts(altstack);
    let mut s = altstack;
    if flags & SS_DISABLE == 0 && sp > ss_sp && sp <= ss_sp.wrapping_add(size) {
        s[8..12].copy_from_slice(&SS_ONSTACK.to_le_bytes());
    }
    s
}

pub struct Frame;

impl Frame {
    /// The frame's bytes, to be written at the address `placement` chose.
    #[must_use]
    pub fn build(regs: &Regs, info: &SigInfo, mask: u64, altstack: [u8; 24]) -> Vec<u8> {
        let mut b = vec![0u8; FRAME_BYTES];
        let put = |b: &mut Vec<u8>, at: usize, bytes: &[u8]| b[at..at + bytes.len()].copy_from_slice(bytes);
        // siginfo
        put(&mut b, 0, &info.signo.to_le_bytes());
        put(&mut b, 8, &info.code.to_le_bytes());
        if info.code > 0 && matches!(info.signo, 4 | 5 | 7 | 8 | 11) {
            put(&mut b, 16, &info.addr.to_le_bytes()); // si_addr
        } else {
            put(&mut b, 16, &info.pid.to_le_bytes()); // si_pid
            put(&mut b, 20, &info.uid.to_le_bytes()); // si_uid
        }
        // ucontext
        let uc = UCONTEXT_OFFSET;
        put(&mut b, uc + 16, &uc_stack(altstack, regs.sp));
        put(&mut b, uc + 40, &mask.to_le_bytes());
        let mc = uc + MCONTEXT_OFFSET;
        put(&mut b, mc, &regs.fault_address.to_le_bytes());
        for (i, x) in regs.x.iter().enumerate() {
            put(&mut b, mc + 8 + i * 8, &x.to_le_bytes());
        }
        put(&mut b, mc + 8 + 31 * 8, &regs.sp.to_le_bytes());
        put(&mut b, mc + 8 + 32 * 8, &regs.pc.to_le_bytes());
        put(&mut b, mc + 8 + 33 * 8, &regs.pstate.to_le_bytes());
        let res = mc + RESERVED_OFFSET;
        put(&mut b, res, &FPSIMD_MAGIC.to_le_bytes());
        put(&mut b, res + 4, &FPSIMD_BYTES.to_le_bytes());
        // fpsr and fpcr at res + 8 and + 12: the CPU seam exposes neither, so they are 0.
        for (i, v) in regs.v.iter().enumerate() {
            put(&mut b, res + 16 + i * 16, &v.to_le_bytes());
        }
        // The terminator after the fpsimd record is already zero.
        // The frame record: the interrupted x29 and x30.
        put(&mut b, RECORD_OFFSET, &regs.x[29].to_le_bytes());
        put(&mut b, RECORD_OFFSET + 8, &regs.x[30].to_le_bytes());
        b
    }

    /// The registers and mask `rt_sigreturn` restores from a frame (what the handler may have
    /// changed in it is what the interrupted code gets back, as on Linux).
    #[must_use]
    pub fn parse(b: &[u8]) -> (Regs, u64) {
        let u64_at = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().expect("8"));
        let uc = UCONTEXT_OFFSET;
        let mc = uc + MCONTEXT_OFFSET;
        let mut regs = Regs { fault_address: u64_at(mc), ..Regs::default() };
        for (i, x) in regs.x.iter_mut().enumerate() {
            *x = u64_at(mc + 8 + i * 8);
        }
        regs.sp = u64_at(mc + 8 + 31 * 8);
        regs.pc = u64_at(mc + 8 + 32 * 8);
        regs.pstate = u64_at(mc + 8 + 33 * 8);
        let res = mc + RESERVED_OFFSET;
        if u32::from_le_bytes(b[res..res + 4].try_into().expect("4")) == FPSIMD_MAGIC {
            for (i, v) in regs.v.iter_mut().enumerate() {
                let at = res + 16 + i * 16;
                *v = u128::from_le_bytes(b[at..at + 16].try_into().expect("16"));
            }
        }
        (regs, u64_at(uc + 40))
    }
}
