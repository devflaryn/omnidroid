//! **A guest heap for the APK's second native library, supplied by the embedding.**
//!
//! `libroblox.so` links its own allocator (mimalloc) and imports no libc `malloc` at all, which is
//! why the bionic layer deliberately implements none (D17). The APK's compression library is an
//! ordinary C library: it imports `malloc`, `calloc` and `free` from libc, and its contexts are
//! allocated through them. So the *embedding* supplies them, over one lazily committed anonymous
//! arena, bound into the same boundary as inline handlers.
//!
//! # What this allocator is, and what it refuses to pretend to be
//!
//! Sized classes over a bump cursor. A request's class is the smallest power of two at or above
//! both its size and its alignment, never below [`MIN_CLASS`]; each class has a free list; a block
//! taken from a list that over-fits is split and the remainder re-listed. Nothing is coalesced and
//! nothing is returned to the host: a gate run is one process, and an arena of [`ARENA_BYTES`]
//! reserved lazily costs only the pages the guest touches.
//!
//! **`free(void *)` carries no length**, and this allocator keeps no side table of one -- so a
//! freed block is filed under the [`CATCH_ALL`] class with the only thing that is *known* about it:
//! that it holds at least [`MIN_CLASS`] bytes, because that is the floor every block shares. Every
//! free list is searched by that known length, so a freed block is re-handed-out only to a request
//! that fits inside what is known of it, and never to a larger one. The alternative -- assuming a
//! freed block is as large as the list it sits on -- is the plausible-wrong-answer shape this
//! project refuses one level up: it would work for every test anyone would think to write and
//! corrupt the guest's heap on the first reuse that was bigger than the block.
//!
//! For the same reason [`GuestHeap::realloc`] copies `min(new_size, bytes remaining in the arena
//! after the old pointer)`: C says the bytes past the old size are indeterminate, so copying what
//! the arena holds is exactly as much as the contract allows and no read leaves the arena.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use omni_android::{AbiError, AbiResult, ImportCall, ImportFn};
use omni_cpu::GuestAddr;

/// Bytes of guest address space the arena reserves.
///
/// **The reservation is free; only touched pages commit** ([`omni_mem::CommitPolicy::Lazy`]), so
/// this is a ceiling on what the library may ask for rather than a cost the run pays. 256 MiB is
/// far above anything the compression library's contexts need and low enough that exhaustion is a
/// named refusal rather than a guest space that ran out.
pub const ARENA_BYTES: usize = 256 << 20;

/// The smallest block this allocator hands out, and the smallest alignment it honours.
///
/// 16 bytes is `max_align_t` on AArch64: a pointer returned for any type must be at least this
/// aligned, so no request can be satisfied by less and no block can be smaller.
pub const MIN_CLASS: usize = 16;

/// The class a freed block is filed under, its own length being unknown. See the module comment.
pub const CATCH_ALL: usize = 4096;

/// What is known about a block `free` was handed: nothing but the floor every block shares.
const KNOWN_MIN: usize = MIN_CLASS;

/// `EINVAL`, what `posix_memalign` answers for an alignment that is not a power of two.
pub const EINVAL: i32 = 22;

/// `ENOMEM`, what `posix_memalign` answers when the arena cannot hold the request.
pub const ENOMEM: i32 = 12;

/// One block on a free list: where it starts, and how many bytes are **known** to be there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Block {
    at: GuestAddr,
    known: usize,
}

/// The largest power of two at or below `len`.
///
/// The list a block is filed under. Because every class is a power of two, `bucket(known) >= class`
/// holds exactly when `known >= class` -- so searching every list from `class` upwards finds every
/// block that fits and no list holds a block that a search of it would have to reject for its size.
fn bucket(len: usize) -> usize {
    debug_assert!(len >= MIN_CLASS, "a block smaller than MIN_CLASS is never filed");
    1usize << (usize::BITS - 1 - len.leading_zeros())
}

/// Sized-class bump allocation over one guest region.
#[derive(Debug)]
pub struct GuestHeap {
    base: GuestAddr,
    size: usize,
    /// Bytes of the arena the bump cursor has spent.
    next: usize,
    /// One free list per class, keyed by [`bucket`] -- and [`CATCH_ALL`] for what `free` returns.
    free: BTreeMap<usize, Vec<Block>>,
    /// How many requests a free list answered rather than the bump cursor.
    reused: usize,
    /// How many over-fitting blocks were split and their remainder re-listed.
    splits: usize,
}

impl GuestHeap {
    /// An allocator over `[base, base + size)`, which must be mapped read-write.
    #[must_use]
    pub fn over(base: GuestAddr, size: usize) -> Self {
        Self { base, size, next: 0, free: BTreeMap::new(), reused: 0, splits: 0 }
    }

    /// The class a request of `size` at `align` is served from.
    #[must_use]
    pub fn class_of(size: usize, align: usize) -> usize {
        let align = align.max(MIN_CLASS);
        size.max(align).next_power_of_two().max(MIN_CLASS)
    }

    /// Bytes the bump cursor has spent. What is *live* is not tracked: nothing needs it.
    #[must_use]
    pub fn spent(&self) -> usize {
        self.next
    }

    /// How many requests a free list answered.
    #[must_use]
    pub fn reused(&self) -> usize {
        self.reused
    }

    /// How many over-fitting blocks were split.
    #[must_use]
    pub fn splits(&self) -> usize {
        self.splits
    }

    /// Whether `at` is inside the arena at all.
    #[must_use]
    pub fn holds(&self, at: GuestAddr) -> bool {
        at >= self.base && at < self.base + self.size
    }

    /// Bytes of arena after `at`, or `None` when `at` is not in the arena.
    #[must_use]
    pub fn remaining_after(&self, at: GuestAddr) -> Option<usize> {
        self.holds(at).then(|| self.base + self.size - at)
    }

    /// Hand out `size` bytes aligned to at least `align` (clamped up to [`MIN_CLASS`]).
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] naming `symbol` when `align` is not a power of two, or when neither a
    /// free list nor the bump cursor can serve the request.
    pub fn alloc(&mut self, symbol: &str, size: usize, align: usize) -> AbiResult<GuestAddr> {
        let align = align.max(MIN_CLASS);
        if !align.is_power_of_two() {
            return Err(self.refuse(symbol, format!("an alignment of {align} is not a power of two")));
        }
        let class = Self::class_of(size, align);
        if let Some(at) = self.take(class, align) {
            return Ok(at);
        }
        let at = (self.base + self.next).next_multiple_of(align);
        let end = at.checked_add(class).filter(|end| *end <= self.base + self.size).ok_or_else(|| {
            self.refuse(
                symbol,
                format!(
                    "the {} MiB guest heap this gate gives the library is exhausted: {} of {} \
                     bytes spent, and a {size}-byte request at alignment {align} needs a \
                     {class}-byte block",
                    self.size >> 20,
                    self.next,
                    self.size
                ),
            )
        })?;
        self.next = end - self.base;
        Ok(at)
    }

    /// Give a block back, by address alone. See the module comment for why that is all it takes.
    pub fn free(&mut self, at: GuestAddr) {
        if at == 0 {
            return;
        }
        self.free.entry(CATCH_ALL).or_default().push(Block { at, known: KNOWN_MIN });
    }

    /// The first block, in class order from `class` upwards, that is known to hold `class` bytes
    /// and starts on `align`. Its remainder, if a block can be made of it, is re-listed.
    fn take(&mut self, class: usize, align: usize) -> Option<GuestAddr> {
        let (key, index) = self.free.range(class..).find_map(|(&key, blocks)| {
            let index = blocks.iter().position(|b| b.known >= class && b.at % align == 0)?;
            Some((key, index))
        })?;
        let blocks = self.free.get_mut(&key).expect("just found");
        let block = blocks.remove(index);
        if blocks.is_empty() {
            self.free.remove(&key);
        }
        let rest = block.known - class;
        if rest >= MIN_CLASS {
            self.free
                .entry(bucket(rest))
                .or_default()
                .push(Block { at: block.at + class, known: rest });
            self.splits += 1;
        }
        self.reused += 1;
        Some(block.at)
    }

    /// `realloc`'s move: a new block, the old bytes the arena can account for, and the old block
    /// back on a free list.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when `old` is not in the arena, or when the new block cannot be made.
    pub fn realloc_plan(
        &mut self,
        symbol: &str,
        old: GuestAddr,
        size: usize,
    ) -> AbiResult<(GuestAddr, usize)> {
        let Some(remaining) = self.remaining_after(old) else {
            return Err(self.refuse(
                symbol,
                format!("{old:#x} is not a pointer this heap handed out"),
            ));
        };
        let at = self.alloc(symbol, size, MIN_CLASS)?;
        self.free(old);
        Ok((at, size.min(remaining)))
    }

    fn refuse(&self, symbol: &str, why: String) -> AbiError {
        AbiError::Refused { symbol: symbol.to_string(), address: self.base, why }
    }
}

/// The arena the handlers below allocate out of, for as long as a guest is loaded.
///
/// An `Arc<Mutex<..>>` behind one lock rather than a `OnceLock`, so a second `Guest::load` in the
/// same process installs its own arena instead of silently allocating out of the first one's.
static HEAP: Mutex<Option<Arc<Mutex<GuestHeap>>>> = Mutex::new(None);

/// Publish `heap` as the arena every handler in this module allocates from.
pub fn install(heap: Arc<Mutex<GuestHeap>>) {
    *HEAP.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(heap);
}

/// The installed arena, or a refusal naming the symbol that asked for one.
fn heap(symbol: &str) -> AbiResult<Arc<Mutex<GuestHeap>>> {
    HEAP.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone().ok_or_else(|| {
        AbiError::Refused {
            symbol: symbol.to_string(),
            address: 0,
            why: "no guest heap is installed: this embedding binds the allocator its second \
                  library imports, and nothing installed an arena for it"
                .to_string(),
        }
    })
}

/// Run `f` against the installed arena.
fn with_heap<T>(symbol: &str, f: impl FnOnce(&mut GuestHeap) -> T) -> AbiResult<T> {
    let heap = heap(symbol)?;
    let mut heap = heap.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Ok(f(&mut heap))
}

/// `void *malloc(size_t)`.
fn malloc(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let size = call.args().next_u64()? as usize;
    let at = with_heap("malloc", |heap| heap.alloc("malloc", size, MIN_CLASS))??;
    call.ret().u64(at as u64);
    Ok(())
}

/// `void *calloc(size_t nmemb, size_t size)`.
///
/// **Zeroed explicitly** rather than trusting the arena. A freshly committed page is zero, but a
/// block off a free list has whatever the guest last wrote in it, and `calloc`'s contract does not
/// care which one a request happened to get.
fn calloc(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (count, size) = {
        let mut args = call.args();
        (args.next_u64()? as usize, args.next_u64()? as usize)
    };
    let Some(total) = count.checked_mul(size) else {
        return Err(AbiError::Refused {
            symbol: "calloc".to_string(),
            address: 0,
            why: format!("{count} * {size} bytes overflows the address space"),
        });
    };
    let at = with_heap("calloc", |heap| heap.alloc("calloc", total, MIN_CLASS))??;
    if total > 0 {
        call.mem().write_bytes(at, &vec![0u8; total], call.blame(0))?;
    }
    call.ret().u64(at as u64);
    Ok(())
}

/// `void *realloc(void *, size_t)`.
fn realloc(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (old, size) = {
        let mut args = call.args();
        (args.next_u64()? as GuestAddr, args.next_u64()? as usize)
    };
    if old == 0 {
        let at = with_heap("realloc", |heap| heap.alloc("realloc", size, MIN_CLASS))??;
        call.ret().u64(at as u64);
        return Ok(());
    }
    if size == 0 {
        with_heap("realloc", |heap| heap.free(old))?;
        call.ret().u64(0);
        return Ok(());
    }
    let (at, carried) = with_heap("realloc", |heap| heap.realloc_plan("realloc", old, size))??;
    let bytes = call.mem().read_bytes(old, carried, call.blame(0))?;
    call.mem().write_bytes(at, &bytes, call.blame(0))?;
    call.ret().u64(at as u64);
    Ok(())
}

/// `void free(void *)`. A null pointer is a no-op, as C says.
fn free(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let at = call.args().next_u64()? as GuestAddr;
    with_heap("free", |heap| heap.free(at))?;
    call.ret().void();
    Ok(())
}

/// `int posix_memalign(void **memptr, size_t alignment, size_t size)`.
///
/// Answers `EINVAL`/`ENOMEM` rather than refusing: both are results this function is defined to
/// return, and a guest that checks the return value is entitled to get one.
fn posix_memalign(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (memptr, align, size) = {
        let mut args = call.args();
        (args.next_u64()? as GuestAddr, args.next_u64()? as usize, args.next_u64()? as usize)
    };
    if !align.is_power_of_two() {
        call.ret().i32(EINVAL);
        return Ok(());
    }
    match with_heap("posix_memalign", |heap| heap.alloc("posix_memalign", size, align))? {
        Ok(at) => {
            call.mem().write_u64(memptr, at as u64, call.blame(0))?;
            call.ret().i32(0);
        }
        Err(_) => call.ret().i32(ENOMEM),
    }
    Ok(())
}

/// `void *memalign(size_t alignment, size_t size)`.
fn memalign(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (align, size) = {
        let mut args = call.args();
        (args.next_u64()? as usize, args.next_u64()? as usize)
    };
    let at = with_heap("memalign", |heap| heap.alloc("memalign", size, align))??;
    call.ret().u64(at as u64);
    Ok(())
}

/// `char *strdup(const char *)`.
fn strdup(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let source = call.args().next_u64()? as GuestAddr;
    let text = call.mem().cstr(source, call.blame(0))?;
    let at = with_heap("strdup", |heap| heap.alloc("strdup", text.len() + 1, MIN_CLASS))??;
    call.mem().write_bytes(at, &text, call.blame(0))?;
    call.mem().write_bytes(at + text.len(), b"\0", call.blame(0))?;
    call.ret().u64(at as u64);
    Ok(())
}

/// **Refuse by name, and say why there is no answer.**
///
/// What an import of the second library gets when the layer does not bind it, this heap does not
/// supply it and it is not a name the engine imports as well: a refusal that names the symbol and
/// says the embedding *records* the call rather than answering it. The alternative -- a handler that
/// returned a plausible zero -- is the failure shape this project refuses one level up.
pub fn refuse_by_name(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    Err(AbiError::Refused {
        symbol: call.symbol().to_string(),
        address: call.address(),
        why: "the APK's second native library imports this name; this embedding records the call \
              rather than answering it, because nothing here has decided what it would return"
            .to_string(),
    })
}

/// **Every allocator name this heap can answer**, with the handler that answers it.
///
/// A table rather than seven `bind_inline` calls, because what is bound is decided by the
/// *library's* imports and not by this list: the embedding binds the names in the intersection and
/// reports the count, so a build of the library that imports one more of these gets it and a build
/// that imports fewer is not handed a symbol a device's libc would have had but it never asked for.
pub const ALLOCATOR_HANDLERS: &[(&str, ImportFn)] = &[
    ("malloc", malloc),
    ("calloc", calloc),
    ("realloc", realloc),
    ("free", free),
    ("posix_memalign", posix_memalign),
    ("memalign", memalign),
    ("strdup", strdup),
];

// ============================================================ the heap, without the guest
//
// These run against the allocator alone: it is address arithmetic over a region it never reads or
// writes, so a base address and a size are the whole of what it needs. Nothing here loads a
// library, maps a guest space or starts a backend.

/// A base that is page-aligned and obviously not a real mapping.
const TEST_BASE: GuestAddr = 0x4000_0000;

#[test]
fn a_class_is_the_smallest_power_of_two_that_holds_the_size_and_the_alignment() {
    assert_eq!(GuestHeap::class_of(0, 1), MIN_CLASS, "a zero-byte request still gets a block");
    assert_eq!(GuestHeap::class_of(1, 1), MIN_CLASS, "alignment is clamped up to 16");
    assert_eq!(GuestHeap::class_of(16, 1), 16);
    assert_eq!(GuestHeap::class_of(17, 1), 32);
    assert_eq!(GuestHeap::class_of(24, 64), 64, "the alignment decides when it is the larger");
    assert_eq!(GuestHeap::class_of(4096, 16), 4096);
    assert_eq!(GuestHeap::class_of(4097, 16), 8192);
}

#[test]
fn every_block_is_inside_the_arena_and_aligned_to_what_was_asked_for() {
    let mut heap = GuestHeap::over(TEST_BASE, 64 * 1024);
    let first = heap.alloc("malloc", 8, 1).expect("a first block");
    assert_eq!(first, TEST_BASE, "the bump cursor starts at the base");
    let second = heap.alloc("malloc", 8, 1).expect("a second block");
    assert_eq!(second, TEST_BASE + MIN_CLASS, "an 8-byte request spends a 16-byte class");
    let aligned = heap.alloc("memalign", 8, 256).expect("an aligned block");
    assert_eq!(aligned % 256, 0, "the alignment asked for is the alignment given");
    assert!(heap.holds(aligned) && heap.holds(first));
    assert_eq!(heap.reused(), 0, "nothing was freed, so nothing was reused");
}

#[test]
fn an_over_fitting_block_is_split_and_the_remainder_is_handed_out_next() {
    let mut heap = GuestHeap::over(TEST_BASE, 64 * 1024);
    // A 512-byte block, put back on its own class list by the split path rather than by `free`:
    // `realloc_plan` files the old block under the catch-all, so the length has to be *known* for
    // this to be a test of splitting. A 256-byte request served out of a 512-byte free block is.
    let big = heap.alloc("malloc", 512, 16).expect("a 512-byte block");
    // File it with its true length, which is what a split remainder carries.
    heap.free.entry(bucket(512)).or_default().push(Block { at: big, known: 512 });
    let half = heap.alloc("malloc", 256, 16).expect("half of it");
    assert_eq!(half, big, "the free block was used, not the bump cursor");
    assert_eq!(heap.splits(), 1, "its remainder was re-listed");
    let rest = heap.alloc("malloc", 256, 16).expect("the remainder");
    assert_eq!(rest, big + 256, "the remainder is the second half of the same block");
    assert_eq!(heap.reused(), 2, "both came off a free list");
    assert_eq!(heap.spent(), 512, "the bump cursor was never moved again");
}

#[test]
fn a_freed_block_is_reused_for_what_fits_in_what_is_known_of_it_and_nothing_larger() {
    let mut heap = GuestHeap::over(TEST_BASE, 64 * 1024);
    let block = heap.alloc("malloc", 512, 16).expect("a 512-byte block");
    let spent = heap.spent();
    heap.free(block);
    // `free` knows only that the block holds MIN_CLASS bytes, so a 32-byte request must NOT get
    // it: that is the reuse that would run past the end of a block that had really been 16 bytes.
    let bigger = heap.alloc("malloc", 32, 16).expect("a 32-byte block");
    assert_ne!(bigger, block, "a freed block of unknown length is not handed to a larger request");
    assert_eq!(heap.reused(), 0);
    // A request that fits in what is known of it does get it.
    let fits = heap.alloc("malloc", 16, 16).expect("a 16-byte block");
    assert_eq!(fits, block, "the catch-all class answered a request that fits it");
    assert_eq!(heap.reused(), 1);
    assert!(heap.spent() > spent, "the larger request came off the bump cursor");
}

#[test]
fn exhaustion_is_a_refusal_that_says_how_much_was_asked_for() {
    // Two 16-byte classes and nothing more.
    let mut heap = GuestHeap::over(TEST_BASE, 32);
    heap.alloc("malloc", 16, 16).expect("the first block");
    heap.alloc("malloc", 16, 16).expect("the second block");
    let error = heap.alloc("malloc", 16, 16).expect_err("the arena is full");
    let text = error.to_string();
    assert!(matches!(error, AbiError::Refused { .. }), "exhaustion refuses by name: {text}");
    assert!(text.contains("malloc"), "the refusal names the symbol: {text}");
    assert!(text.contains("exhausted"), "and says what happened: {text}");
    // And a freed block still serves what fits, with the cursor at the end.
    heap.free(TEST_BASE);
    assert_eq!(heap.alloc("malloc", 16, 16).expect("the freed block"), TEST_BASE);
}

#[test]
fn an_alignment_that_is_not_a_power_of_two_is_refused_by_name() {
    let mut heap = GuestHeap::over(TEST_BASE, 64 * 1024);
    let error = heap.alloc("memalign", 16, 24).expect_err("24 is not a power of two");
    assert!(error.to_string().contains("memalign"), "{error}");
}

#[test]
fn realloc_carries_no_more_than_the_arena_holds_after_the_old_pointer() {
    let mut heap = GuestHeap::over(TEST_BASE, 64 * 1024);
    let old = heap.alloc("malloc", 16, 16).expect("a block");
    let (new, carried) = heap.realloc_plan("realloc", old, 64).expect("a larger block");
    assert_ne!(new, old);
    assert_eq!(carried, 64, "the whole new size is inside the arena, so all of it is copied");
    // A pointer at the very end of the arena carries only what is left.
    let end = TEST_BASE + 64 * 1024 - 16;
    let (_, carried) = heap.realloc_plan("realloc", end, 64).expect("a block for the tail");
    assert_eq!(carried, 16, "the copy stops at the end of the arena");
    let error = heap.realloc_plan("realloc", 0x1000, 16).expect_err("not this heap's pointer");
    assert!(error.to_string().contains("realloc"), "{error}");
}
