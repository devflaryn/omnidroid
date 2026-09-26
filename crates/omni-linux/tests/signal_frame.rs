//! The arm64 signal frame (milestone A5), against the kernel's layout.
use omni_linux::signal::{self, Frame, Regs, SigInfo};

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn regs() -> Regs {
    let mut r = Regs::default();
    for (i, x) in r.x.iter_mut().enumerate() {
        *x = 0x1000 + i as u64;
    }
    r.sp = 0x7fff_0000;
    r.pc = 0x4000_1234;
    r.pstate = 0x6000_0000; // Z and C
    for (i, v) in r.v.iter_mut().enumerate() {
        *v = (0xabcd_0000_0000_0000_0000_0000_0000_0000u128) | i as u128;
    }
    r.fault_address = 0xdead_0000;
    r
}

#[test]
fn the_layout_is_the_kernels() {
    let info = SigInfo { signo: 11, code: 2, addr: 0xdead_0000, pid: 0, uid: 0 };
    let bytes = Frame::build(&regs(), &info, 0b1010, signal::altstack_disabled());
    assert_eq!(bytes.len(), signal::FRAME_BYTES);
    assert_eq!(signal::FRAME_BYTES % 16, 0);
    // siginfo
    assert_eq!(u32_at(&bytes, 0), 11, "si_signo");
    assert_eq!(u32_at(&bytes, 8), 2, "si_code");
    assert_eq!(u64_at(&bytes, 16), 0xdead_0000, "si_addr");
    // ucontext at 128: uc_sigmask at 40, uc_mcontext at 176
    let uc = 128;
    assert_eq!(u64_at(&bytes, uc + 40), 0b1010, "uc_sigmask");
    let mc = uc + 176;
    assert_eq!(u64_at(&bytes, mc), 0xdead_0000, "fault_address");
    assert_eq!(u64_at(&bytes, mc + 8 + 5 * 8), 0x1005, "regs[5]");
    assert_eq!(u64_at(&bytes, mc + 8 + 31 * 8), 0x7fff_0000, "sp");
    assert_eq!(u64_at(&bytes, mc + 8 + 32 * 8), 0x4000_1234, "pc");
    assert_eq!(u64_at(&bytes, mc + 8 + 33 * 8), 0x6000_0000, "pstate");
    // __reserved at sigcontext + 288: an fpsimd_context, then a null terminator
    let res = mc + 288;
    assert_eq!(u32_at(&bytes, res), 0x4650_8001, "FPSIMD_MAGIC");
    assert_eq!(u32_at(&bytes, res + 4), 528, "fpsimd_context size");
    let v3 = u128::from_le_bytes(bytes[res + 16 + 3 * 16..res + 16 + 4 * 16].try_into().unwrap());
    assert_eq!(v3, regs().v[3]);
    assert_eq!(u64_at(&bytes, res + 528), 0, "terminator");
}

#[test]
fn a_frame_parses_back_to_the_registers_and_mask_it_saved() {
    let info = SigInfo { signo: 10, code: -6, addr: 0, pid: 1000, uid: 10000 };
    let bytes = Frame::build(&regs(), &info, 0xff00, signal::altstack_disabled());
    let (back, mask) = Frame::parse(&bytes);
    assert_eq!(back, Regs { fault_address: back.fault_address, ..regs() });
    assert_eq!(mask, 0xff00);
}

#[test]
fn the_frame_goes_below_sp_or_to_the_top_of_the_alternate_stack() {
    let size = signal::FRAME_BYTES as u64;
    assert_eq!(signal::placement(0x1_0008, signal::altstack_disabled(), true), (0x1_0008 - size) & !15);
    let alt = signal::altstack(0x5000_0000, 0x1_0000);
    assert_eq!(signal::placement(0x1_0008, alt, true), (0x5001_0000 - size) & !15, "SA_ONSTACK moves it");
    assert_eq!(signal::placement(0x1_0008, alt, false), (0x1_0008 - size) & !15, "without SA_ONSTACK it stays");
    assert_eq!(
        signal::placement(0x5000_8000, alt, true),
        (0x5000_8000 - size) & !15,
        "already on the alternate stack: below sp there"
    );
}
