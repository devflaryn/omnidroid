//! Reading and writing guest memory from a host thunk, with the guest treated as hostile.
//!
//! # Why identity mapping does not make this trivial
//!
//! D4 identity-maps the guest, so a guest address *is* a host address and a `memcpy` implementation
//! could be `core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, n)`. That is exactly
//! the shape Global Constraint 11 is about: `src`, `dst` and `n` all come from guest code, the guest
//! will pass null and pass garbage, and on this host a wild store lands in *Omnidroid's own* address
//! space, where it corrupts the runtime rather than faulting.
//!
//! So every range crosses [`omni_mem::admit`] first — the single copy of "may guest code touch this",
//! the same function the CPU's own slow path uses. Nothing here has its own rules, which is
//! deliberate: two copies is how one of them ends up more permissive than the other.
//!
//! # The cap on a string walk
//!
//! A C string has no length, so a host that walks one is walking guest-controlled data with no bound
//! but the guest's honesty. Two bounds instead: the containing region's end, which `admit` reports
//! and which is a hard fact about the mapping, and [`GuestMem::STRING_LIMIT`], which is a policy
//! choice stated once. A string with no NUL inside either is a typed error naming the symbol, not a
//! walk that eventually reads a page nobody mapped.

use std::sync::Arc;

use omni_mem::{admit, scan_reach, FaultAccess, GuestAddr, GuestSpace, Refusal};

use crate::error::{AbiError, AbiResult};

/// Where an access came from, for the error message.
///
/// Carried as a pair rather than as a formatted string because every error the boundary raises has to
/// name the symbol *and* which argument (Global Constraint 7), and a caller that had to format that
/// itself would eventually format it differently.
#[derive(Debug, Clone, Copy)]
pub struct Blame<'a> {
    /// The symbol being serviced.
    pub symbol: &'a str,
    /// Its thunk address.
    pub address: GuestAddr,
    /// Which argument, counted from zero as the ABI counts them.
    pub argument: usize,
}

impl<'a> Blame<'a> {
    /// Name a symbol and an argument.
    #[must_use]
    pub const fn new(symbol: &'a str, address: GuestAddr, argument: usize) -> Self {
        Self { symbol, address, argument }
    }

    /// The same symbol, a different argument.
    #[must_use]
    pub const fn argument(self, argument: usize) -> Self {
        Self { argument, ..self }
    }
}

/// Checked access to one guest's memory.
///
/// Cheap to clone: it is an `Arc` to the address space and nothing else.
#[derive(Debug, Clone)]
pub struct GuestMem {
    space: Arc<GuestSpace>,
}

impl GuestMem {
    /// How far [`cstr`](GuestMem::cstr) will walk before giving up.
    ///
    /// **A policy number, and stated as one.** 64 KiB is far longer than any string bionic's own
    /// interfaces produce — `PATH_MAX` is 4096, a log line is 4096, a `strerror` message is tens of
    /// bytes — and short enough that a hostile guest cannot make the boundary scan a gigabyte per
    /// call. It is not a correctness bound: the region's end is, and that one comes from the mapping
    /// rather than from a choice.
    pub const STRING_LIMIT: usize = 64 * 1024;

    /// Wrap a guest address space.
    #[must_use]
    pub fn new(space: Arc<GuestSpace>) -> Self {
        Self { space }
    }

    /// The address space itself, for a caller that needs to map something.
    #[must_use]
    pub fn space(&self) -> &Arc<GuestSpace> {
        &self.space
    }

    /// Check `[address, address + len)` for an access of `access`, and return the region's end.
    ///
    /// The one gate. Returns the exclusive end of the containing region, which is what a string walk
    /// needs and what nothing else should have to work out for itself.
    fn check(
        &self,
        address: GuestAddr,
        len: usize,
        access: FaultAccess,
        blame: Blame<'_>,
    ) -> AbiResult<GuestAddr> {
        match admit(&self.space, address, len, access) {
            Ok(admitted) => Ok(admitted.end),
            Err(refusal) => Err(self.bad_pointer(address, len, access, refusal, blame)),
        }
    }

    fn bad_pointer(
        &self,
        address: GuestAddr,
        len: usize,
        access: FaultAccess,
        refusal: Refusal,
        blame: Blame<'_>,
    ) -> AbiError {
        AbiError::BadPointer {
            symbol: blame.symbol.to_string(),
            address: blame.address,
            argument: blame.argument,
            pointer: address,
            len,
            access: match access {
                FaultAccess::Write => "writing",
                FaultAccess::Execute => "execution",
                FaultAccess::Read => "reading",
            },
            refusal: refusal.into(),
        }
    }

    /// Read `len` bytes of guest memory into a `Vec`.
    ///
    /// Copied out rather than borrowed on purpose. A `&[u8]` into the guest would be a Rust shared
    /// reference to memory another guest thread may be writing, which is undefined behaviour however
    /// carefully the borrow is scoped; the guest's own `pthread` threads make that a live possibility
    /// and not a hypothetical. Any handler that cannot afford the copy can take the pointer from
    /// [`checked_ptr`](GuestMem::checked_ptr) and state its own argument.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`], naming the symbol, the argument and the refusal.
    pub fn read_bytes(&self, address: GuestAddr, len: usize, blame: Blame<'_>) -> AbiResult<Vec<u8>> {
        if len == 0 {
            // A zero-length access touches nothing, so there is nothing to check and nothing to
            // copy. `admit` would refuse it or accept it depending on the address, and a boundary
            // that refused `memcpy(dst, src, 0)` — which is legal C and which the guest does emit —
            // would be refusing a correct program.
            return Ok(Vec::new());
        }
        self.check(address, len, FaultAccess::Read, blame)?;
        let mut out = vec![0u8; len];
        // SAFETY: `check` established through `admit` that `[address, address + len)` lies inside one
        // mapped, readable region of this guest space and committed it if the mapping was lazy. D4's
        // identity mapping makes the guest address a host address, so this is a read of memory this
        // process owns, into a buffer this frame owns. Unaligned because a guest pointer has no
        // alignment guarantee at all.
        unsafe {
            core::ptr::copy_nonoverlapping(address as *const u8, out.as_mut_ptr(), len);
        }
        Ok(out)
    }

    /// Write `bytes` into guest memory.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`] with `access: "writing"`, which is the distinction that matters: a
    /// guest passing a pointer into its own read-only `.rodata` as an output buffer is refused here
    /// rather than at a host access violation.
    pub fn write_bytes(&self, address: GuestAddr, bytes: &[u8], blame: Blame<'_>) -> AbiResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.check(address, bytes.len(), FaultAccess::Write, blame)?;
        // SAFETY: as `read_bytes`, with `admit` having also established that the region's protection
        // permits writing.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len());
        }
        Ok(())
    }

    /// A raw host pointer to a checked guest range.
    ///
    /// For the handful of handlers where copying is the wrong answer — a `memcpy` of a megabyte, a
    /// `read` into a guest buffer. The check has happened; what has *not* happened is any claim about
    /// other guest threads, so a caller here owns that argument.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`].
    pub fn checked_ptr(
        &self,
        address: GuestAddr,
        len: usize,
        write: bool,
        blame: Blame<'_>,
    ) -> AbiResult<*mut u8> {
        let access = if write { FaultAccess::Write } else { FaultAccess::Read };
        self.check(address, len, access, blame)?;
        Ok(address as *mut u8)
    }

    /// Read a `u64`.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`].
    pub fn read_u64(&self, address: GuestAddr, blame: Blame<'_>) -> AbiResult<u64> {
        let bytes = self.read_bytes(address, 8, blame)?;
        Ok(u64::from_le_bytes(bytes.try_into().expect("read_bytes returned eight bytes")))
    }

    /// Read a `u32`.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`].
    pub fn read_u32(&self, address: GuestAddr, blame: Blame<'_>) -> AbiResult<u32> {
        let bytes = self.read_bytes(address, 4, blame)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("read_bytes returned four bytes")))
    }

    /// Read an `i32`, which is what a `va_list`'s two offset fields are.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`].
    pub fn read_i32(&self, address: GuestAddr, blame: Blame<'_>) -> AbiResult<i32> {
        Ok(self.read_u32(address, blame)? as i32)
    }

    /// Write a `u64`.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`].
    pub fn write_u64(&self, address: GuestAddr, value: u64, blame: Blame<'_>) -> AbiResult<()> {
        self.write_bytes(address, &value.to_le_bytes(), blame)
    }

    /// Write a `u32`.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`].
    pub fn write_u32(&self, address: GuestAddr, value: u32, blame: Blame<'_>) -> AbiResult<()> {
        self.write_bytes(address, &value.to_le_bytes(), blame)
    }

    /// Read a NUL-terminated string, without the NUL, capped at
    /// [`STRING_LIMIT`](GuestMem::STRING_LIMIT).
    ///
    /// Bytes, not `String`: a guest is under no obligation to hand over UTF-8, and a boundary that
    /// insisted would refuse a `fopen` of a path in whatever encoding the device uses. Callers that
    /// need text use [`String::from_utf8_lossy`] and say so.
    ///
    /// # Errors
    ///
    /// [`AbiError::BadPointer`] if the pointer itself is not readable, or
    /// [`AbiError::Unterminated`] if no NUL appears before the end of the mapping or within the cap.
    pub fn cstr(&self, address: GuestAddr, blame: Blame<'_>) -> AbiResult<Vec<u8>> {
        // The walk needs a *hard* bound before it starts: without one it would step from a mapped
        // page onto an unmapped one and take the fault on the host side, where there is no guest to
        // report it against.
        //
        // **`scan_reach`, not `admit`, because a scan has no length to declare.** `admit` walks as
        // many entries as the length it is given needs, so asking it for one byte reports the end
        // of the *first entry* -- and a lazy commit carves one mapping into a run of entries, one
        // per OS placeholder, that are never joined back up. Bounding the walk by that is bounding
        // it by a granule boundary the guest has never heard of.
        //
        // MEASURED, and it killed a thread: a real Roblox worker died during startup on
        // "`__android_log_print` ... a string at 0x277dca62fa0 ... with no NUL in the first 96
        // bytes". The string was ordinary and NUL-terminated; 96 was the distance to the next
        // entry. `scan_reach` answers the question this actually asks -- how far may I read before
        // I must stop -- and its documentation is where the rule it will not cross is written.
        let reach_end = match scan_reach(&self.space, address, FaultAccess::Read, Self::STRING_LIMIT)
        {
            Ok(end) => end,
            Err(refusal) => {
                return Err(self.bad_pointer(address, 1, FaultAccess::Read, refusal, blame))
            }
        };
        let reach = reach_end.saturating_sub(address).min(Self::STRING_LIMIT);
        // SAFETY: `scan_reach` established that every byte in `[address, address + reach)` lies in
        // a mapped, committed entry of **one** mapping that permits reading -- that is the whole of
        // what it promises and the whole of what is needed here. Identity
        // mapping (D4) makes the guest address a host address. The scan is byte-at-a-time through a
        // raw pointer rather than over a slice, because forming a `&[u8]` across memory another guest
        // thread can write would be undefined behaviour whatever the scan then did with it.
        let found = unsafe {
            let base = address as *const u8;
            (0..reach).find(|&offset| base.add(offset).read() == 0)
        };
        match found {
            Some(len) => self.read_bytes(address, len, blame),
            None => Err(AbiError::Unterminated {
                symbol: blame.symbol.to_string(),
                address: blame.address,
                argument: blame.argument,
                pointer: address,
                limit: reach,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_mem::{CommitPolicy, Placement, Protection};

    struct Fixture {
        mem: GuestMem,
        rw: GuestAddr,
        ro: GuestAddr,
        len: usize,
        unmapped: GuestAddr,
    }

    fn fixture() -> Fixture {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let page = space.page_size();
        let rw = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                page,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("a writable page");
        let ro = space
            .map_anonymous(
                Placement::Anywhere { align: page },
                page,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("a page to drop to read-only");
        space.protect(ro, page, Protection::Read).expect("drop to read-only");
        let unmapped = space
            .regions()
            .into_iter()
            .find(|r| r.is_free() && r.len >= page)
            .map(|r| r.start + r.len / 2)
            .expect("some free address space");
        Fixture { mem: GuestMem::new(space), rw, ro, len: page, unmapped }
    }

    fn blame() -> Blame<'static> {
        Blame::new("strlen", 0xABC0, 0)
    }

    #[test]
    fn a_round_trip_through_guest_memory_keeps_the_bytes() {
        let f = fixture();
        f.mem.write_bytes(f.rw, b"omnidroid", blame()).expect("the write must be admitted");
        assert_eq!(f.mem.read_bytes(f.rw, 9, blame()).expect("read"), b"omnidroid");
        f.mem.write_u64(f.rw + 16, 0x0123_4567_89AB_CDEF, blame()).expect("write");
        assert_eq!(f.mem.read_u64(f.rw + 16, blame()).expect("read"), 0x0123_4567_89AB_CDEF);
        f.mem.write_u32(f.rw + 32, 0xDEAD_BEEF, blame()).expect("write");
        assert_eq!(f.mem.read_u32(f.rw + 32, blame()).expect("read"), 0xDEAD_BEEF);
        assert_eq!(f.mem.read_i32(f.rw + 32, blame()).expect("read"), 0xDEAD_BEEFu32 as i32);
    }

    /// Null is the pointer guest code passes most often by accident, and it must be a typed error
    /// naming the symbol rather than anything else.
    #[test]
    fn a_null_pointer_is_a_typed_error_that_names_the_symbol_and_the_argument() {
        let f = fixture();
        let error = f.mem.read_bytes(0, 8, Blame::new("memcpy", 0x1000, 1)).expect_err("refused");
        assert!(matches!(
            error,
            AbiError::BadPointer { argument: 1, pointer: 0, len: 8, access: "reading", .. }
        ), "{error:?}");
        assert_eq!(error.symbol(), Some("memcpy"));
        assert!(error.to_string().contains("0x0"), "{error}");
    }

    #[test]
    fn a_wild_pointer_and_an_unmapped_hole_are_both_refused() {
        let f = fixture();
        for pointer in [f.unmapped, usize::MAX / 2, usize::MAX] {
            let error = f.mem.read_bytes(pointer, 1, blame()).expect_err("refused");
            assert!(matches!(error, AbiError::BadPointer { .. }), "{pointer:#x}: {error:?}");
        }
    }

    /// The length is the guest's claim, and a read that starts inside a mapping and runs off its end
    /// **into free space** must be refused whole rather than truncated to what fits.
    ///
    /// Into free space, because running into an *adjacent mapping* is admitted, as Linux admits it
    /// (`omni_mem::admit`, 8e0dc34) -- and the fixture's two pages are placed `Anywhere`, so they
    /// may well be adjacent. The edge is found in the region map rather than assumed.
    #[test]
    fn a_length_that_runs_off_the_end_of_the_mapping_is_refused_not_clipped() {
        let f = fixture();
        let edge = f
            .mem
            .space()
            .regions()
            .windows(2)
            .find(|pair| !pair[0].is_free() && pair[1].is_free())
            .map(|pair| pair[0].end())
            .expect("a mapping with free space after it");
        let error = f
            .mem
            .read_bytes(edge - 8, 16, blame())
            .expect_err("a read straddling the end of the mapping must be refused");
        assert!(matches!(error, AbiError::BadPointer { len: 16, .. }), "{error:?}");
        // And the eight that do fit still work, so the refusal is about the range and not the page.
        assert_eq!(f.mem.read_bytes(edge - 8, 8, blame()).expect("read").len(), 8);
    }

    /// A guest that hands its own `.rodata` over as an output buffer gets a typed refusal, and the
    /// access kind in the message is what tells a reader which of the two mistakes it was.
    #[test]
    fn a_write_to_a_read_only_mapping_is_refused_with_the_access_named() {
        let f = fixture();
        assert_eq!(f.mem.read_bytes(f.ro, 4, blame()).expect("reading is fine").len(), 4);
        let error = f.mem.write_bytes(f.ro, b"no", blame()).expect_err("refused");
        match error {
            AbiError::BadPointer { access, refusal, .. } => {
                assert_eq!(access, "writing");
                assert_eq!(refusal, Refusal::Protection.into());
            }
            other => panic!("{other:?}"),
        }
    }

    /// `memcpy(dst, src, 0)` is legal C and the engine emits it. A boundary that refused it would be
    /// refusing a correct program.
    #[test]
    fn a_zero_length_access_touches_nothing_and_is_not_an_error() {
        let f = fixture();
        assert!(f.mem.read_bytes(0, 0, blame()).expect("no bytes, no access").is_empty());
        f.mem.write_bytes(0, &[], blame()).expect("no bytes, no access");
    }

    #[test]
    fn a_string_is_read_up_to_its_nul_and_the_nul_is_not_included() {
        let f = fixture();
        f.mem.write_bytes(f.rw, b"libroblox.so\0trailing", blame()).expect("write");
        assert_eq!(f.mem.cstr(f.rw, blame()).expect("cstr"), b"libroblox.so");
        // An empty string is a NUL at offset zero, not an error.
        f.mem.write_bytes(f.rw, b"\0", blame()).expect("write");
        assert!(f.mem.cstr(f.rw, blame()).expect("cstr").is_empty());
    }

    /// **A string that straddles a commit granule is read, not refused.**
    ///
    /// The regression this is the detector for, and the run that produced it: a real Roblox worker
    /// thread died during §8 row 21 with
    ///
    /// ```text
    /// `__android_log_print` was passed a string at 0x277dca62fa0 for argument 3
    ///  with no NUL in the first 96 bytes
    /// ```
    ///
    /// 96 is not a property of the string -- it is the distance to the next entry boundary in a
    /// lazily-committed mapping, which the guest has never heard of. The log line was ordinary and
    /// NUL-terminated. The thread was killed by this layer, and the future it was holding was never
    /// completed.
    ///
    /// Every other fixture here is `CommitPolicy::Eager`, which is one entry that is never split,
    /// which is why nothing saw this.
    #[test]
    fn a_string_that_crosses_a_commit_granule_is_read_rather_than_refused() {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let granule = space.commit_granule();
        let base = space
            .map_anonymous(
                Placement::Anywhere { align: granule },
                4 * granule,
                Protection::ReadWrite,
                CommitPolicy::Lazy,
            )
            .expect("a lazily-committed mapping");
        let boundary = base + granule;
        space.ensure_committed(base, 1).expect("commit the first granule");
        space.ensure_committed(boundary, 1).expect("commit the second granule");
        // The precondition: the granules really are separate entries, which is what used to bound
        // the walk.
        assert_eq!(
            space.region_at(boundary - 8).expect("mapped").end(),
            boundary,
            "adjacent committed granules are expected to stay separate entries",
        );

        let mem = GuestMem::new(Arc::clone(&space));
        // A log line of the length the engine actually emits, placed so that it begins 96 bytes
        // before the boundary and ends well past it.
        let line: Vec<u8> = b"[FLog::NativeDM] nativeActivity_onSurfaceChanged: state:2, \
                              window 0x0000007f00000000, density 0.000, flags-received"
            .to_vec();
        assert!(line.len() > 96, "the line has to cross the boundary to be the case under test");
        let start = boundary - 96;
        let mut stored = line.clone();
        stored.push(0);
        mem.write_bytes(start, &stored, blame()).expect("write the line across the boundary");

        assert_eq!(mem.cstr(start, blame()).expect("the string is readable"), line);
    }

    /// The hostile string: mapped memory that never terminates. The walk must stop at the end of the
    /// region — a hard fact — and say so, rather than stepping onto the next page.
    #[test]
    fn an_unterminated_string_stops_at_the_end_of_its_region_and_says_so() {
        let f = fixture();
        f.mem.write_bytes(f.rw, &vec![b'A'; f.len], blame()).expect("fill the whole page");
        let error = f.mem.cstr(f.rw, Blame::new("fopen", 0x2000, 0)).expect_err("refused");
        match error {
            AbiError::Unterminated { symbol, pointer, limit, .. } => {
                assert_eq!(symbol, "fopen");
                assert_eq!(pointer, f.rw);
                assert_eq!(limit, f.len, "the walk is bounded by the region, not by the cap");
            }
            other => panic!("{other:?}"),
        }
        // Starting one byte from the end reaches exactly one byte, and still refuses rather than
        // reading the byte after the mapping.
        let error = f.mem.cstr(f.rw + f.len - 1, blame()).expect_err("refused");
        assert!(matches!(error, AbiError::Unterminated { limit: 1, .. }), "{error:?}");
    }

    #[test]
    fn a_string_at_an_unmapped_address_is_a_bad_pointer_and_not_an_unterminated_string() {
        let f = fixture();
        let error = f.mem.cstr(f.unmapped, blame()).expect_err("refused");
        assert!(matches!(error, AbiError::BadPointer { len: 1, .. }), "{error:?}");
    }

    #[test]
    fn a_checked_pointer_is_refused_for_the_access_it_is_asked_for() {
        let f = fixture();
        assert_eq!(
            f.mem.checked_ptr(f.rw, 16, true, blame()).expect("writable") as usize,
            f.rw,
            "identity mapping means the guest address is the host pointer (D4)"
        );
        f.mem.checked_ptr(f.ro, 16, false, blame()).expect("readable");
        f.mem.checked_ptr(f.ro, 16, true, blame()).expect_err("not writable");
    }

    /// The cap is a policy number and the region end is a fact, so the cap must be the smaller of
    /// the two when the region is large. Asserted on the constant rather than on a 64 KiB mapping,
    /// which would make the test about commit charge.
    #[test]
    fn the_string_cap_is_stated_once_and_is_larger_than_any_bionic_string() {
        assert_eq!(GuestMem::STRING_LIMIT, 65_536);
        // PATH_MAX and an Android log line are both 4096, so the cap is an order of magnitude
        // above anything bionic itself produces.
        assert_eq!(GuestMem::STRING_LIMIT / 4096, 16);
    }
}
