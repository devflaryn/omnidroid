//! `omni-bionic`'s [`GuestMemory`], [`GuestAtomic`] and [`GuestContext`] over the boundary's
//! [`GuestMem`].
//!
//! # The two error vocabularies, and why the detail is not thrown away
//!
//! `omni-bionic` reports a failed access as [`Fault`], which is one address and nothing else —
//! deliberately, because that crate has no idea what a protection or a commit policy is. The
//! boundary reports the same failure as [`AbiError::BadPointer`], which names the symbol, the
//! argument, the length, the access and *which of `admit`'s three rules refused it*. Converting
//! the second into the first at the trait boundary and back again at the handler would lose all
//! of it and leave "fault at 0x…" to be read three thousand initializers deep.
//!
//! So the view keeps the rich error: a failed access stashes the real [`AbiError`] and returns
//! the thin `Fault` to `omni-bionic`, and the handler's [`GuestView::refuse`] hands the stashed
//! one back. A `Fault` that arrives with nothing stashed came from `omni-bionic`'s **own**
//! range check — a null pointer, or a length that would wrap the address space — and is
//! reported as [`AbiError::Refused`] saying so, rather than as a `BadPointer` with a length
//! nobody measured.
//!
//! # What identity mapping does and does not buy
//!
//! D4 makes a guest address a host address, so every access here is a `copy_nonoverlapping`
//! after [`omni_mem::admit`] has agreed to it. What it does **not** buy is a borrow: guest
//! memory is written by other guest threads, so nothing here ever forms a `&[u8]` into it.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU32, Ordering};

use omni_bionic::atomics::GuestAtomic;
use omni_bionic::context::GuestContext;
use omni_bionic::memory::{Fault, GuestMemory};
use omni_mem::GuestAddr;

use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

use super::Active;

/// Bytes of per-thread scratch a returned string may use.
///
/// `strerror` is the reachable caller and bionic's longest message is well under a hundred
/// bytes; 256 leaves room for the whole table and for `localeconv`'s structure later. A
/// returned string longer than this is refused by name rather than truncated, because a
/// truncated `strerror` is a plausible wrong answer.
pub const SCRATCH_BYTES: usize = 256;

/// Bytes of one guest `struct dl_phdr_info`.
///
/// **LP64 bionic's layout, field by field**, because getting it wrong hands the in-guest unwinder
/// a `dlpi_phdr` read out of the middle of `dlpi_name`:
///
/// | offset | bytes | field |
/// |---|---|---|
/// | 0 | 8 | `ElfW(Addr) dlpi_addr` |
/// | 8 | 8 | `const char *dlpi_name` |
/// | 16 | 8 | `const ElfW(Phdr) *dlpi_phdr` |
/// | 24 | 2 | `ElfW(Half) dlpi_phnum` |
/// | 26 | 6 | padding to the next 8-byte field |
/// | 32 | 8 | `unsigned long long dlpi_adds` |
/// | 40 | 8 | `unsigned long long dlpi_subs` |
/// | 48 | 8 | `size_t dlpi_tls_modid` |
/// | 56 | 8 | `void *dlpi_tls_data` |
///
/// **Derived from bionic's `link.h`, not verified against an NDK** — there is none on this
/// machine, which is the same gap `omni-bionic`'s `layouts.rs` records for `pthread_mutex_t`. Two
/// things make the derivation safe rather than merely plausible. The last four fields were added
/// in Android R and nothing has been added since, so 64 is the *largest* this structure has ever
/// been: a guest built against an older header reads a prefix of what is written here and never
/// reads past its own end. And `dl_iterate_phdr` passes this number to the callback as its `size`
/// argument, so a callback that checks before reading is told exactly how much is there.
pub const DL_PHDR_INFO_BYTES: usize = 64;

/// How many `dl_phdr_info` records one thread block holds.
///
/// One per level of boundary nesting, because the record is live *while the guest callback runs*
/// and that callback may call `dl_iterate_phdr` again. A single per-thread record would then be
/// overwritten underneath the outer iteration, which is a wrong answer rather than a crash — the
/// failure shape Global Constraint 1 is about. [`MAX_GUEST_DEPTH`](crate::MAX_GUEST_DEPTH) is the
/// cap on that nesting, and the `+ 1` is for depth zero.
pub const DL_INFO_SLOTS: usize = crate::boundary::MAX_GUEST_DEPTH + 1;

/// One guest thread's private block in the adapter's arena.
///
/// | offset | bytes | what |
/// |---|---|---|
/// | 0 | 4 | `errno`, an `int` |
/// | 4 | 4 | padding, so the locale handle is 8-byte aligned |
/// | 8 | 8 | this thread's `locale_t`, for `uselocale` |
/// | 16 | [`SCRATCH_BYTES`] | scratch for a returned string |
/// | 16 + [`SCRATCH_BYTES`] | [`DL_INFO_SLOTS`] × [`DL_PHDR_INFO_BYTES`] | one `dl_phdr_info` per nesting level |
///
/// **The locale handle fits in padding that already existed**, which is why the M3 gate's
/// `uselocale` cost this arena nothing: bytes 4-15 were there only so the scratch buffer starts
/// 16-byte aligned, and eight of them at offset 8 are aligned for a `locale_t`. [`ARENA_BYTES`]
/// and with it the one-commit-granule relation are therefore unchanged.
///
/// [`ARENA_BYTES`]: super::ARENA_BYTES
pub const THREAD_BLOCK_BYTES: usize =
    16 + SCRATCH_BYTES + DL_INFO_SLOTS * DL_PHDR_INFO_BYTES;

/// Offset of `errno` inside a thread block.
pub const ERRNO_OFFSET: usize = 0;
/// Offset of this thread's `locale_t` inside a thread block.
///
/// Eight rather than four: `locale_t` is a pointer and the guest reads it as one. See the table
/// above for why this needed no more arena.
pub const LOCALE_OFFSET: usize = 8;
/// Offset of the scratch buffer inside a thread block.
pub const SCRATCH_OFFSET: usize = 16;
/// Offset of the first `dl_phdr_info` record inside a thread block.
pub const DL_INFO_OFFSET: usize = 16 + SCRATCH_BYTES;

/// Guest memory and guest process state, as one bionic call sees them.
pub struct GuestView<'a> {
    mem: &'a GuestMem,
    symbol: &'a str,
    address: GuestAddr,
    /// The instance state and this thread's identity and block.
    pub active: &'a Active,
    /// Which argument the next access should be blamed on. A cursor because the bionic
    /// functions take their pointers as plain `u64` and cannot say which parameter one was.
    argument: Cell<usize>,
    /// The last refusal, kept so the thin `Fault` can be turned back into it.
    refusal: RefCell<Option<AbiError>>,
}

impl<'a> GuestView<'a> {
    /// A view for one call.
    #[must_use]
    pub fn new(
        mem: &'a GuestMem,
        symbol: &'a str,
        address: GuestAddr,
        active: &'a Active,
    ) -> Self {
        Self {
            mem,
            symbol,
            address,
            active,
            argument: Cell::new(0),
            refusal: RefCell::new(None),
        }
    }

    /// Blame the accesses that follow on argument `n`.
    pub fn blaming(&self, n: usize) -> &Self {
        self.argument.set(n);
        self
    }

    fn blame(&self) -> Blame<'_> {
        Blame::new(self.symbol, self.address, self.argument.get())
    }

    /// The boundary's checked memory, for a handler that needs it directly.
    #[must_use]
    pub fn mem(&self) -> &'a GuestMem {
        self.mem
    }

    /// The symbol this call is for.
    #[must_use]
    pub fn symbol(&self) -> &'a str {
        self.symbol
    }

    /// Its thunk address.
    #[must_use]
    pub fn address(&self) -> GuestAddr {
        self.address
    }

    /// Refuse this call, naming the symbol, the address and what would have had to be guessed.
    #[must_use]
    pub fn refusal(&self, why: impl Into<String>) -> AbiError {
        AbiError::Refused {
            symbol: self.symbol.to_string(),
            address: self.address,
            why: why.into(),
        }
    }

    /// Turn `omni-bionic`'s thin [`Fault`] back into the boundary's error.
    #[must_use]
    pub fn fault(&self, fault: Fault) -> AbiError {
        if let Some(error) = self.refusal.borrow_mut().take() {
            return error;
        }
        // Nothing stashed: the refusal came from `omni-bionic`'s own `checked_range`, which
        // rejects a null pointer with a nonzero length and a range whose last byte would leave
        // the 64-bit address space. Reporting it as a `BadPointer` would have to invent a length
        // and an `admit` rule, neither of which was measured.
        self.refusal(format!(
            "the guest address {:#x} is not a usable range: a non-empty access at null, or a \
             length that would wrap the 64-bit address space",
            fault.addr()
        ))
    }

    fn stash(&self, error: AbiError) -> Fault {
        let address = error.guest_address().unwrap_or(0);
        // `BadPointer` carries the *pointer* separately from the thunk address, and the pointer
        // is what `omni-bionic` wants to see.
        let at = match &error {
            AbiError::BadPointer { pointer, .. } => *pointer,
            AbiError::Unterminated { pointer, .. } => *pointer,
            _ => address,
        };
        // First one wins, matching the boundary's own pending-error channel: a second failure
        // inside one call would otherwise overwrite the reason the call actually failed for.
        let mut slot = self.refusal.borrow_mut();
        if slot.is_none() {
            *slot = Some(error);
        }
        Fault(at as u64)
    }

    /// This thread's `errno` cell in guest memory.
    #[must_use]
    pub fn errno_address(&self) -> GuestAddr {
        self.active.block + ERRNO_OFFSET
    }

    /// This thread's `locale_t` cell in guest memory.
    ///
    /// `uselocale` is a *thread* property, and `omni-bionic` deliberately refuses to invent
    /// storage for it — its `uselocale` takes the slot's guest address as an argument and
    /// answers `Unimplemented` for a zero, rather than guessing. This is the adapter deciding
    /// where that storage lives, which is the same arrangement as `scratch`.
    #[must_use]
    pub fn locale_address(&self) -> GuestAddr {
        self.active.block + LOCALE_OFFSET
    }

    /// This thread's scratch buffer in guest memory.
    #[must_use]
    pub fn scratch_address(&self) -> GuestAddr {
        self.active.block + SCRATCH_OFFSET
    }

    /// This thread's `dl_phdr_info` record for boundary nesting level `depth`.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] for a depth past [`DL_INFO_SLOTS`], which
    /// [`MAX_GUEST_DEPTH`](crate::MAX_GUEST_DEPTH) already makes unreachable — checked rather than
    /// asserted because an index computed from a depth is exactly the kind of arithmetic that
    /// stops being true when the cap moves.
    pub fn dl_info_address(&self, depth: usize) -> AbiResult<GuestAddr> {
        if depth >= DL_INFO_SLOTS {
            return Err(self.refusal(format!(
                "boundary nesting level {depth} has no dl_phdr_info record: the arena holds \
                 {DL_INFO_SLOTS} per thread"
            )));
        }
        Ok(self.active.block + DL_INFO_OFFSET + depth * DL_PHDR_INFO_BYTES)
    }

    /// Copy `bytes` plus a NUL into this thread's scratch, returning its guest address.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the string does not fit, rather than a truncation: a
    /// truncated `strerror` message is a believable wrong answer.
    pub fn put_scratch(&self, bytes: &[u8]) -> AbiResult<GuestAddr> {
        if bytes.len() + 1 > SCRATCH_BYTES {
            return Err(self.refusal(format!(
                "a {}-byte result does not fit the {SCRATCH_BYTES}-byte per-thread scratch \
                 buffer, and truncating it would be a plausible wrong answer",
                bytes.len() + 1
            )));
        }
        let at = self.scratch_address();
        let mut buf = Vec::with_capacity(bytes.len() + 1);
        buf.extend_from_slice(bytes);
        buf.push(0);
        // The arena is the adapter's own mapping, so this write is checked against a range this
        // process mapped rather than against anything the guest chose.
        self.mem.write_bytes(at, &buf, self.blame())?;
        Ok(at)
    }
}

impl GuestMemory for GuestView<'_> {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        if buf.is_empty() {
            // A zero-length access touches nothing and is legal C at any address, null
            // included. `GuestMemory`'s own contract says to honour it.
            return Ok(());
        }
        let at = usize::try_from(addr)
            .map_err(|_| self.stash(self.refusal("a guest address wider than the host's usize")))?;
        let ptr = match self.mem.checked_ptr(at, buf.len(), false, self.blame()) {
            Ok(ptr) => ptr,
            Err(error) => return Err(self.stash(error)),
        };
        // SAFETY: `checked_ptr` has taken `[at, at + buf.len())` through `omni_mem::admit`, which
        // established that the whole range lies in one mapped region of this guest space, that
        // the region permits reading, and that it is committed. D4's identity mapping makes the
        // guest address a host address, so this is a read of memory this process owns into a
        // buffer this frame owns. Unaligned, because a guest pointer has no alignment guarantee.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr.cast_const(), buf.as_mut_ptr(), buf.len());
        }
        Ok(())
    }

    fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault> {
        if buf.is_empty() {
            return Ok(());
        }
        let at = usize::try_from(addr)
            .map_err(|_| self.stash(self.refusal("a guest address wider than the host's usize")))?;
        let ptr = match self.mem.checked_ptr(at, buf.len(), true, self.blame()) {
            Ok(ptr) => ptr,
            Err(error) => return Err(self.stash(error)),
        };
        // SAFETY: as `read`, with `admit` having also established that the region's protection
        // permits writing — which is what refuses a guest handing its own `.rodata` over as an
        // output buffer, instead of taking a host access violation on it.
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), ptr, buf.len());
        }
        Ok(())
    }
}

impl GuestAtomic for GuestView<'_> {
    fn cas_u32(&self, addr: u64, expect: u32, new: u32) -> Result<bool, Fault> {
        let at = usize::try_from(addr)
            .map_err(|_| self.stash(self.refusal("a guest address wider than the host's usize")))?;
        // **Alignment is a refusal, not a slow path.** A 32-bit atomic on an unaligned address is
        // undefined behaviour in Rust, and on the guest's own hardware `LDXR`/`STXR` take an
        // alignment fault there too — so refusing is what the guest would see on a real device,
        // and it is the only answer that is not undefined behaviour here.
        if at % 4 != 0 {
            return Err(self.stash(self.refusal(format!(
                "a compare-and-swap on the unaligned guest address {at:#x}: AArch64's exclusive \
                 accesses require 4-byte alignment and fault without it"
            ))));
        }
        let ptr = match self.mem.checked_ptr(at, 4, true, self.blame()) {
            Ok(ptr) => ptr,
            Err(error) => return Err(self.stash(error)),
        };
        // SAFETY: `checked_ptr` established that the four bytes at `at` are mapped, writable and
        // committed in this guest space, and the check above established 4-byte alignment, which
        // is `AtomicU32`'s only additional requirement. Identity mapping (D4) makes the guest
        // address a host address. Forming an `&AtomicU32` rather than a `&mut u32` is the whole
        // point: other guest threads may be racing this word, and an atomic reference is the one
        // Rust reference type that permits that.
        let atomic = unsafe { AtomicU32::from_ptr(ptr.cast::<u32>()) };
        // `SeqCst` on both paths. The guest's own `LDAXR`/`STLXR` pairs are acquire/release, and
        // a weaker ordering here would be a memory-model difference between a mutex the guest
        // took through the thunk and one it took in its own code — invisible until it is not.
        Ok(atomic.compare_exchange(expect, new, Ordering::SeqCst, Ordering::SeqCst).is_ok())
    }
}

impl GuestContext for GuestView<'_> {
    fn errno(&self) -> i32 {
        // The arena is the adapter's own eagerly-committed mapping, so this read cannot fail for
        // a reason the guest caused. If it somehow does, zero is reported: `errno` is only
        // meaningful after a call that set it, and there is no error channel on this trait
        // method to report anything else through.
        self.mem.read_i32(self.errno_address(), self.blame()).unwrap_or(0)
    }

    fn set_errno(&mut self, value: i32) {
        let _ = self.mem.write_u32(self.errno_address(), value as u32, self.blame());
    }

    fn rand_state(&self) -> u32 {
        self.active.bionic.rand_state()
    }

    fn set_rand_state(&mut self, state: u32) {
        self.active.bionic.set_rand_state(state);
    }

    fn scratch(&mut self) -> Option<(u64, usize)> {
        Some((self.scratch_address() as u64, SCRATCH_BYTES))
    }
}
