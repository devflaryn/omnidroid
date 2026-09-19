//! Tests for the `GuestContext` state plumbing, using a dedicated test double.
//!
//! Oracles: the trait's contract itself (errno round-trips, rand state round-trips, scratch
//! addresses are written verbatim into guest memory). No C library involved.

use omni_bionic::context::GuestContext;
use omni_bionic::memory::{Fault, GuestMemory};
use omni_bionic::mock::MockMemory;

/// A test double: real regions for memory, plain fields for the state hooks.
#[derive(Debug, Default)]
struct TestContext {
    mem: MockMemory,
    errno: i32,
    rand: u32,
    scratch_addr: Option<u64>,
    scratch_cap: usize,
}

impl GuestMemory for TestContext {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        self.mem.read(addr, buf)
    }
    fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault> {
        self.mem.write(addr, buf)
    }
}

impl GuestContext for TestContext {
    fn errno(&self) -> i32 {
        self.errno
    }
    fn set_errno(&mut self, value: i32) {
        self.errno = value;
    }
    fn rand_state(&self) -> u32 {
        self.rand
    }
    fn set_rand_state(&mut self, state: u32) {
        self.rand = state;
    }
    fn scratch(&mut self) -> Option<(u64, usize)> {
        match self.scratch_addr {
            Some(addr) => Some((addr, self.scratch_cap)),
            None => None,
        }
    }
}

#[test]
fn errno_round_trips_and_defaults_to_zero() {
    let mut ctx = TestContext::default();
    assert_eq!(ctx.errno(), 0);
    ctx.set_errno(34);
    assert_eq!(ctx.errno(), 34);
    ctx.set_errno(22);
    assert_eq!(ctx.errno(), 22);
}

#[test]
fn rand_state_round_trips() {
    let mut ctx = TestContext::default();
    ctx.set_rand_state(0x1234_5678);
    assert_eq!(ctx.rand_state(), 0x1234_5678);
    ctx.set_rand_state(u32::MAX);
    assert_eq!(ctx.rand_state(), u32::MAX);
}

#[test]
fn scratch_addr_is_writable_guest_memory() {
    let mut ctx = TestContext::default();
    ctx.mem.map(0x9000, &[0u8; 64]);
    ctx.scratch_addr = Some(0x9000);
    ctx.scratch_cap = 64;

    let (addr, cap) = ctx.scratch().expect("scratch configured");
    assert_eq!(addr, 0x9000);
    assert_eq!(cap, 64);
    // The contract: the crate writes the returned string through the same memory view.
    assert_eq!(ctx.write(addr, b"hi"), Ok(()));
    let mut out = [0u8; 2];
    assert_eq!(ctx.read(addr, &mut out), Ok(()));
    assert_eq!(&out, b"hi");
}

#[test]
fn missing_scratch_is_reported_and_not_fatal() {
    let mut ctx = TestContext::default();
    assert_eq!(ctx.scratch(), None);
}

#[test]
fn context_is_usable_where_guest_memory_is_expected() {
    // GuestContext has GuestMemory as a supertrait: a context must pass through any
    // function that takes &impl GuestMemory. This test fails to compile if that breaks.
    fn takes_memory(mem: &impl GuestMemory) -> Result<[u8; 2], Fault> {
        let mut out = [0u8; 2];
        mem.read(0x1000, &mut out)?;
        Ok(out)
    }
    let mut ctx = TestContext::default();
    ctx.mem.map(0x1000, &[7, 9]);
    assert_eq!(takes_memory(&ctx), Ok([7, 9]));
}
