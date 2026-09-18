//! The JIT code arena: dual-mapped, so no page is ever writable and executable at once.

use std::sync::atomic::{AtomicU64, Ordering};

use omni_platform::vm::{self, Protection, SharedSection};
use parking_lot::Mutex;

use crate::error::{platform, MemError, MemResult};

/// Default arena chunk: 1 MiB.
///
/// Chunks exist because a pagefile-backed section is committed when it is *created*, not when it is
/// touched — the same asymmetry ordinary commit has (D10) — so one large section would charge the
/// system commit limit for code that has not been emitted yet. The arena therefore grows a chunk at
/// a time, and 1 MiB bounds the waste at the tail while keeping the number of sections, views and
/// 64 KiB-aligned view placements small.
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

/// Default ceiling on total arena size: 256 MiB.
///
/// A ceiling rather than a reservation: nothing is charged until a chunk is created. It exists so
/// that a translator bug which emits without bound fails with [`MemError::ArenaFull`] naming the
/// limit, instead of consuming the machine's commit limit and taking every other instance down with
/// it.
pub const DEFAULT_MAX_TOTAL: usize = 256 * 1024 * 1024;

/// Default alignment for emitted code blocks: 16 bytes.
pub const DEFAULT_BLOCK_ALIGNMENT: usize = 16;

/// How a [`CodeArena`] grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaConfig {
    /// Size of each chunk the arena adds when it runs out of room. Rounded up to the allocation
    /// granularity.
    pub chunk_size: usize,
    /// Ceiling on the total size of all chunks.
    pub max_total: usize,
    /// Alignment every block starts at. A power of two.
    pub block_alignment: usize,
}

impl Default for ArenaConfig {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_total: DEFAULT_MAX_TOTAL,
            block_alignment: DEFAULT_BLOCK_ALIGNMENT,
        }
    }
}

/// What a [`CodeArena`] currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArenaStats {
    /// Chunks created. Each one is a section and two views.
    pub chunks: usize,
    /// Total bytes of section across all chunks. This is what has been charged against the system
    /// commit limit.
    pub mapped: usize,
    /// Bytes handed out as blocks.
    pub used: usize,
    /// Bytes lost to block alignment and to the tails of chunks that could not fit the next block.
    pub wasted: usize,
    /// Bytes of writable view currently sealed, rounded outwards to whole pages.
    ///
    /// Reported because sealing is page-granular while the API that requests it is block-granular,
    /// so the amount actually sealed is not something a caller can work out from the blocks it
    /// sealed. A translator that seals every 256-byte block on a 4 KiB page seals the page sixteen
    /// times over and this says so.
    pub sealed: usize,
}

/// Identity of a [`CodeArena`], carried by every [`CodeBlock`] it mints.
///
/// It exists because a block is a plain `Copy` value holding addresses rather than a borrow, so the
/// compiler cannot tell one arena's block from another's — and per-thread code caches mean several
/// live arenas is the expected shape (D5 measured 20–35 MiB of code cache per guest thread, not
/// shared between threads). Every arena operation that dereferences a block's address checks this
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArenaId(pub u64);

static NEXT_ARENA: AtomicU64 = AtomicU64::new(1);

/// A block of code memory, addressable for writing and for execution at two different addresses.
///
/// The two addresses are views of the same physical pages: a store through
/// [`write_ptr`](CodeBlock::write_ptr) is visible through [`exec_ptr`](CodeBlock::exec_ptr)
/// immediately, with no protection change and no instruction-cache flush on x86-64 — measured at
/// 162 ns per emit-and-execute cycle against 2259 ns for flipping a single mapping with
/// `VirtualProtect`, with 0 mismatches across 200,000 trials (D12).
///
/// `Send + Sync`, and a plain value: it holds addresses, not borrows. The arena owns the memory and
/// keeps it alive for its own lifetime, and nothing frees an individual block.
///
/// # Why it carries an arena identity
///
/// Because it holds addresses rather than borrows, nothing in the type system ties a block to the
/// arena that made it. So every operation that dereferences one — [`CodeArena::write`],
/// [`CodeArena::seal`], [`CodeArena::unseal`] — checks [`arena`](CodeBlock::arena) against the
/// arena it was called on and refuses a foreign block with [`MemError::ForeignBlock`]. Without that
/// check `let b = a.alloc(16)?; drop(a); other.write(&b, 0, &bytes)` is expressible in safe Rust and
/// writes through an unmapped address. The fields are private and there is no public constructor, so
/// a block can only come from an [`alloc`](CodeArena::alloc) and its identity cannot be forged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeBlock {
    arena: ArenaId,
    chunk: usize,
    write: usize,
    exec: usize,
    len: usize,
}

impl CodeBlock {
    /// Which arena minted this block.
    #[must_use]
    pub fn arena(&self) -> ArenaId {
        self.arena
    }

    /// Where to write the code. Writable, never executable.
    ///
    /// # Safety
    ///
    /// The returned pointer is valid for `len()` bytes for as long as the arena lives. Writing
    /// through it while another thread executes the same block through
    /// [`exec_ptr`](CodeBlock::exec_ptr) is a data race on instruction bytes, and is the caller's
    /// problem: the arena hands out memory, it does not sequence the translator.
    #[must_use]
    pub fn write_ptr(&self) -> *mut u8 {
        self.write as *mut u8
    }

    /// Where to call the code. Executable, never writable.
    ///
    /// Deliberately `*const`: there is no supported way to obtain a `*mut` to the executable view,
    /// because there is no operation in `omni-platform` that could make it writable — the view is
    /// created `PAGE_EXECUTE_READ` and [`Protection`] has no writable-and-executable variant.
    #[must_use]
    pub fn exec_ptr(&self) -> *const u8 {
        self.exec as *const u8
    }

    /// Length of the block in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the block is empty. Always false: a zero-length allocation is rejected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The distance from the writable address to the executable one.
    ///
    /// Non-zero by construction, and exposed so that a caller can assert the W^X property rather
    /// than take it on trust.
    #[must_use]
    pub fn view_distance(&self) -> isize {
        self.exec as isize - self.write as isize
    }
}

struct Chunk {
    /// Kept alive for as long as its views are mapped. Dropping it closes the section handle, which
    /// does not unmap the views — but it would make the chunk unable to be mapped again.
    _section: SharedSection,
    write: usize,
    exec: usize,
    len: usize,
    used: usize,
    /// One bit per page of the writable view: set when the page is sealed.
    ///
    /// A bitmap rather than a set of ranges because sealing is page-granular at the OS and mirroring
    /// it exactly is the only representation that cannot drift from it: sealing two blocks that
    /// share a page sets the same bit twice, and unsealing either clears it, which is precisely what
    /// `VirtualProtect` does to the page. A 1 MiB chunk of 4 KiB pages needs 256 bits — four `u64`s.
    sealed: Vec<u64>,
}

impl Chunk {
    /// Page indices `[address, address + len)` covers, clamped to this chunk.
    fn pages(&self, page: usize, address: usize, len: usize) -> core::ops::Range<usize> {
        let start = address.max(self.write);
        let end = address.saturating_add(len).min(self.write + self.len);
        if end <= start {
            return 0..0;
        }
        ((start - self.write) / page)..((end - 1 - self.write) / page + 1)
    }

    /// Mark pages sealed or unsealed. Returns how many pages actually changed state, which is what
    /// keeps the arena-level counter — and therefore [`CodeArena::write`]'s lock-free fast path —
    /// exact rather than approximate.
    fn set_sealed(&mut self, pages: core::ops::Range<usize>, sealed: bool) -> isize {
        let mut changed = 0isize;
        for index in pages {
            let (word, bit) = (index / 64, 1u64 << (index % 64));
            let was = self.sealed[word] & bit != 0;
            if was == sealed {
                continue;
            }
            self.sealed[word] ^= bit;
            changed += if sealed { 1 } else { -1 };
        }
        changed
    }

    /// Whether any page in the range is sealed. Returns the first one that is.
    fn first_sealed(&self, pages: core::ops::Range<usize>) -> Option<usize> {
        pages.into_iter().find(|index| self.sealed[index / 64] & (1u64 << (index % 64)) != 0)
    }
}

/// A dual-mapped arena for emitted code.
///
/// # Why two mappings instead of flipping one
///
/// The translator writes code constantly, so this is a hot path, and there are only two ways to
/// write to memory that will be executed: flip one mapping between writable and executable, or map
/// the same pages twice with different protections. Measured, the second is **fourteen times
/// faster** and never holds a page that is both writable and executable (D12). It is the rare case
/// where the safer option is also the faster one.
///
/// # The W^X guarantee
///
/// It is not possible to obtain a pointer from this API that is both writable and executable:
///
/// * The writable view is created [`Protection::ReadWrite`] and the executable view
///   [`Protection::ReadExecute`]. They are separate address ranges, asserted non-overlapping when a
///   chunk is created.
/// * [`CodeBlock::exec_ptr`] returns `*const u8`, and no operation exists that could raise a view
///   to writable-and-executable: `omni-platform`'s [`Protection`] has no such variant, and the
///   Windows backend asserts that no [`Protection`] can resolve to `PAGE_EXECUTE_READWRITE` or
///   `PAGE_EXECUTE_WRITECOPY` whatever kind of region it is applied to.
/// * The pagefile-backed *section* does allow both, because a section's protection caps its views'
///   and the arena needs one of each. A section is a capability, not a mapping; no page-table entry
///   in the process ever grants write and execute together.
///
/// # Thread safety
///
/// `Send + Sync`. [`alloc`](CodeArena::alloc) takes a short lock; writing into a block afterwards
/// takes none, because a block is a private range of memory that no other allocation can overlap —
/// unless something in this arena has been [`seal`](CodeArena::seal)ed, in which case
/// [`write`](CodeArena::write) takes the lock long enough to refuse a store to a read-only page.
/// An arena that only ever emits never pays for that.
pub struct CodeArena {
    id: ArenaId,
    config: ArenaConfig,
    page: usize,
    granularity: usize,
    /// How many pages across all chunks are sealed.
    ///
    /// It exists so that [`write`](CodeArena::write) can stay lock-free while nothing is sealed,
    /// which is the state a translator emitting a fresh block is always in. Only when something has
    /// been sealed does `write` take the lock to find out whether this write is the one that would
    /// have faulted.
    sealed_pages: AtomicU64,
    inner: Mutex<Vec<Chunk>>,
}

impl CodeArena {
    /// Create an arena with the default configuration. Nothing is mapped until the first
    /// [`alloc`](CodeArena::alloc).
    ///
    /// # Errors
    ///
    /// [`MemError::InvalidConfig`] — see [`CodeArena::with_config`].
    pub fn new() -> MemResult<Self> {
        Self::with_config(ArenaConfig::default())
    }

    /// Create an arena.
    ///
    /// # Errors
    ///
    /// [`MemError::InvalidConfig`] for a zero chunk size, a `max_total` smaller than one chunk, or
    /// a block alignment that is not a power of two.
    pub fn with_config(config: ArenaConfig) -> MemResult<Self> {
        if config.chunk_size == 0 {
            return Err(MemError::InvalidConfig {
                field: "chunk_size",
                value: 0,
                reason: "must be greater than zero",
            });
        }
        if !config.block_alignment.is_power_of_two() {
            return Err(MemError::InvalidConfig {
                field: "block_alignment",
                value: config.block_alignment as u64,
                reason: "must be a power of two",
            });
        }
        if config.max_total < config.chunk_size {
            return Err(MemError::InvalidConfig {
                field: "max_total",
                value: config.max_total as u64,
                reason: "must be at least one chunk",
            });
        }
        // An alignment larger than a chunk can never be satisfied inside one, so every allocation
        // would get a chunk of its own — `1 << 62` was accepted and then produced exactly that, which
        // looks like it works and is a 4 EiB-per-block arena. It also overflows the round-up in
        // `carve`. Refused with the value, rather than silently degrading.
        if config.block_alignment > config.chunk_size {
            return Err(MemError::InvalidConfig {
                field: "block_alignment",
                value: config.block_alignment as u64,
                reason: "must not exceed chunk_size; no address inside a chunk could satisfy it, so \
                         every block would take a chunk of its own",
            });
        }
        Ok(Self {
            id: ArenaId(NEXT_ARENA.fetch_add(1, Ordering::Relaxed)),
            config,
            page: vm::page_size(),
            granularity: vm::allocation_granularity(),
            sealed_pages: AtomicU64::new(0),
            inner: Mutex::new(Vec::new()),
        })
    }

    /// This arena's identity, as carried by every [`CodeBlock`] it mints.
    #[must_use]
    pub fn id(&self) -> ArenaId {
        self.id
    }

    /// The configuration in force.
    #[must_use]
    pub fn config(&self) -> ArenaConfig {
        self.config
    }

    /// Reserve `size` bytes of code memory, addressable for writing and for execution.
    ///
    /// # Errors
    ///
    /// [`MemError::ZeroSize`], [`MemError::ArenaFull`] if the arena would grow past
    /// [`ArenaConfig::max_total`], or [`MemError::Platform`] if a section or a view cannot be
    /// created.
    pub fn alloc(&self, size: usize) -> MemResult<CodeBlock> {
        if size == 0 {
            return Err(MemError::ZeroSize { operation: "CodeArena::alloc" });
        }
        let align = self.config.block_alignment;
        let mut chunks = self.inner.lock();

        // Fit into an existing chunk if one has room. Most recent first: the translator emits in
        // bursts, so the newest chunk is almost always the one with space.
        for index in (0..chunks.len()).rev() {
            if let Some(block) = self.carve(&mut chunks[index], index, size, align) {
                return Ok(block);
            }
        }

        let mapped: usize = chunks.iter().map(|chunk| chunk.len).sum();
        let full = || MemError::ArenaFull {
            requested: size,
            in_use: mapped,
            limit: self.config.max_total,
        };
        // Every step is checked, because each of them can overflow on a hostile or simply wrong
        // `size`, and in a release build an overflow would *wrap past* the limit check below and go
        // on to map something — the one outcome worse than refusing the request.
        let want = size.max(self.config.chunk_size);
        let chunk_len = want
            .checked_add(self.granularity - 1)
            .map(|len| len & !(self.granularity - 1))
            .ok_or_else(full)?;
        if mapped.checked_add(chunk_len).is_none_or(|total| total > self.config.max_total) {
            return Err(full());
        }

        let mut chunk = self.new_chunk(chunk_len)?;
        let index = chunks.len();
        let block = self
            .carve(&mut chunk, index, size, align)
            .expect("a fresh chunk is at least as large as the block that asked for it");
        chunks.push(chunk);
        tracing::debug!(
            chunk = index,
            len = chunk_len,
            // The cumulative figure, because this is the arena's contribution to the system commit
            // limit and it is invisible to `process_commit_charge` — see `CommitBudget`.
            arena_mapped = mapped + chunk_len,
            write = format_args!("{:#x}", block.write),
            exec = format_args!("{:#x}", block.exec),
            "grew the code arena"
        );
        Ok(block)
    }

    /// Copy `bytes` into a block at `offset`.
    ///
    /// Writes go through the writable view, so they are visible through
    /// [`CodeBlock::exec_ptr`](CodeBlock::exec_ptr) as soon as the store retires. Safe, because the
    /// block belongs to this arena — checked — and because the bounds are checked against the block,
    /// which is memory the arena owns and never hands out twice.
    ///
    /// Takes no lock while nothing in the arena is sealed: the identity check is a field comparison,
    /// and a block of *this* arena always names memory this arena has mapped for as long as it
    /// lives. Once anything has been sealed it takes the lock for long enough to refuse a write to a
    /// sealed page — see [`MemError::BlockSealed`] for why that check has to exist.
    ///
    /// # Errors
    ///
    /// [`MemError::ForeignBlock`] if the block was minted by a different arena,
    /// [`MemError::BlockOverflow`] if the write would run past the end of the block, or
    /// [`MemError::BlockSealed`] if any page it would touch is sealed.
    ///
    /// # What the sealed check promises, exactly
    ///
    /// Once anything in this arena is sealed, the check **and the store** happen under the arena
    /// lock, so a concurrent [`seal`](CodeArena::seal) — which takes the same lock — cannot change a
    /// page's protection between them. That closes the hazard completely for an arena that has ever
    /// sealed, and it costs the lock-free path nothing.
    ///
    /// One window remains, and it is worth naming rather than waving at: the **first** seal in an
    /// arena, racing a write that has already read `sealed_pages` as zero and taken the fast path.
    /// Closing that would mean taking the lock on every write, including in the arena of a
    /// translator that never seals, which is the one path D12 measured as hot.
    ///
    /// It is deliberately *not* the same class as two threads emitting into one block. That is a
    /// race between a caller and itself over memory it asked for. This one couples **two unrelated
    /// blocks** through a page neither of them named — with the default 16-byte alignment, up to 256
    /// blocks share a page, and sealing any of them seals the rest. That is a genuinely surprising
    /// coupling, which is why it is stated here in full rather than folded into a general warning.
    ///
    /// For M2's expected shape it does not arise at all: D5 gives each guest thread its own code
    /// cache, so an arena has one writer.
    ///
    /// The cost of the closure: while anything is sealed, writes to this arena serialise against
    /// each other and against `alloc`. One arena per guest thread means no contention; an arena
    /// shared between threads would feel it, and would be choosing safety over throughput, which is
    /// the right way round for a page that may be read-only.
    pub fn write(&self, block: &CodeBlock, offset: usize, bytes: &[u8]) -> MemResult<()> {
        self.check_own(block)?;
        if offset > block.len || bytes.len() > block.len - offset {
            return Err(MemError::BlockOverflow {
                offset,
                len: bytes.len(),
                block_len: block.len,
            });
        }
        if bytes.is_empty() {
            return Ok(());
        }
        // The fast path is the common one: a translator emitting a block has sealed nothing, and
        // this is one acquire load of a counter that is almost always zero. The slow path is the one
        // that matters, because the pages really are read-only and this is a *safe* function —
        // without the check, the `copy_nonoverlapping` below is an access violation reachable from
        // entirely safe code.
        //
        // The guard is *held across the store*, not dropped after the check. `seal` takes the same
        // lock, so while it is held no page's protection can change underneath the copy. Keeping it
        // is what turns the check from "usually right" into a guarantee, and it costs the fast path
        // nothing because the fast path never takes it.
        let _sealed_guard = if self.sealed_pages.load(Ordering::Acquire) != 0 {
            let chunks = self.inner.lock();
            let chunk = chunks.get(block.chunk).ok_or(MemError::BlockOutsideChunk {
                write: block.write,
                len: block.len,
                chunk: block.chunk,
                chunks: chunks.len(),
            })?;
            let pages = chunk.pages(self.page, block.write + offset, bytes.len());
            if let Some(index) = chunk.first_sealed(pages) {
                return Err(MemError::BlockSealed {
                    write: block.write,
                    offset,
                    len: bytes.len(),
                    page: chunk.write + index * self.page,
                });
            }
            Some(chunks)
        } else {
            None
        };
        // SAFETY: `[block.write + offset, + bytes.len())` is inside the block, which is inside the
        // chunk's writable view — checked above — and the arena never hands the same range out
        // twice, so this is an exclusive write to memory the arena owns. The source and destination
        // cannot overlap: `bytes` is the caller's memory and this range is the arena's. If anything
        // in this arena is sealed, `_sealed_guard` still holds the arena lock, so the protection of
        // these pages cannot have changed since it was checked.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (block.write + offset) as *mut u8,
                bytes.len(),
            );
        }
        drop(_sealed_guard);
        Ok(())
    }

    /// Make a block's pages read-only through the writable view, so nothing can modify live code.
    ///
    /// The executable view is unaffected: the code stays callable. Undo it with
    /// [`unseal`](CodeArena::unseal) to patch the block.
    ///
    /// Page-granular, and it rounds **outwards**: blocks are 16-byte aligned by default, so several
    /// share a page, and sealing one seals its page-mates. Allocate a block of at least a page if it
    /// must be sealed independently. A [`write`](CodeArena::write) to any of those page-mates is
    /// then refused with [`MemError::BlockSealed`], which names the page so the cause is visible —
    /// rather than faulting, which is what a safe `write` to a read-only page would otherwise do.
    ///
    /// # Errors
    ///
    /// [`MemError::ForeignBlock`] if the block was minted by a different arena, or
    /// [`MemError::Platform`] if the protection change fails.
    pub fn seal(&self, block: &CodeBlock) -> MemResult<()> {
        self.reprotect(block, Protection::Read)
    }

    /// Make a sealed block writable again through the writable view.
    ///
    /// This is the operation that depends on a shared-writable view staying *shared*. Resolving
    /// [`Protection::ReadWrite`] to `PAGE_WRITECOPY` here — which is the correct answer for a
    /// private file view and the wrong one for this — would privatise the pages: every later write
    /// would land on a copy, the executable view would keep running the old code, and nothing would
    /// report an error. `omni-platform` classifies the region by the protection its view was created
    /// with, so the shared case stays shared.
    ///
    /// # Errors
    ///
    /// [`MemError::ForeignBlock`] if the block was minted by a different arena, or
    /// [`MemError::Platform`] if the protection change fails.
    pub fn unseal(&self, block: &CodeBlock) -> MemResult<()> {
        self.reprotect(block, Protection::ReadWrite)
    }

    /// What the arena currently holds.
    #[must_use]
    pub fn stats(&self) -> ArenaStats {
        let chunks = self.inner.lock();
        let mut stats = ArenaStats { chunks: chunks.len(), ..ArenaStats::default() };
        for chunk in chunks.iter() {
            stats.mapped += chunk.len;
            stats.used += chunk.used;
        }
        stats.wasted = stats.mapped - stats.used;
        stats.sealed = self.sealed_pages.load(Ordering::Acquire) as usize * self.page;
        stats
    }

    // -------------------------------------------------------------------------------------------

    fn carve(
        &self,
        chunk: &mut Chunk,
        index: usize,
        size: usize,
        align: usize,
    ) -> Option<CodeBlock> {
        // Checked: `block_alignment` is capped at `chunk_size` by `with_config`, so this cannot
        // overflow today, but the round-up is the exact shape that wrapped past the limit check in
        // `alloc` before it was made checked.
        let start = chunk.used.checked_add(align - 1)? & !(align - 1);
        if start >= chunk.len || chunk.len - start < size {
            return None;
        }
        chunk.used = start + size;
        Some(CodeBlock {
            arena: self.id,
            chunk: index,
            write: chunk.write + start,
            exec: chunk.exec + start,
            len: size,
        })
    }

    /// Refuse a block another arena minted.
    ///
    /// The whole soundness of the safe [`write`](CodeArena::write) rests on this: a block's addresses
    /// are only live for as long as *its* arena is, and a block is otherwise indistinguishable from
    /// one of ours.
    fn check_own(&self, block: &CodeBlock) -> MemResult<()> {
        if block.arena != self.id {
            return Err(MemError::ForeignBlock {
                arena: self.id.0,
                block_arena: block.arena.0,
            });
        }
        Ok(())
    }

    fn new_chunk(&self, len: usize) -> MemResult<Chunk> {
        let section = vm::create_shared_section(len as u64)
            .map_err(platform("CodeArena::alloc", 0, len))?;

        // SAFETY: the section is live and `len` is its whole length, so neither view runs past its
        // end. Each returned view is a fresh mapping at an address the OS chose, which nothing else
        // in the process holds; both are released in `Drop`.
        let write = unsafe { vm::map_section(&section, 0, len, Protection::ReadWrite) }
            .map_err(platform("CodeArena::alloc", 0, len))?;
        // SAFETY: as above — the same live section, mapped a second time at another address the OS
        // chooses. This is the whole point: two views of the same pages, one writable and one
        // executable, so that neither is both.
        let exec = match unsafe { vm::map_section(&section, 0, len, Protection::ReadExecute) } {
            Ok(exec) => exec,
            Err(error) => {
                // SAFETY: `write` is the view mapped immediately above and nothing has been handed
                // out from it, so nothing holds a reference into it.
                let _ = unsafe { vm::unmap_and_release(write, len) };
                return Err(platform("CodeArena::alloc", write as usize, len)(error));
            }
        };

        let (write, exec) = (write as usize, exec as usize);
        // The W^X guarantee in one line: the writable and executable views must not overlap, or a
        // single address would carry both rights. The OS chooses the addresses, so this is asserted
        // rather than assumed.
        assert!(
            write + len <= exec || exec + len <= write,
            "the arena's writable view {write:#x}+{len:#x} overlaps its executable view {exec:#x}"
        );

        let pages = len.div_ceil(self.page);
        Ok(Chunk {
            _section: section,
            write,
            exec,
            len,
            used: 0,
            sealed: vec![0; pages.div_ceil(64)],
        })
    }

    fn reprotect(&self, block: &CodeBlock, protection: Protection) -> MemResult<()> {
        self.check_own(block)?;
        let mut chunks = self.inner.lock();
        // Was `chunks[block.chunk]`, which panicked for an out-of-range index, and then computed
        // `end - start` guarded only by a `debug_assert!` — so in a release build a block whose
        // `write` sat below `chunk.write` underflowed that subtraction into a huge page-aligned
        // length and handed it to `vm::protect`. Both are now refusals. Unreachable for a block this
        // arena minted, which is what `check_own` above establishes; this is the second line.
        let count = chunks.len();
        let chunk = chunks.get_mut(block.chunk).ok_or(MemError::BlockOutsideChunk {
            write: block.write,
            len: block.len,
            chunk: block.chunk,
            chunks: count,
        })?;
        if !block_fits_chunk(block.write, block.len, chunk.write, chunk.len) {
            return Err(MemError::BlockOutsideChunk {
                write: block.write,
                len: block.len,
                chunk: block.chunk,
                chunks: count,
            });
        }
        let start = (block.write & !(self.page - 1)).max(chunk.write);
        let end = ((block.write + block.len + self.page - 1) & !(self.page - 1))
            .min(chunk.write + chunk.len);
        debug_assert!(end > start);
        // SAFETY: `[start, end)` is a page-aligned sub-range of this chunk's writable view, which is
        // live for as long as the arena is. The range is only ever the writable view, so this cannot
        // make executable pages writable.
        unsafe { vm::protect(start as *mut u8, end - start, protection) }
            .map_err(platform("CodeArena::seal", start, end - start))?;

        // Record it only after the OS has agreed, so the bitmap never claims a protection the pages
        // do not have. Page-granular and outward-rounded, exactly as the call above was: a block
        // that shares a page with another is sealed together with it, and this mirrors that rather
        // than pretending blocks are independent.
        let pages = chunk.pages(self.page, start, end - start);
        let changed = chunk.set_sealed(pages, protection == Protection::Read);
        if changed != 0 {
            let magnitude = changed.unsigned_abs() as u64;
            if changed > 0 {
                self.sealed_pages.fetch_add(magnitude, Ordering::Release);
            } else {
                self.sealed_pages.fetch_sub(magnitude, Ordering::Release);
            }
        }
        Ok(())
    }
}

/// Whether a block's writable range lies wholly inside a chunk's writable view.
///
/// A free function so that it can be tested without an arena, and therefore without an OS: the
/// property it guards is pure arithmetic, and the way it used to fail was pure arithmetic too.
/// `reprotect` computed `end - start` behind a `debug_assert!`, so in a **release** build a `write`
/// below `chunk.write` underflowed that subtraction into a huge page-aligned length which was then
/// handed to `vm::protect`. Unreachable for a block the arena minted — `check_own` establishes that
/// first — so this is the second line, and the only way to exercise it is to ask it directly.
fn block_fits_chunk(write: usize, len: usize, chunk_write: usize, chunk_len: usize) -> bool {
    write >= chunk_write
        && write
            .checked_add(len)
            .is_some_and(|end| end <= chunk_write.saturating_add(chunk_len))
}

impl core::fmt::Debug for CodeArena {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let stats = self.stats();
        f.debug_struct("CodeArena")
            .field("chunks", &stats.chunks)
            .field("mapped", &stats.mapped)
            .field("used", &stats.used)
            .finish()
    }
}

impl Drop for CodeArena {
    fn drop(&mut self) {
        let mut chunks = self.inner.lock();
        for chunk in chunks.drain(..) {
            // SAFETY: both views were mapped by `new_chunk` and are unmapped exactly once, here.
            // The arena is being dropped, so no `CodeBlock` handed out of it may still be in use —
            // a block borrows nothing, so this is the caller's contract, stated on `write_ptr`.
            unsafe {
                let write = chunk.write as *mut u8;
                let exec = chunk.exec as *mut u8;
                if let Err(error) = vm::unmap_and_release(write, chunk.len) {
                    tracing::error!(%error, "unmapping the code arena's writable view failed");
                }
                if let Err(error) = vm::unmap_and_release(exec, chunk.len) {
                    tracing::error!(%error, "unmapping the code arena's executable view failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::block_fits_chunk;

    /// The containment check that stands between a foreign `CodeBlock` and `vm::protect`.
    ///
    /// Pure arithmetic, tested as such, because the failure it guards is arithmetic: the previous
    /// code subtracted a chunk base from a block address with only a `debug_assert!` in the way, so
    /// in a release build an address *below* the chunk wrapped into a length of nearly the whole
    /// address space. Every case here is one the identity check now makes unreachable through the
    /// public API, which is exactly why the arithmetic has to be asked directly.
    #[test]
    fn a_block_only_fits_a_chunk_that_really_contains_it() {
        let (chunk, len) = (0x1_0000usize, 0x1000usize);

        // Inside, at both ends and exactly filling it.
        assert!(block_fits_chunk(chunk, 16, chunk, len));
        assert!(block_fits_chunk(chunk + len - 16, 16, chunk, len));
        assert!(block_fits_chunk(chunk, len, chunk, len));

        // Below the chunk: the underflow case. One byte below is enough.
        assert!(!block_fits_chunk(chunk - 1, 16, chunk, len));
        assert!(!block_fits_chunk(0, 16, chunk, len));
        // Above it, and straddling its end.
        assert!(!block_fits_chunk(chunk + len, 16, chunk, len));
        assert!(!block_fits_chunk(chunk + len - 8, 16, chunk, len));
        assert!(!block_fits_chunk(chunk, len + 1, chunk, len));

        // A length that overflows when added to the address, which is what a `Copy` value carrying
        // a garbage `len` looks like.
        assert!(!block_fits_chunk(chunk, usize::MAX, chunk, len));
        assert!(!block_fits_chunk(usize::MAX, 1, chunk, len));
        // And a chunk extent that would itself overflow is not a licence to accept anything.
        assert!(!block_fits_chunk(0, 1, usize::MAX - 1, 16));
    }
}
