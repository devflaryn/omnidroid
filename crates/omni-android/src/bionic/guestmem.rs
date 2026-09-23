//! `mmap`, `munmap`, `mprotect`, `madvise`, `mlock`, `msync` — the guest's own view of its
//! address space.
//!
//! # This is the heap seam, and it is not `malloc`
//!
//! `libroblox.so` imports **no allocator at all**: it carries its own and reaches the host through
//! guest `mmap`. So these five are where the engine's heap actually comes from, and a wrong answer
//! here is a wrong answer under every allocation the engine ever makes.
//!
//! # Why every one of them is `bind_reentrant`, and nothing in the types says so
//!
//! Task 2's review, **finding F9**: `ImportCall::mem()` reaches `GuestMem::space()` and therefore
//! the whole [`GuestSpace`](omni_mem::GuestSpace), so an *inline* handler — which runs inside one
//! of the translating backend's own callbacks, with generated code live and a `&mut CpuCtx` on the
//! stack — could call `map_anonymous`, `unmap` or `protect`. Two things go wrong there and neither
//! is a type error:
//!
//! * the pager's documented invariant is that **the thread running guest code must not hold this
//!   space's lock**, and an inline handler is that thread;
//! * unmapping or reprotecting a range invalidates memory that live translations reference, from
//!   inside the very callback those translations are executing.
//!
//! Phase 1 avoided it by mapping exactly once, in `Bionic::new`, before any CPU existed. This
//! phase cannot: `mmap` is a guest call. So all five are on the **exit path**, where `cpu.run` has
//! already returned and no callback frame is live. The type system does not enforce this — nothing
//! stops a later editor moving one of these into `INLINE` — so it is stated here, asserted by
//! `dispatch_paths_are_what_f9_requires` in `tests/bionic.rs`, and pinned by mutation rows
//! `guestmem-A1`/`B1`.
//!
//! `msync` is the sixth and is there for a reason of its own: it neither unmaps nor reprotects,
//! but its `MS_SYNC` is disk I/O over a mapping the engine makes 100 MiB long, and the exit path
//! is where a handler can block that long with no translating-backend frame live beneath it.
//!
//! The exit path is also the only one that can reach the CPU, which is what makes
//! [`ReentrantCall::invalidate_code`](crate::ReentrantCall::invalidate_code) available — see
//! [`invalidate`].
//!
//! # The split between a refusal and a `-1`
//!
//! Both exist here and confusing them is the whole risk:
//!
//! * A call this layer **cannot carry out correctly** — a writable private or executable
//!   file-backed `mmap`, `MAP_FIXED`, a protection AArch64 can express and [`Protection`] cannot,
//!   `MADV_REMOVE`, `mlock` — is
//!   [`AbiError::Refused`] naming the symbol, the guest address and the argument. `MAP_FAILED`
//!   would be a *believable* answer: the guest's allocator handles it by trying something else,
//!   and the real failure would surface as an allocation pattern nobody could explain.
//! * A call that is **well-formed and legitimately failed** — no address space left, the commit
//!   ceiling reached, a length of zero — returns what Linux returns, with `errno` set. That is not
//!   a stub; it is the contract. An allocator that cannot handle a failing `mmap` is broken on a
//!   real device too.

use omni_bionic::context::GuestContext;
use omni_bionic::errno::consts;
use omni_mem::{Backing, CommitPolicy, GuestAddr, MemError, Placement, Protection};

use crate::boundary::{ImportCall, ReentrantCall};
use crate::error::{AbiError, AbiResult};

use super::view::GuestView;
use super::{active, Active};

// ------------------------------------------------------------------ the guest's constants
//
// **Linux's values, not the host's.** These are what `libroblox.so` was compiled against; a
// `MAP_ANONYMOUS` taken from a BSD header would be `0x1000` and every anonymous mapping would look
// file-backed. From `asm-generic/mman-common.h` and `asm-generic/mman.h`, which arm64 uses
// unmodified.

/// `PROT_NONE`.
pub const PROT_NONE: i32 = 0x0;
/// `PROT_READ`.
pub const PROT_READ: i32 = 0x1;
/// `PROT_WRITE`.
pub const PROT_WRITE: i32 = 0x2;
/// `PROT_EXEC`.
pub const PROT_EXEC: i32 = 0x4;

/// `MAP_SHARED`.
pub const MAP_SHARED: i32 = 0x01;
/// `MAP_PRIVATE`.
pub const MAP_PRIVATE: i32 = 0x02;
/// `MAP_TYPE`: the mask the sharing mode lives in.
pub const MAP_TYPE: i32 = 0x0f;
/// `MAP_FIXED`.
pub const MAP_FIXED: i32 = 0x10;
/// `MAP_ANONYMOUS`.
pub const MAP_ANONYMOUS: i32 = 0x20;
/// `MAP_NORESERVE`.
pub const MAP_NORESERVE: i32 = 0x4000;
/// `MAP_POPULATE`.
pub const MAP_POPULATE: i32 = 0x8000;
/// `MAP_STACK`.
pub const MAP_STACK: i32 = 0x2_0000;
/// `MAP_FIXED_NOREPLACE`.
pub const MAP_FIXED_NOREPLACE: i32 = 0x10_0000;

/// `MAP_FAILED`, which is `(void *) -1`.
pub const MAP_FAILED: u64 = u64::MAX;

/// Every flag this layer understands. A flag outside it is refused by name rather than ignored:
/// silently dropping `MAP_GROWSDOWN` or `MAP_HUGETLB` would give the guest a mapping that is not
/// the one it asked for, and it would never find out.
const KNOWN_MAP_FLAGS: i32 = MAP_SHARED
    | MAP_PRIVATE
    | MAP_FIXED
    | MAP_ANONYMOUS
    | MAP_NORESERVE
    | MAP_POPULATE
    | MAP_STACK
    | MAP_FIXED_NOREPLACE;

/// `MADV_DONTNEED`.
pub const MADV_DONTNEED: i32 = 4;
/// `MADV_FREE`.
pub const MADV_FREE: i32 = 8;
/// `MADV_REMOVE`.
pub const MADV_REMOVE: i32 = 9;

/// `MS_ASYNC`.
pub const MS_ASYNC: i32 = 1;
/// `MS_INVALIDATE`.
pub const MS_INVALIDATE: i32 = 2;
/// `MS_SYNC`. DECODED: the one value the engine passes (`libroblox.so` link `0x6222e50`).
pub const MS_SYNC: i32 = 4;

/// The advices that are hints, and which a conforming implementation may ignore entirely.
///
/// `MADV_NORMAL`/`RANDOM`/`SEQUENTIAL`/`WILLNEED` are read-ahead policy; `DONTFORK`, `DOFORK`,
/// `WIPEONFORK` and `KEEPONFORK` describe what happens across a `fork` this runtime has none of;
/// `MERGEABLE`/`UNMERGEABLE` are kernel same-page merging; `HUGEPAGE`/`NOHUGEPAGE` are page-size
/// policy; `DONTDUMP`/`DODUMP` are core-dump policy; `COLD`/`PAGEOUT` are reclaim hints. Every one
/// of them leaves the *contents* of the range exactly as they were, which is the property that
/// makes ignoring them correct rather than convenient.
const ADVISORY: &[i32] = &[0, 1, 2, 3, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21];

// ------------------------------------------------------------------ shared plumbing

/// Set this thread's `errno` and hand back the value the call returns.
fn fail(view: &mut GuestView<'_>, errno: i32) {
    view.set_errno(errno);
}

/// Map a `GuestSpace` failure onto the `errno` Linux would report.
///
/// Deliberately narrow. `ENOMEM` is what `mmap` reports when the kernel will not give the address
/// space, and the commit ceiling and a full space are both that; `EINVAL` is what it reports for
/// an argument it cannot use. Nothing here reports a success.
fn errno_for(error: &MemError) -> i32 {
    match error {
        MemError::ZeroSize { .. } | MemError::Misaligned { .. } | MemError::OutsideSpace { .. } => {
            consts::EINVAL
        }
        _ => consts::ENOMEM,
    }
}

/// AArch64's `PROT_*` bits as one of [`Protection`]'s four states, or a refusal saying why there
/// is no such state.
///
/// **The two that are refused are refused because widening is silent.** `PROT_WRITE` alone and
/// `PROT_EXEC` alone have no read-less form here, and answering them with `ReadWrite` or
/// `ReadExecute` would hand the guest *more* permission than it asked for — the direction Global
/// Constraint 11 calls out, where saturating a limit turns hostile input into a larger permission.
/// `PROT_WRITE | PROT_EXEC` is refused because D12 makes W^X an invariant of this runtime with one
/// recorded exception, and the guest's own mappings are not it.
fn protection_for(prot: i32) -> Result<Protection, String> {
    match prot {
        PROT_NONE => Ok(Protection::None),
        p if p == PROT_READ => Ok(Protection::Read),
        p if p == PROT_READ | PROT_WRITE => Ok(Protection::ReadWrite),
        p if p == PROT_READ | PROT_EXEC => Ok(Protection::ReadExecute),
        p if p & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 => Err(format!(
            "the protection {p:#x} sets bits outside PROT_READ|PROT_WRITE|PROT_EXEC, and this \
             layer will not guess what they mean"
        )),
        p if p == PROT_WRITE || p == PROT_EXEC => Err(format!(
            "the protection {p:#x} asks for write or execute without read. `omni_mem::Protection` \
             has no such state, and answering with the readable form would grant the guest more \
             access than it asked for"
        )),
        p => Err(format!(
            "the protection {p:#x} asks for write and execute at once, which D12 makes an \
             invariant of this runtime with one recorded exception that is not a guest mapping"
        )),
    }
}

/// Discard any translated code covering `[address, len)` **in this context**.
///
/// Mandatory after an unmap or a reprotect: the backend caches translations by guest address, and
/// a guest that unmaps code and maps something else at the same address would otherwise execute
/// the old translation. This is the other half of why these handlers are on the exit path — an
/// inline handler holds no CPU at all, by design, so it could not do this even if it were safe to.
///
/// **Stated limitation, because it is a real one.** `GuestCpu::invalidate_code` is per context,
/// and this reaches the context the calling guest thread is running on. A *second* guest thread
/// that had already translated the same code keeps its translation. Closing that needs a registry
/// of live contexts, which the boundary does not have and which is the thread-lifecycle phase's to
/// build; until then this is a narrowing of the window rather than a closing of it, and it is
/// labelled as one rather than described as complete.
fn invalidate(c: &mut ReentrantCall<'_>, address: GuestAddr, len: usize) -> AbiResult<()> {
    if len == 0 {
        return Ok(());
    }
    c.invalidate_code(address, len)
}

/// Round a length up to the guest's page size, as every one of these calls does.
///
/// Checked rather than saturating: a length near `usize::MAX` rounded up would wrap to a *small*
/// number and turn a refusal into a mapping, which is Global Constraint 11's "saturating
/// arithmetic on a limit turns hostile input into a larger permission" exactly.
fn pages(len: u64, page: usize) -> Option<usize> {
    let len = usize::try_from(len).ok()?;
    let mask = page.checked_sub(1)?;
    len.checked_add(mask).map(|n| n & !mask)
}

/// The state, the memory and the names every one of these handlers starts with.
///
/// Owns all of it rather than borrowing the call, because every one of these handlers has to hold
/// it across a `&mut` use of the call — `ret`, and `invalidate_code`.
struct Call {
    symbol: String,
    address: GuestAddr,
    state: Active,
    mem: crate::mem::GuestMem,
}

impl Call {
    fn begin(c: &ReentrantCall<'_>) -> AbiResult<Self> {
        let symbol = c.symbol().to_string();
        let address = c.address();
        let state = active(&symbol, address)?;
        Ok(Self { symbol, address, state, mem: c.mem().clone() })
    }

    fn view(&self) -> GuestView<'_> {
        GuestView::new(&self.mem, &self.symbol, self.address, &self.state)
    }

    fn refuse<T>(&self, why: impl Into<String>) -> AbiResult<T> {
        Err(AbiError::Refused {
            symbol: self.symbol.clone(),
            address: self.address,
            why: why.into(),
        })
    }
}

// ------------------------------------------------------------------ the handlers

/// `void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset)`
///
/// Anonymous mappings lazily committed — which is what Linux does and what makes the demand pager
/// the heap seam rather than an eager commit of the engine's whole arena — and read-only file
/// mappings, made as a snapshot of the file.
///
/// # A read-only file mapping is the file's bytes, copied in
///
/// MEASURED reader: the engine's `ReadOnlySharedBuffer`, which turns a finished download (16 MB,
/// written through a `FILE *`) into memory with `fileno` and
/// `mmap(NULL, size, PROT_READ, MAP_SHARED | MAP_NORESERVE, fd, 0)`. A host file view is not the
/// answer here: the engine still holds the file open for writing, and the host mapping open
/// shares reads only. So the pages are anonymous, filled from the file with `pread` and then
/// protected read-only -- byte for byte what the guest reads, including the zeros past the end of
/// the file in its last page. **What differs is visibility of later writes**: a `MAP_SHARED`
/// mapping on Linux shows writes made to the file after it was mapped, and this one does not.
/// Nothing reachable writes a file it has mapped read-only (the buffer is read-only by
/// construction); if something does, this is where it will be. Pages wholly past the end of the
/// file read as zero rather than raising `SIGBUS`.
///
/// # A writable `MAP_SHARED` file mapping is a real view of the file
///
/// MEASURED: three engine threads in the first signed-in session died on this handler's refusal,
/// each asking for `mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0)` of a file under
/// `files/` (`UniversalApp_cache.tmp.0`, `ota_rbxm_decompressed_cache/.../DataModelPatch_*.tmp.N`).
/// DECODED, `libroblox.so` link addresses: the site is `MappedFile::open` at `0x2273210`, called
/// from `RbxmFileManager`'s `OutputMappedFileWrapper` (vtable at `0x638e340`), and it is the
/// **only** writable file mapping in the library -- the other fourteen `mmap` sites are anonymous
/// or `PROT_READ`. What the engine does around it decides what this has to honour:
///
/// * `open(path, O_RDWR | O_CREAT | O_TRUNC, 0666)`, `posix_fallocate(fd, 0, len)`, the `mmap`,
///   and **`close(fd)` on the next instruction** (`0x2273404`). So the mapping outlives its
///   descriptor, and nothing ever reads or writes the file through one while it is mapped.
/// * `len` is `0x6400000` (100 MiB) at first (`0x2d8b590`); the chunks are decompressed into a
///   heap buffer; then `resize(n)` unmaps and **re-opens the path `O_TRUNC`**, fallocates `n`
///   and maps `n` bytes (`0x6222d0c`) -- so `n` is any length, not a page multiple. It `memcpy`s
///   the buffer in (`0x2275834`), `msync(ptr, n, MS_SYNC)` (`0x6222e30`), `munmap`s (`0x24b459c`),
///   and only then renames the `.tmp` into place (`AtomicCacheWrite`, `0x2d850c0`). On a read
///   failure it removes the `.tmp` *while still mapped* (`0x2d851c0`) and unmaps afterwards.
///
/// So the mapping is a host view of a `PAGE_READWRITE` section built over the guest's own
/// descriptor (`Filesystem::share_for_mapping`, `Backing::share`), at the guest address like every
/// other mapping. That is coherent with `read`/`write` through any other descriptor in both
/// directions with no flush, costs no private commit charge however long it is, and needs no
/// write-back at `munmap` or at teardown -- the host's file cache holds the writes, as Linux's page
/// cache does. The rename and the remove-while-mapped both work on this host (measured). Linux's
/// partial last page is honoured: the bytes past the end of the file read as zero and a store
/// there never reaches it.
///
/// **What it cannot do, refused rather than approximated:**
///
/// * Shortening the file while it is mapped -- `ftruncate`, or an `open` with `O_TRUNC` -- is
///   refused *by the host* (`ERROR_USER_MAPPED_FILE`, 1224), which the descriptor layer reports as
///   a refusal naming it. Linux allows it and faults later accesses. The engine unmaps first.
/// * A mapping reaching whole pages past the end of the file, and a mapping of an empty file.
///   Linux maps both and raises `SIGBUS` on touching the pages; a host view cannot extend past its
///   file without growing the file, which would be a write the guest never made.
/// * A `PROT_READ` `MAP_SHARED` mapping of the same file is still the snapshot above and does not
///   see this mapping's writes. Nothing decoded maps one file both ways.
///
/// A writable `MAP_PRIVATE` file mapping (copy-on-write) and an executable one are still refused
/// by name: nothing in the library asks for either.
pub(super) fn mmap(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (addr, length, prot, flags, fd, offset) = {
        let mut a = c.args();
        (
            a.next_u64()?,
            a.next_u64()?,
            a.next_i32()?,
            a.next_i32()?,
            a.next_i32()?,
            a.next_u64()? as i64,
        )
    };
    let call = Call::begin(c)?;
    let space = call.mem.space();
    let page = space.page_size();

    // **A file-backed mapping is one without `MAP_ANONYMOUS`, and `fd` says nothing.**
    //
    // This used to read `if fd != -1 || flags & MAP_ANONYMOUS == 0`, and that extra clause was
    // wrong: Linux **ignores `fd` entirely** when `MAP_ANONYMOUS` is set. `mmap(2)` says so — "the
    // fd argument is ignored; however, some implementations require fd to be -1 ... portable
    // applications should ensure this" — and the kernel's anonymous path never looks at it.
    // Passing `0` is legal and ordinary.
    //
    // **M3's gate is what found it, at `init_array[188]`**, where the engine's own allocator asks
    // for `mmap(NULL, len, prot, MAP_PRIVATE|MAP_ANONYMOUS, 0, 0)`. Every such call was refused as
    // "file-backed" — which is the guest **heap seam** refusing every allocation, since
    // `libroblox.so` imports no allocator at all and guest `mmap` is where its heap comes from. No
    // test here could see it: every one of them passes `-1`, which is what the manual page tells
    // applications to do and what nothing is obliged to do.
    let file_backed = flags & MAP_ANONYMOUS == 0;
    // The one writable file mapping there is: a view of the file itself (see `mmap`).
    let shared_writable =
        file_backed && prot == PROT_READ | PROT_WRITE && flags & MAP_TYPE == MAP_SHARED;
    if file_backed && prot != PROT_READ && !shared_writable {
        // The file by name, which the descriptor's number alone does not give.
        let named = super::files::filesystem(&call.view())
            .ok()
            .and_then(|fs| fs.guest_path_of(fd))
            .map_or_else(|| "a descriptor no path names".to_string(), |path| format!("`{path}`"));
        return call.refuse(format!(
            "the guest asked for a file-backed mapping of fd {fd} ({named}) with prot {prot:#x}, \
             flags {flags:#x}, offset {offset:#x}. A file mapping here is PROT_READ, made as a \
             snapshot of the file, or PROT_READ|PROT_WRITE with MAP_SHARED, made as a view that \
             writes the file (see `mmap`). A writable MAP_PRIVATE mapping is a copy-on-write copy \
             of the file and an executable one is code; the one writable file mapping in \
             libroblox.so is MAP_SHARED (link 0x22733f0), and no run has asked for either"
        ));
    }
    if flags & !KNOWN_MAP_FLAGS != 0 {
        return call.refuse(format!(
            "the guest passed flags {flags:#x}, of which {:#x} is outside the set this layer \
             implements. Ignoring an unknown flag would give the guest a mapping that is not the \
             one it asked for",
            flags & !KNOWN_MAP_FLAGS
        ));
    }
    if flags & MAP_FIXED != 0 {
        return call.refuse(format!(
            "the guest asked for MAP_FIXED at {addr:#x}. Linux MAP_FIXED silently unmaps whatever \
             is already there, and `Placement::Fixed` deliberately refuses an occupied range \
             instead — it behaves like MAP_FIXED_NOREPLACE. Honouring MAP_FIXED would mean \
             destroying a mapping this layer cannot see the guest still using, so the two \
             spellings are kept apart and only the one with the checkable meaning is implemented"
        ));
    }

    let mut view = call.view();
    // **Linux's two answers, kept apart.** A length of zero is `EINVAL`; a length that cannot be
    // rounded up to a page — because it would wrap, or because it is wider than this host's
    // `usize` — is `ENOMEM`, which is what `PAGE_ALIGN(len) == 0` gives there.
    if length == 0 {
        fail(&mut view, consts::EINVAL);
        c.ret(|mut r| r.u64(MAP_FAILED));
        return Ok(());
    }
    let Some(len) = pages(length, page) else {
        fail(&mut view, consts::ENOMEM);
        c.ret(|mut r| r.u64(MAP_FAILED));
        return Ok(());
    };

    // MAP_SHARED on anonymous memory shares only with children of a `fork`, and there is no
    // `fork` here — it is not in the reachable set and there is no process surface to build one
    // on. Within one process the two sharing modes are indistinguishable, so both are accepted
    // and neither is a guess.
    if !matches!(flags & MAP_TYPE, MAP_PRIVATE | MAP_SHARED) {
        fail(&mut view, consts::EINVAL);
        c.ret(|mut r| r.u64(MAP_FAILED));
        return Ok(());
    }

    let protection = match protection_for(prot) {
        Ok(protection) => protection,
        Err(why) => return call.refuse(why),
    };

    let placement = if flags & MAP_FIXED_NOREPLACE != 0 {
        let at = usize::try_from(addr).unwrap_or(usize::MAX);
        if at == 0 || at % page != 0 {
            fail(&mut view, consts::EINVAL);
            c.ret(|mut r| r.u64(MAP_FAILED));
            return Ok(());
        }
        Placement::Fixed(at)
    } else if addr != 0 {
        match usize::try_from(addr) {
            // A hint is a preference, and a misaligned or unusable one is not an error on Linux —
            // it is simply not honoured. Rounding it down rather than refusing is what `mmap`
            // does.
            Ok(at) => Placement::Hint { address: at & !(page - 1), align: page },
            Err(_) => Placement::Anywhere { align: page },
        }
    } else {
        Placement::Anywhere { align: page }
    };

    if file_backed {
        // Linux: the offset must be a multiple of the page size.
        if offset < 0 || offset as u64 % page as u64 != 0 {
            fail(&mut view, consts::EINVAL);
            c.ret(|mut r| r.u64(MAP_FAILED));
            return Ok(());
        }
        if shared_writable {
            return map_shared_file(c, &call, fd, offset as u64, len, placement);
        }
        let fs = super::files::filesystem(&view)?;
        let mut host = vec![0u8; usize::try_from(length).unwrap_or(len).min(len)];
        let read = match super::files::settle(&view, fs.read_for_mapping(fd, &mut host, offset as u64))? {
            super::files::Settled::Done(read) => read,
            super::files::Settled::Failed(errno) => {
                fail(&mut view, errno);
                c.ret(|mut r| r.u64(MAP_FAILED));
                return Ok(());
            }
        };
        let at = match space.map_anonymous(placement, len, Protection::ReadWrite, CommitPolicy::Eager) {
            Ok(at) => at,
            Err(error) => {
                fail(&mut view, errno_for(&error));
                c.ret(|mut r| r.u64(MAP_FAILED));
                return Ok(());
            }
        };
        let blame = crate::mem::Blame::new(&call.symbol, call.address, 4);
        call.mem.write_bytes(at, &host[..read], blame)?;
        if let Err(error) = space.protect(at, len, Protection::Read) {
            let _ = space.unmap(at, len);
            fail(&mut view, errno_for(&error));
            c.ret(|mut r| r.u64(MAP_FAILED));
            return Ok(());
        }
        invalidate(c, at, len)?;
        c.ret(|mut r| r.u64(at as u64));
        return Ok(());
    }

    match space.map_anonymous(placement, len, protection, CommitPolicy::Lazy) {
        Ok(at) => {
            // A fresh mapping cannot hold code the backend has translated — the address range was
            // free — but it can *reuse* addresses a previous mapping held, and those translations
            // are still cached. Invalidating on the way in is the cheaper half of the pair: the
            // range is about to be written by the guest anyway.
            invalidate(c, at, len)?;
            c.ret(|mut r| r.u64(at as u64));
        }
        Err(error) => {
            let mut view = call.view();
            fail(&mut view, errno_for(&error));
            c.ret(|mut r| r.u64(MAP_FAILED));
        }
    }
    Ok(())
}

/// The writable `MAP_SHARED` half of [`mmap`]: a view of the very file `fd` names, placed as
/// `placement` says. `offset` is page-aligned and `len` a page multiple by the time this runs.
///
/// Linux's failures stay failures (`EBADF`, `EACCES` for a descriptor not open for reading *or*
/// not open for writing -- `do_mmap`'s rule for `MAP_SHARED` with `PROT_WRITE`), and what the host
/// cannot express is refused by name; see [`mmap`] for which is which.
fn map_shared_file(
    c: &mut ReentrantCall<'_>,
    call: &Call,
    fd: i32,
    offset: u64,
    len: usize,
    placement: Placement,
) -> AbiResult<()> {
    let space = call.mem.space();
    let mut view = call.view();
    let fs = super::files::filesystem(&view)?;
    let name = fs.guest_path_of(fd).unwrap_or_else(|| format!("fd {fd}"));
    let file = match super::files::settle(&view, fs.share_for_mapping(fd))? {
        super::files::Settled::Done(file) => file,
        super::files::Settled::Failed(errno) => {
            fail(&mut view, errno);
            c.ret(|mut r| r.u64(MAP_FAILED));
            return Ok(());
        }
    };
    // The file's length **now**, which is what the section will be: the engine `posix_fallocate`s
    // exactly the length it then maps, so the mapping ends inside the file's last page.
    let file_len = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            return call.refuse(format!(
                "the length of `{name}` (fd {fd}) could not be read for a writable MAP_SHARED \
                 mapping of it: {error}"
            ))
        }
    };
    let page = space.page_size() as u64;
    let last_page_end = file_len.div_ceil(page).saturating_mul(page);
    if offset.checked_add(len as u64).is_none_or(|end| end > last_page_end) {
        return call.refuse(format!(
            "the guest asked for a writable MAP_SHARED mapping of {len} bytes at offset \
             {offset:#x} of `{name}` (fd {fd}), which is {file_len} bytes long, so the mapping \
             reaches whole pages past the end of the file. Linux maps them and raises SIGBUS on \
             touching one; a host view cannot extend past its file without growing the file, \
             which would be a write the guest never made. DECODED: the engine's one writable file \
             mapping maps exactly the length it has just posix_fallocate'd (libroblox.so link \
             0x2273210)"
        ));
    }
    let backing = match Backing::share(file, &name) {
        Ok(backing) => backing,
        Err(error) => {
            return call.refuse(format!(
                "`{name}` (fd {fd}) could not be made mappable as a writable MAP_SHARED file: \
                 {error}"
            ))
        }
    };
    match space.map_file(&backing, offset, placement, len, Protection::ReadWrite) {
        Ok(at) => {
            // As for an anonymous mapping: the addresses may have held code before.
            invalidate(c, at, len)?;
            c.ret(|mut r| r.u64(at as u64));
        }
        // No room in the address space, or a MAP_FIXED_NOREPLACE collision: the answers an
        // anonymous mapping gives for the same thing.
        Err(error) if !matches!(error, MemError::Platform { .. }) => {
            fail(&mut view, errno_for(&error));
            c.ret(|mut r| r.u64(MAP_FAILED));
        }
        // The host refused the view itself. `errno_for` would call that ENOMEM, which is a
        // believable answer for a failure nobody has identified.
        Err(error) => {
            return call.refuse(format!(
                "a writable MAP_SHARED view of `{name}` (fd {fd}), {len} bytes at offset \
                 {offset:#x}, could not be mapped: {error}"
            ))
        }
    }
    Ok(())
}

/// `int munmap(void *addr, size_t length)`
pub(super) fn munmap(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (addr, length) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let call = Call::begin(c)?;
    let space = call.mem.space();
    let page = space.page_size();
    let at = usize::try_from(addr).unwrap_or(usize::MAX);
    let len = pages(length, page).filter(|&n| n != 0);

    let Some(len) = len.filter(|_| at % page == 0) else {
        let mut view = call.view();
        fail(&mut view, consts::EINVAL);
        c.ret(|mut r| r.i32(-1));
        return Ok(());
    };

    // **Before the unmap, not after.** Once the range is gone `GuestRange` still describes it, but
    // the window between the unmap and the invalidate is a window in which another guest thread
    // could execute a translation of memory this process no longer owns.
    invalidate(c, at, len)?;
    match space.unmap(at, len) {
        Ok(()) => c.ret(|mut r| r.i32(0)),
        Err(error) => {
            let mut view = call.view();
            fail(&mut view, errno_for(&error));
            c.ret(|mut r| r.i32(-1));
        }
    }
    Ok(())
}

/// `int mprotect(void *addr, size_t len, int prot)`
pub(super) fn mprotect(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (addr, length, prot) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_i32()?)
    };
    let call = Call::begin(c)?;
    let space = call.mem.space();
    let page = space.page_size();
    let at = usize::try_from(addr).unwrap_or(usize::MAX);

    let protection = match protection_for(prot) {
        Ok(protection) => protection,
        Err(why) => return call.refuse(why),
    };

    // `mprotect` over zero bytes is a no-op that succeeds, which is what Linux answers; only a
    // misaligned address or an unusable length is an error.
    if length == 0 && at % page == 0 {
        c.ret(|mut r| r.i32(0));
        return Ok(());
    }
    let Some(len) = pages(length, page).filter(|&n| n != 0 && at % page == 0) else {
        let mut view = call.view();
        fail(&mut view, consts::EINVAL);
        c.ret(|mut r| r.i32(-1));
        return Ok(());
    };

    invalidate(c, at, len)?;
    match space.protect(at, len, protection) {
        Ok(()) => c.ret(|mut r| r.i32(0)),
        Err(error) => {
            let mut view = call.view();
            fail(&mut view, errno_for(&error));
            c.ret(|mut r| r.i32(-1));
        }
    }
    Ok(())
}

/// `int madvise(void *addr, size_t length, int advice)`
///
/// **`MADV_FREE` and `MADV_DONTNEED` differ in *when*, and that is the whole content of this
/// handler.** `MADV_FREE` says the kernel may drop the pages and that a later read sees either
/// the old contents or zeroes, which is [`advise_idle`](omni_mem::GuestSpace::advise_idle) alone.
/// `MADV_DONTNEED` is stronger: on private anonymous memory a later read is *guaranteed* to be
/// zero, immediately.
///
/// # `MADV_DONTNEED` was refused and is now carried out — and the earlier reasoning was wrong
///
/// D21 refused it, on the argument that meeting the guarantee would mean **writing zeroes** over
/// the range and so committing every lazy granule the call was asking to release. That argument
/// assumed the only way to zero a range is to write to it. It is not:
/// [`advise_idle`](omni_mem::GuestSpace::advise_idle) followed by
/// [`reclaim_idle`](omni_mem::GuestSpace::reclaim_idle) **decommits** the granules with
/// `MEM_DECOMMIT` — D10's only primitive that gives commit charge back — and the demand pager
/// faults the range back in on the next access as a freshly committed, **zero-filled** page. The
/// guarantee is met by giving the memory back rather than by writing to it, which is the
/// direction the call was asking for in the first place.
///
/// **Found by M4's gate**, where `JNI_OnLoad` reaches the engine's own heap trim and this refusal
/// stopped §8 step 6. It is a correction to D21 rather than a relaxation of it: the refusal did
/// exactly what it was written to do, and what was wrong was its premise.
///
/// # Two things a reader needs to know
///
/// * **`reclaim_idle` is space-wide.** It decommits every granule any earlier `MADV_FREE` marked,
///   not only this call's range. `MADV_FREE`'s contract permits the pages to be dropped at any
///   time, so that is correct; what it costs is that one `MADV_DONTNEED` makes every outstanding
///   `MADV_FREE` take effect at once.
/// * **A partial range is refused rather than half-done.** `advise_idle` reports how many bytes
///   it marked, and an entry it could not split stays in use. Returning `0` after marking less
///   than the whole range would promise zeroes this layer had not delivered, which is precisely
///   the believable wrong answer the refusal existed to prevent.
///
/// `MADV_REMOVE` stays refused: it is defined on shared, file-backed mappings as punching a hole
/// in the **underlying object**. The writable `MAP_SHARED` views [`mmap`] makes are the only
/// mappings here with one -- the guest's file -- and no run has asked to punch a hole in it.
pub(super) fn madvise(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (addr, length, advice) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_i32()?)
    };
    let call = Call::begin(c)?;
    let space = call.mem.space();
    let page = space.page_size();
    let at = usize::try_from(addr).unwrap_or(usize::MAX);

    if advice == MADV_REMOVE {
        return call.refuse(format!(
            "the guest asked for MADV_REMOVE over {length} bytes at {at:#x}. It is defined as \
             punching a hole in the object *underlying* a shared mapping. The only such mappings \
             here are writable MAP_SHARED file views, where that object is the guest's file and \
             no run has asked for a hole in it; every other guest mapping is private and \
             anonymous, with no underlying object to punch. MADV_DONTNEED, which is what a \
             private anonymous range wants, is carried out"
        ));
    }

    if advice == MADV_DONTNEED {
        let Some(len) = pages(length, page).filter(|&n| n != 0 && at % page == 0) else {
            let mut view = call.view();
            fail(&mut view, consts::EINVAL);
            c.ret(|mut r| r.i32(-1));
            return Ok(());
        };
        if let Err(error) = space.advise_idle(at, len) {
            let mut view = call.view();
            fail(&mut view, errno_for(&error));
            c.ret(|mut r| r.i32(-1));
            return Ok(());
        }
        if let Err(error) = space.reclaim_idle() {
            let mut view = call.view();
            fail(&mut view, errno_for(&error));
            c.ret(|mut r| r.i32(-1));
            return Ok(());
        }
        // **The guarantee, checked directly rather than through a proxy.**
        //
        // `advise_idle` returns how many bytes it *newly* marked, which is zero for a range an
        // earlier `MADV_FREE` already marked -- so a `marked < len` test refuses the ordinary
        // free-then-dontneed sequence an allocator makes, which is what it did the first time it
        // was written. What actually has to be true is that nothing in the range is committed any
        // more, and `RegionInfo::committed` says exactly that, per entry and unmerged.
        let mut cursor = at;
        while cursor < at + len {
            let Some(region) = space.region_at(cursor) else {
                return call.refuse(format!(
                    "MADV_DONTNEED over {len} bytes at {at:#x} reaches unmapped address \
                     {cursor:#x}, so the range it guarantees zeroes for is not all mapped"
                ));
            };
            if region.committed != 0 {
                return call.refuse(format!(
                    "MADV_DONTNEED over {len} bytes at {at:#x} left {} committed bytes at {:#x}, \
                     so a later read there would return the old contents rather than the zeroes \
                     the call guarantees. Reporting success would be the believable wrong answer",
                    region.committed, region.start
                ));
            }
            cursor = region.end();
        }
        // The bytes at those addresses are gone, so any translation covering them is stale. The
        // same argument `munmap` and `mprotect` make, and part of why all five of these are on
        // the exit path at all (finding F9).
        c.invalidate_code(at, len)?;
        c.ret(|mut r| r.i32(0));
        return Ok(());
    }

    if advice == MADV_FREE {
        let Some(len) = pages(length, page).filter(|&n| n != 0 && at % page == 0) else {
            let mut view = call.view();
            fail(&mut view, consts::EINVAL);
            c.ret(|mut r| r.i32(-1));
            return Ok(());
        };
        match space.advise_idle(at, len) {
            Ok(_) => c.ret(|mut r| r.i32(0)),
            Err(error) => {
                let mut view = call.view();
                fail(&mut view, errno_for(&error));
                c.ret(|mut r| r.i32(-1));
            }
        }
        return Ok(());
    }

    if ADVISORY.contains(&advice) {
        // Nothing to do, and nothing pretended: every advice in this set leaves the contents of
        // the range untouched by definition, so ignoring it is the documented latitude rather
        // than a stub.
        c.ret(|mut r| r.i32(0));
        return Ok(());
    }

    // What Linux itself answers for an advice it does not know.
    let mut view = call.view();
    fail(&mut view, consts::EINVAL);
    c.ret(|mut r| r.i32(-1));
    Ok(())
}

/// `int mlock(const void *addr, size_t len)`
///
/// Refused. `mlock`'s contract is that the range is resident *and stays* resident — no major
/// fault, never paged out — and nothing in `omni-platform` can make that promise: there is no
/// `VirtualLock`, no `mlock`, and committing a range through the pager gives residency without
/// permanence.
///
/// **`-1` with `ENOMEM` was considered and rejected**, and it is worth saying why, because it is
/// the most tempting wrong answer in this file: a failed `mlock` is ordinary on a real device,
/// `RLIMIT_MEMLOCK` is small, and well-written code handles it. That is exactly what makes it
/// dangerous — the guest would carry on believing it had asked and been refused by policy, when in
/// fact nobody asked. Global Constraint 1 is about the believable answer, not the obviously wrong
/// one.
pub(super) fn mlock(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (addr, len) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let call = Call::begin(c)?;
    call.refuse(format!(
        "the guest asked to lock {len} bytes at {addr:#x} into memory. `omni-platform` is virtual \
         memory and faults only — it has no VirtualLock and no mlock — and committing the range \
         through the pager would make it resident without making it stay resident, which is the \
         half of the contract that matters. Returning -1/ENOMEM was rejected deliberately: a \
         failed mlock is ordinary on a real device, so the guest would record a refusal by policy \
         for a request nobody made"
    ))
}

/// `int msync(void *addr, size_t length, int flags)`
///
/// `mm/msync.c`, in its order: a flag outside `MS_ASYNC | MS_INVALIDATE | MS_SYNC`, a misaligned
/// address, or `MS_ASYNC` with `MS_SYNC`, is `EINVAL`; a length that wraps when rounded up to a
/// page is `ENOMEM`; a length of zero succeeds. Unmapped address space inside the range is
/// **skipped and then reported** as `ENOMEM`, after the mapped parts have been written back.
///
/// # What each flag does here, and why it is what Linux does
///
/// * **`MS_SYNC`** writes every writable `MAP_SHARED` file view in the range back to its file and
///   the file to the device ([`GuestSpace::sync`](omni_mem::GuestSpace::sync)), which is Linux's
///   `vfs_fsync_range`. Anonymous memory and the `PROT_READ` snapshots have nothing of their own to
///   write, and Linux skips them the same way: it syncs only a `VM_SHARED` mapping with a file.
///   DECODED: this is the one flag the engine passes, from its `MappedFile` flush at
///   `libroblox.so` link `0x6222e30`, with the length it has written rather than the mapping's.
/// * **`MS_ASYNC` does nothing, and that is Linux's answer, not a shortcut.** Since 2.6.19 the
///   kernel tracks dirty pages itself and `MS_ASYNC` only walks the range for `ENOMEM`. Here the
///   host's file cache holds a shared view's writes and they are already visible to every reader
///   of the file (measured), so there is equally nothing to start.
/// * **`MS_INVALIDATE`** fails only on a locked mapping (`EBUSY`), and `mlock` is refused here,
///   so it changes nothing either -- again Linux's own behaviour.
///
/// A host write-back failure is refused by name rather than answered `EIO`: nothing has
/// identified it, and `EIO` is an answer guest code acts on.
pub(super) fn msync(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (addr, length, flags) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_i32()?)
    };
    let call = Call::begin(c)?;
    let space = call.mem.space();
    let page = space.page_size();
    let at = usize::try_from(addr).unwrap_or(usize::MAX);

    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0
        || at % page != 0
        || (flags & MS_ASYNC != 0 && flags & MS_SYNC != 0)
    {
        let mut view = call.view();
        fail(&mut view, consts::EINVAL);
        c.ret(|mut r| r.i32(-1));
        return Ok(());
    }
    let Some((len, end)) =
        pages(length, page).and_then(|len| at.checked_add(len).map(|end| (len, end)))
    else {
        let mut view = call.view();
        fail(&mut view, consts::ENOMEM);
        c.ret(|mut r| r.i32(-1));
        return Ok(());
    };
    if len == 0 {
        c.ret(|mut r| r.i32(0));
        return Ok(());
    }

    // Every guest mapping is inside the space, so whatever of the range is outside it is a hole.
    let from = at.max(space.base());
    let to = end.min(space.end());
    let mut hole = from != at || to != end || from >= to;
    let mut cursor = from;
    while !hole && cursor < to {
        match space.region_at(cursor) {
            Some(region) => cursor = region.end(),
            None => hole = true,
        }
    }

    if flags & MS_SYNC != 0 && from < to {
        if let Err(error) = space.sync(from, to - from) {
            return call.refuse(format!(
                "msync(MS_SYNC) over {length} bytes at {at:#x}: the host could not write a shared \
                 file mapping in the range back to its file: {error}"
            ));
        }
    }
    if hole {
        let mut view = call.view();
        fail(&mut view, consts::ENOMEM);
        c.ret(|mut r| r.i32(-1));
    } else {
        c.ret(|mut r| r.i32(0));
    }
    Ok(())
}

// ================================================================== the allocator that is not here

/// Bytes of a guest `struct mallinfo`: ten `size_t` fields on LP64.
///
/// **Stated so the refusal can name it**, not so anything can write one. Bionic's `mallinfo` is
/// `size_t arena, ordblks, smblks, hblks, hblkhd, usmblks, fsmblks, uordblks, fordblks,
/// keepcost` — ten machine words, which is 80 bytes on LP64 and is why this is the only reachable
/// import that returns through `X8` (task 2's review found the brief's "returns in X0/X1/V0"
/// omitted the indirect result register; [`Args::indirect_result`](crate::Args::indirect_result)
/// is what would read it).
pub const MALLINFO_BYTES: usize = 80;

/// `struct mallinfo mallinfo(void)`
///
/// **Ten zeroed `size_t` fields, and the reasoning that used to refuse them is corrected rather
/// than deleted, because what changed is one step of it and not the principle.**
///
/// The facts are unchanged. `libroblox.so` **imports no allocator at all** (D17): no `malloc`, no
/// `free`, no `calloc`, no `realloc`. It carries its own -- Mimalloc, which the engine announces
/// by name at startup -- and reaches the host through guest `mmap`, which is the seam this module
/// is. So `mallinfo` here describes libc's heap, and libc's heap in this process has no
/// relationship to the guest's memory.
///
/// What the old text called "the believable wrong answer" it also called, in the same sentence,
/// *arithmetically true*: zero arena, zero blocks, zero in use, of a heap nothing has allocated
/// from. Those cannot both stand. **It is not a wrong answer; it is the right one**, and the
/// worry underneath it was a different worry: that a human reading a log line would take "0 bytes
/// allocated" for "this program uses no memory". That is a reason to write this comment, not a
/// reason to fail a call the engine needs.
///
/// MEASURED: `nativePostClientSettingsLoadedInitialization3` stops here. The three call sites are
/// `0x02283420`, `0x02283470` and `0x022834c0`, and each is a one-line getter -- the first reads
/// the field at `+0x38`, `uordblks`, and returns it. This is the engine's own memory telemetry
/// asking libc how much of *libc's* heap it is using, and the answer is none of it.
///
/// The other available answer is still rejected and for the reason it always was: reporting this
/// **process's** commit charge as the arena would be a real number, from the right process,
/// describing the wrong allocator. That one would be a lie. Zero is not.
pub(super) fn mallinfo(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let out = c.args().indirect_result();
    // Through `GuestMem`, so the eighty bytes go past the same admission rules every other guest
    // write does and a hostile `X8` is the boundary's typed refusal rather than a host fault.
    c.mem().write_bytes(
        out,
        &[0u8; MALLINFO_BYTES],
        crate::mem::Blame::new(c.symbol(), c.address(), 0),
    )?;
    // AAPCS64 passes the indirect result location in `X8` and the callee writes through it; the
    // value left in `X0` is not what a caller reads -- all three sites here load the field
    // straight out of their own stack slot. It is set to the same address because that is what
    // bionic leaves there, and a register this layer declines to set is a register that differs
    // from the device for no reason.
    c.ret().u64(out as u64);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guest's constants are **Linux's**, and this is the test that says so. A `MAP_ANONYMOUS`
    /// taken from a BSD or Darwin header is `0x1000`, and every anonymous mapping would then look
    /// file-backed and be refused.
    #[test]
    fn the_mmap_constants_are_the_linux_ones() {
        assert_eq!(MAP_SHARED, 0x01);
        assert_eq!(MAP_PRIVATE, 0x02);
        assert_eq!(MAP_FIXED, 0x10);
        assert_eq!(MAP_ANONYMOUS, 0x20, "0x1000 is the BSD value and would be silently wrong");
        assert_eq!(MAP_NORESERVE, 0x4000);
        assert_eq!(MAP_STACK, 0x20000);
        assert_eq!(MAP_FIXED_NOREPLACE, 0x100000);
        assert_eq!(MAP_FAILED, u64::MAX, "(void *) -1, and it is not NULL");
        assert_eq!(PROT_READ | PROT_WRITE, 3);
        assert_eq!(MADV_DONTNEED, 4);
        assert_eq!(MADV_FREE, 8);
    }

    /// The four expressible protections, and the three that are refused rather than widened.
    #[test]
    fn a_protection_with_no_expressible_state_is_refused_rather_than_widened() {
        assert_eq!(protection_for(PROT_NONE), Ok(Protection::None));
        assert_eq!(protection_for(PROT_READ), Ok(Protection::Read));
        assert_eq!(protection_for(PROT_READ | PROT_WRITE), Ok(Protection::ReadWrite));
        assert_eq!(protection_for(PROT_READ | PROT_EXEC), Ok(Protection::ReadExecute));
        for prot in [PROT_WRITE, PROT_EXEC, PROT_WRITE | PROT_EXEC, PROT_READ | PROT_WRITE | PROT_EXEC] {
            let why = protection_for(prot).expect_err("must refuse");
            assert!(why.contains(&format!("{prot:#x}")), "{why}");
        }
        // A bit nobody defined is refused as well, with its own reason.
        let why = protection_for(0x40).expect_err("must refuse");
        assert!(why.contains("outside PROT_READ"), "{why}");
    }

    /// Rounding a hostile length up to a page must not wrap it into a small one, which would turn
    /// a refusal into a mapping.
    #[test]
    fn rounding_a_length_up_to_a_page_cannot_wrap() {
        let page = 4096usize;
        assert_eq!(pages(1, page), Some(4096));
        assert_eq!(pages(4096, page), Some(4096));
        assert_eq!(pages(4097, page), Some(8192));
        assert_eq!(pages(0, page), Some(0));
        assert_eq!(pages(u64::MAX, page), None, "a wrap must be None, never a small length");
        assert_eq!(pages(usize::MAX as u64 - 1, page), None);
    }

    /// The advisory set must not contain either of the two that change what a read returns, or
    /// ignoring it would be a silent wrong answer.
    #[test]
    fn the_advisory_set_excludes_every_advice_that_changes_the_contents() {
        assert!(!ADVISORY.contains(&MADV_DONTNEED));
        assert!(!ADVISORY.contains(&MADV_FREE));
        assert!(!ADVISORY.contains(&MADV_REMOVE));
        assert_eq!(ADVISORY.len(), 16);
    }

    /// The known-flag mask is the union of the flags the handler branches on, and nothing else.
    /// A flag added to the mask without a branch would be silently ignored, which is the failure
    /// the mask exists to prevent.
    #[test]
    fn the_known_flag_mask_is_exactly_the_flags_that_are_handled() {
        assert_eq!(
            KNOWN_MAP_FLAGS,
            MAP_SHARED
                | MAP_PRIVATE
                | MAP_FIXED
                | MAP_ANONYMOUS
                | MAP_NORESERVE
                | MAP_POPULATE
                | MAP_STACK
                | MAP_FIXED_NOREPLACE
        );
        assert_eq!(KNOWN_MAP_FLAGS & 0x0100, 0, "MAP_GROWSDOWN is not implemented");
        assert_eq!(KNOWN_MAP_FLAGS & 0x40000, 0, "MAP_HUGETLB is not implemented");
    }
}
