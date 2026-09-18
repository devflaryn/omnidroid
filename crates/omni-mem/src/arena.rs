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
/// takes none, because a block is a private range of memory that no other allocation can overlap.
pub struct CodeArena {
    id: ArenaId,
    config: ArenaConfig,
    page: usize,
    granularity: usize,
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
    /// Takes no lock: the identity check is a field comparison, and a block of *this* arena always
    /// names memory this arena has mapped for as long as it lives.
    ///
    /// # Errors
    ///
    /// [`MemError::ForeignBlock`] if the block was minted by a different arena, or
    /// [`MemError::BlockOverflow`] if the write would run past the end of the block.
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
        // SAFETY: `[block.write + offset, + bytes.len())` is inside the block, which is inside the
        // chunk's writable view — checked above — and the arena never hands the same range out
        // twice, so this is an exclusive write to memory the arena owns. The source and destination
        // cannot overlap: `bytes` is the caller's memory and this range is the arena's.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (block.write + offset) as *mut u8,
                bytes.len(),
            );
        }
        Ok(())
    }

    /// Make a block's pages read-only through the writable view, so nothing can modify live code.
    ///
    /// The executable view is unaffected: the code stays callable. Undo it with
    /// [`unseal`](CodeArena::unseal) to patch the block.
    ///
    /// Page-granular, and it rounds **outwards**: blocks are 16-byte aligned by default, so several
    /// share a page, and sealing one seals its page-mates. Allocate a block of at least a page if it
    /// must be sealed independently.
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

        Ok(Chunk { _section: section, write, exec, len, used: 0 })
    }

    fn reprotect(&self, block: &CodeBlock, protection: Protection) -> MemResult<()> {
        self.check_own(block)?;
        let chunks = self.inner.lock();
        // Was `chunks[block.chunk]`, which panicked for an out-of-range index, and then computed
        // `end - start` guarded only by a `debug_assert!` — so in a release build a block whose
        // `write` sat below `chunk.write` underflowed that subtraction into a huge page-aligned
        // length and handed it to `vm::protect`. Both are now refusals. Unreachable for a block this
        // arena minted, which is what `check_own` above establishes; this is the second line.
        let chunk = chunks.get(block.chunk).ok_or(MemError::BlockOutsideChunk {
            write: block.write,
            len: block.len,
            chunk: block.chunk,
            chunks: chunks.len(),
        })?;
        let outside = block.write < chunk.write
            || block
                .write
                .checked_add(block.len)
                .is_none_or(|end| end > chunk.write + chunk.len);
        if outside {
            return Err(MemError::BlockOutsideChunk {
                write: block.write,
                len: block.len,
                chunk: block.chunk,
                chunks: chunks.len(),
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
            .map_err(platform("CodeArena::seal", start, end - start))
    }
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
