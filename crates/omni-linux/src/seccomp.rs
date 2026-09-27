//! seccomp: a process's system-call filters, as the kernel runs them. A filter is a classic BPF
//! program over `struct seccomp_data` (the call's number, the arch, the instruction pointer and
//! the six arguments); every filter installed runs on every call from then on, and the most
//! restrictive answer wins -- kill, a SIGSYS trap, an errno, or allow. Filters are kept across
//! `fork` and `execve`, as `no_new_privs` is, which installing one requires (or CAP_SYS_ADMIN).
//! minijail installs one in the media daemons.
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::errno::{Errno, EINVAL};

pub const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;

const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_KILL_THREAD: u32 = 0;
const RET_TRAP: u32 = 0x0003_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_USER_NOTIF: u32 = 0x7fc0_0000;
const RET_TRACE: u32 = 0x7ff0_0000;
const RET_LOG: u32 = 0x7ffc_0000;
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ACTION_FULL: u32 = 0xffff_0000;
const RET_DATA: u32 = 0xffff;

/// The most instructions a filter may have (`BPF_MAXINSNS`).
const MAX_INSNS: usize = 4096;
/// `sizeof(struct seccomp_data)`.
const DATA_LEN: u32 = 64;

#[derive(Clone, Copy, Debug)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// What a filter answers for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Fail the call with this errno.
    Errno(u16),
    /// Do not make the call; deliver SIGSYS (`SYS_SECCOMP`) with this in `si_errno`.
    Trap(u16),
    /// End the process with SIGSYS.
    Kill,
    /// A tracer or a listener would decide; there is none: the call fails with `ENOSYS`.
    Unavailable,
}

/// A process's filters.
#[derive(Default)]
pub struct Seccomp {
    filters: Mutex<Vec<Arc<Vec<Insn>>>>,
    /// Whether any filter is installed (read on every call).
    on: AtomicBool,
    /// `SECCOMP_MODE_*`: 0 none, 1 strict, 2 filter.
    mode: AtomicU8,
    /// `PR_SET_NO_NEW_PRIVS`.
    pub no_new_privs: AtomicBool,
}

impl Seccomp {
    /// A child's or an executed image's: the same filters and `no_new_privs`.
    pub fn inherit(&self, from: &Seccomp) {
        *self.filters.lock() = from.filters.lock().clone();
        self.mode.store(from.mode.load(Ordering::SeqCst), Ordering::SeqCst);
        self.on.store(from.on.load(Ordering::SeqCst), Ordering::SeqCst);
        self.no_new_privs.store(from.no_new_privs.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    #[must_use]
    pub fn active(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// `SECCOMP_MODE_*` (`PR_GET_SECCOMP`).
    #[must_use]
    pub fn mode(&self) -> u8 {
        self.mode.load(Ordering::SeqCst)
    }

    /// `SECCOMP_SET_MODE_STRICT`: only `read`, `write`, `exit` and `rt_sigreturn` from now on.
    ///
    /// # Errors
    /// `EINVAL` once a filter is installed.
    pub fn set_strict(&self) -> Result<(), Errno> {
        if self.mode() == 2 {
            return Err(EINVAL);
        }
        self.mode.store(1, Ordering::SeqCst);
        self.on.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Install a filter (`struct sock_filter` array bytes).
    ///
    /// # Errors
    /// `EINVAL` for a program the kernel would refuse (empty, too long, an instruction seccomp does
    /// not allow, a jump out of it, a load beyond `seccomp_data`, not ending in a return), or in
    /// strict mode.
    pub fn install(&self, bytes: &[u8]) -> Result<(), Errno> {
        if self.mode() == 1 {
            return Err(EINVAL);
        }
        let prog = parse(bytes)?;
        self.filters.lock().push(Arc::new(prog));
        self.mode.store(2, Ordering::SeqCst);
        self.on.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// What the filters answer for call `nr` made at `ip` with `args`.
    #[must_use]
    pub fn check(&self, nr: u64, ip: u64, args: &[u64; 6]) -> Verdict {
        if self.mode() == 1 {
            use crate::syscall::nr as n;
            return if matches!(nr, n::READ | n::WRITE | n::EXIT | n::RT_SIGRETURN) { Verdict::Allow } else { Verdict::Kill };
        }
        let mut data = [0u8; DATA_LEN as usize];
        data[0..4].copy_from_slice(&(nr as u32).to_le_bytes());
        data[4..8].copy_from_slice(&AUDIT_ARCH_AARCH64.to_le_bytes());
        data[8..16].copy_from_slice(&ip.to_le_bytes());
        for (i, a) in args.iter().enumerate() {
            data[16 + i * 8..24 + i * 8].copy_from_slice(&a.to_le_bytes());
        }
        // Every filter runs, newest first; the most restrictive action wins (the lowest, as a
        // signed number: kill-process is the most restrictive).
        let mut ret = RET_ALLOW;
        for f in self.filters.lock().iter().rev() {
            let r = run(f, &data);
            if ((r & RET_ACTION_FULL) as i32) < ((ret & RET_ACTION_FULL) as i32) {
                ret = r;
            }
        }
        match ret & RET_ACTION_FULL {
            RET_ALLOW | RET_LOG => Verdict::Allow,
            RET_ERRNO => Verdict::Errno(((ret & RET_DATA) as u16).min(4095)),
            RET_TRAP => Verdict::Trap((ret & RET_DATA) as u16),
            RET_USER_NOTIF | RET_TRACE => Verdict::Unavailable,
            RET_KILL_PROCESS | RET_KILL_THREAD => Verdict::Kill,
            // An action the kernel does not know is taken as kill-process.
            _ => Verdict::Kill,
        }
    }
}

/// Whether `action` (`SECCOMP_GET_ACTION_AVAIL`) is one this kernel offers.
#[must_use]
pub fn action_available(action: u32) -> bool {
    matches!(action, RET_KILL_PROCESS | RET_KILL_THREAD | RET_TRAP | RET_ERRNO | RET_LOG | RET_ALLOW)
}

fn parse(bytes: &[u8]) -> Result<Vec<Insn>, Errno> {
    if bytes.is_empty() || bytes.len() % 8 != 0 || bytes.len() / 8 > MAX_INSNS {
        return Err(EINVAL);
    }
    let prog: Vec<Insn> = bytes
        .chunks_exact(8)
        .map(|c| Insn { code: u16::from_le_bytes([c[0], c[1]]), jt: c[2], jf: c[3], k: u32::from_le_bytes([c[4], c[5], c[6], c[7]]) })
        .collect();
    let len = prog.len();
    for (pc, i) in prog.iter().enumerate() {
        let after = pc + 1;
        let ok = match i.code {
            // BPF_LD|BPF_W|BPF_ABS: an aligned word of seccomp_data.
            0x20 => i.k % 4 == 0 && i.k < DATA_LEN,
            // BPF_LD|BPF_W|BPF_LEN, BPF_LDX|BPF_W|BPF_LEN, BPF_LD|BPF_IMM, BPF_LDX|BPF_IMM.
            0x80 | 0x81 | 0x00 | 0x01 => true,
            // BPF_LD|BPF_MEM, BPF_LDX|BPF_MEM, BPF_ST, BPF_STX: scratch M[0..16].
            0x60 | 0x61 | 0x02 | 0x03 => i.k < 16,
            // BPF_ALU: add sub mul div or and lsh rsh neg mod xor, K or X; no division by a zero K.
            c if c & 0x07 == 0x04 => {
                let op = c & 0xf0;
                matches!(op, 0x00 | 0x10 | 0x20 | 0x30 | 0x40 | 0x50 | 0x60 | 0x70 | 0x80 | 0x90 | 0xa0)
                    && !(matches!(op, 0x30 | 0x90) && c & 0x08 == 0 && i.k == 0)
            }
            // BPF_JMP|BPF_JA: forward, within the program.
            0x05 => (after as u64 + u64::from(i.k)) < len as u64,
            // BPF_JMP conditionals: jeq jgt jge jset, K or X.
            c if c & 0x07 == 0x05 => matches!(c & 0xf0, 0x10 | 0x20 | 0x30 | 0x40) && after + (i.jt as usize) < len && after + (i.jf as usize) < len,
            // BPF_RET|BPF_K, BPF_RET|BPF_A.
            0x06 | 0x16 => true,
            // BPF_MISC: tax, txa.
            0x07 | 0x87 => true,
            _ => false,
        };
        if !ok {
            return Err(EINVAL);
        }
    }
    if !matches!(prog[len - 1].code, 0x06 | 0x16) {
        return Err(EINVAL);
    }
    Ok(prog)
}

/// Run a checked program over `data`.
fn run(prog: &[Insn], data: &[u8; DATA_LEN as usize]) -> u32 {
    let (mut a, mut x) = (0u32, 0u32);
    let mut m = [0u32; 16];
    let mut pc = 0usize;
    loop {
        let i = prog[pc];
        pc += 1;
        match i.code {
            0x20 => a = u32::from_le_bytes(data[i.k as usize..i.k as usize + 4].try_into().expect("4")),
            0x80 => a = DATA_LEN,
            0x81 => x = DATA_LEN,
            0x00 => a = i.k,
            0x01 => x = i.k,
            0x60 => a = m[i.k as usize],
            0x61 => x = m[i.k as usize],
            0x02 => m[i.k as usize] = a,
            0x03 => m[i.k as usize] = x,
            c if c & 0x07 == 0x04 => {
                let v = if c & 0x08 != 0 { x } else { i.k };
                a = match c & 0xf0 {
                    0x00 => a.wrapping_add(v),
                    0x10 => a.wrapping_sub(v),
                    0x20 => a.wrapping_mul(v),
                    // Division by a zero X ends the program with 0.
                    0x30 => match a.checked_div(v) {
                        Some(q) => q,
                        None => return 0,
                    },
                    0x40 => a | v,
                    0x50 => a & v,
                    0x60 => a.checked_shl(v).unwrap_or(0),
                    0x70 => a.checked_shr(v).unwrap_or(0),
                    0x80 => a.wrapping_neg(),
                    0x90 => match a.checked_rem(v) {
                        Some(r) => r,
                        None => return 0,
                    },
                    _ => a ^ v,
                };
            }
            0x05 => pc += i.k as usize,
            c if c & 0x07 == 0x05 => {
                let v = if c & 0x08 != 0 { x } else { i.k };
                let taken = match c & 0xf0 {
                    0x10 => a == v,
                    0x20 => a > v,
                    0x30 => a >= v,
                    _ => a & v != 0,
                };
                pc += if taken { i.jt } else { i.jf } as usize;
            }
            0x06 => return i.k,
            0x16 => return a,
            0x07 => x = a,
            _ => a = x, // 0x87: txa
        }
    }
}
