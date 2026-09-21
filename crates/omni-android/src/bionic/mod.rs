//! The adapter: `omni-bionic`'s functions bound to the thunk boundary.
//!
//! D19 keeps `omni-bionic` a separate crate with **zero dependencies**, so that "no OS access"
//! is a fact `cargo tree` can check rather than a rule a reviewer has to notice. The price of
//! that is this module: the crate states what it needs as traits — guest memory, a 32-bit
//! compare-and-swap, a thread identity, a futex, a clock, a place to put `errno` — and something
//! has to supply them. D19 says where: "the adapter binds bionic's functions to the thunk
//! boundary and needs both, so it belongs in `omni-android`".
//!
//! # What a handler is, and why it needs a thread-local
//!
//! [`ImportFn`] is a bare `fn` pointer. It has no captured state and [`ImportCall`] carries no
//! user data, so a handler cannot be handed the mutex owner table or the TLS registry through
//! its arguments. A process-wide `static` would be wrong rather than merely ugly: this runtime
//! is designed to host **three concurrent guest instances** (see the memory figures in
//! `STATUS.md`), and one static would give them one shared `pthread_key` table.
//!
//! So the state is per-instance, in [`Bionic`], and a caller holds an [`Activation`] across
//! [`Boundary::run`](crate::Boundary::run) that publishes it to the calling thread. A guest
//! thread runs one instance at a time, which makes a thread-local exactly the right scope. A
//! handler that finds none refuses with [`AbiError::BionicNotActive`] naming the symbol — it
//! does **not** construct a default, because a per-call default state would give two guest
//! threads their own private copy of the same mutex, and two threads that each believe they
//! hold it is the failure no later test can see.
//!
//! # The per-thread block, and why it is mapped before any guest code runs
//!
//! `errno` is per-thread storage the *guest* must be able to read, because `__errno()` returns a
//! pointer to it. `strerror` likewise returns a pointer to a per-thread buffer. Both come out of
//! one small arena this module maps at construction time, one [`view::THREAD_BLOCK_BYTES`] block
//! per guest thread.
//!
//! **Mapped in [`Bionic::new`], deliberately, and this is F9's constraint.** Task 2's review
//! found that `ImportCall::mem()` reaches the whole [`GuestSpace`], so an inline handler *could*
//! map and unmap while generated code is live — which the pager's "the thread running guest code
//! must not hold this space's lock" invariant does not allow. Nothing in this phase maps from a
//! handler. The one mapping this module performs happens before any CPU exists, let alone runs.

mod clocks;
mod data;
mod dl;
mod files;
mod format;
mod guestmem;
mod handlers;
mod logging;
mod procenv;
mod runtime;
mod signals;
mod stdio;
mod threads;
mod view;

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use omni_bionic::atexit::AtexitRegistry;
use omni_bionic::cond::CondWaiters;
use omni_bionic::metadata::NameRegistry;
use omni_bionic::mutex::OwnerTable;
use omni_bionic::threads::GuestThreadId;
use omni_bionic::tls::TlsRegistry;
use omni_elf::loader::DlPhdrInfo;
use omni_bionic::stdio::Stream;
use omni_mem::{CommitPolicy, GuestAddr, GuestSpace, Placement, Protection};
use omni_platform::fs::Filesystem;
use parking_lot::{Condvar, Mutex};

use crate::boundary::{BoundaryBuilder, ImportCall, ImportFn, ReentrantFn};
use threads::JoinOutcome;
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

pub use data::{DataObject, GuestProcess, DATA_OBJECTS, FILE_BYTES};
pub use files::{
    errno_for, DIRENT_BYTES, STATVFS_BYTES, STAT_BYTES, S_IFCHR, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG,
};
pub use logging::LogRecord;
pub use omni_platform::log::Priority as LogPriority;
pub use procenv::{HwcapPolicy, HWCAP_ATOMICS, PROP_VALUE_MAX};
pub use runtime::{AddressFutex, CallThreads, HostClock, HostYield, ThreadSlot, ThreadTable};
pub use threads::{
    GuestThreadFailure, GuestThreadState, ThreadHost, DEFAULT_GUEST_STACK_BYTES,
    GUEST_THREAD_STEP_WINDOW, MIN_GUEST_STACK_BYTES, SCHED_OTHER,
};
pub use view::{
    GuestView, DL_INFO_OFFSET, DL_INFO_SLOTS, DL_PHDR_INFO_BYTES, ERRNO_OFFSET, SCRATCH_BYTES,
    SCRATCH_OFFSET, THREAD_BLOCK_BYTES,
};

/// How many guest threads one instance can give a block to.
///
/// **A policy number, and stated as one.** This phase binds no thread lifecycle at all —
/// `pthread_create` is a later phase — so the guest has exactly the threads the host started for
/// it, which is a handful. 64 blocks cost [`ARENA_BYTES`] of address space and one commit
/// granule, and a 65th thread is a refusal naming the symbol rather than a second thread
/// writing the first one's `errno`.
pub const MAX_GUEST_THREADS: usize = 64;

/// Bytes of the arena set aside for objects that outlive a call and belong to no thread.
///
/// The `AMEDIAFORMAT_KEY_*` strings, `environ`'s empty vector, and one copy of each loaded
/// image's `dlpi_name`. All of it is written **before guest code runs** — by
/// [`Bionic::declare_data_into`] and [`Bionic::register_image`], both of which a host calls while
/// setting up — so nothing here maps or writes from inside a handler. That is F9's constraint,
/// and it is the same reason [`Bionic::new`] maps the arena rather than a handler doing it.
///
/// One page. The ten media keys are 104 bytes with their terminators, `environ`'s vector is eight,
/// and eleven library paths at `PATH_MAX`-ish lengths are the rest. A pool that fills is a
/// refusal naming the symbol, never a silent overwrite.
pub const POOL_BYTES: usize = 4096;

/// How many `FILE` objects one instance can hand out beyond the three standard streams.
///
/// **A policy number, and it is the ONE that the commit granule chose rather than a preference.**
/// Each costs [`FILE_BYTES`] of the arena and one entry in the stream table, and the three
/// standard streams do not come out of it — they live in `__sF`, which the boundary already
/// placed. A `fopen` past this is `EMFILE`, which is what a real device reports when a process
/// runs out of streams and one every correct caller already branches on.
///
/// **It was 32 and the granule test refused it.** [`ARENA_BYTES`] came to **67,712** bytes with
/// 32 slots, against `omni_mem::DEFAULT_COMMIT_GRANULE` — the granule D10 *measured*, which this
/// crate references rather than restates. Over it by 2,176 bytes, so [`Bionic::new`]'s eager
/// commit would silently have cost a **second** granule per instance and the comment justifying
/// that exception to D10 ("never commit speculatively") would have been false. Nothing else would
/// have noticed: the charge is real but small, and no test measured it. Sixteen slots gives
/// 64 × 848 + 4096 + 16 × 152 + 16 × 280 = **65,280**. The relation is pinned by
/// `the_arena_fits_in_one_commit_granule`, which compares against the constant rather than a
/// literal, so a re-measured granule moves this with it.
///
/// It is also smaller than [`omni_platform::fs::MAX_OPEN_FILES`] on purpose: a descriptor is
/// cheaper than a stream, and a guest holding sixteen streams open during static initialisation
/// is doing something this layer wants to hear about.
pub const MAX_GUEST_FILES: usize = 16;

/// How many directory streams one instance can hand out.
///
/// Matched to [`omni_platform::fs::MAX_OPEN_DIRS`] so the two ceilings cannot disagree: a larger
/// arena would hand out a slot the seam then refused, and a smaller one would leave the seam's own
/// ceiling unreachable and untested.
pub const MAX_GUEST_DIRS: usize = omni_platform::fs::MAX_OPEN_DIRS;

/// A stream needs a descriptor, so the stream table must not outrun the descriptor table.
///
/// A **compile-time** assertion rather than a test, because both sides are constants: a build
/// that violated it could not produce a binary to run a test with. It was written as a test
/// first, and clippy pointed out that `assert!(16 < 64)` is folded away — which is the lint
/// being right about where the check belongs rather than about whether to make it.
const _: () = assert!(MAX_GUEST_FILES < omni_platform::fs::MAX_OPEN_FILES);

/// Bytes of guest address space the arena occupies.
///
/// The per-thread blocks, the pool, the `FILE` objects `fopen` hands out, and one `struct dirent`
/// per open directory stream. **All of it is mapped once, in [`Bionic::new`]**, which is F9's
/// constraint: a handler may not map guest memory, and `fopen` and `opendir` are handlers.
pub const ARENA_BYTES: usize = MAX_GUEST_THREADS * THREAD_BLOCK_BYTES
    + POOL_BYTES
    + MAX_GUEST_FILES * FILE_BYTES
    + MAX_GUEST_DIRS * DIRENT_BYTES;

/// The longest a single guest `nanosleep` or `usleep` may block a host thread.
///
/// **A policy number, and a hostile-input defence rather than a semantics change.** The duration
/// is a value the guest chose, and a sleeping thread executes no guest instructions — so D16's
/// runaway-guest defence, which is built from short step budgets, cannot end one. `nanosleep({
/// INT64_MAX, 0 })` is otherwise a permanent hang of that host thread.
///
/// Sixty seconds is far longer than anything the 3,594 initializers can legitimately want and far
/// shorter than a hang. A request past it is a refusal naming both numbers, never a clamp: a clamp
/// would return success from a call that slept for a minute when it was asked for a year.
pub const MAX_SLEEP_SECONDS: u64 = 60;

/// How many log records one instance keeps for inspection.
///
/// **A policy number.** How much the engine logs during initialisation has not been measured, and
/// an unbounded ring is a host allocation a guest can drive in a loop. Records past the cap are
/// dropped oldest-first and counted, so [`Bionic::log_dropped`] can say the ring wrapped rather
/// than the ring quietly pretending it did not.
pub const LOG_CAPTURE_MAX: usize = 256;

/// One guest instance's bionic state.
///
/// Everything here is state a *process* has exactly one of: the mutex owner table, the
/// `pthread_key` table, the atexit list, the `rand` sequence. Sharing any of them between two
/// guest instances would be the bug; per-call defaults would be a different one.
pub struct Bionic {
    /// The address space, kept so the arena can be given back.
    space: Arc<GuestSpace>,
    /// First address of the per-thread arena.
    arena: GuestAddr,
    /// Who is attached, and which block each one has.
    threads: ThreadTable,
    /// Blocking, keyed by guest address.
    futex: AddressFutex,
    /// The monotonic and wall clocks the timed waits measure against.
    clock: HostClock,
    /// `sched_yield`.
    yielder: HostYield,
    /// `pthread_key_create` / `getspecific` / `setspecific`, and `__cxa_thread_atexit_impl`.
    tls: TlsRegistry,
    /// Who owns which mutex. An `Arc` because `omni_bionic::cond` installs it as ambient state
    /// for the duration of a `pthread_cond_wait`, which is how the relock on the way out finds
    /// the same table the lock on the way in used.
    owners: Arc<OwnerTable>,
    /// Registered `pthread_cond_wait` waiters.
    conds: CondWaiters,
    /// `pthread_setname_np`.
    names: NameRegistry,
    /// `__cxa_atexit`.
    atexit: AtexitRegistry,
    /// The `rand` sequence's state. Process-wide, as C says it is.
    rand: AtomicU32,
    /// The bump allocator for [`POOL_BYTES`], and what has been handed out of it.
    pool: Mutex<usize>,
    /// Every loaded image `dl_iterate_phdr` must enumerate, in the order it was registered.
    images: Mutex<Vec<GuestImage>>,

    // ---------------------------------------------------------------- phase 3a: the OS surface
    /// `"UTC"` in the pool, for `gmtime_r`'s `tm_zone`. Interned once in [`Bionic::new`].
    utc_zone: OnceLock<GuestAddr>,
    /// The guest's environment: the name, and the value interned in the pool.
    ///
    /// **Empty by default and that is a fact, not a gap** — this guest process was started with no
    /// environment, which is what the `environ` data object already says. The host fills it with
    /// [`Bionic::set_env`]. It is never the *host's* environment: see `procenv`'s module docs.
    env: Mutex<Vec<(Vec<u8>, GuestAddr)>>,
    /// The Android property table `__system_property_get` reads, empty by default for the same
    /// reason: there is no property service here.
    properties: Mutex<Vec<(Vec<u8>, String)>>,
    /// What `getauxval(AT_HWCAP)` should answer. **[`HwcapPolicy::Undecided`] by default, and
    /// that default refuses** — the decision is open; see `procenv`'s module documentation.
    hwcap: Mutex<HwcapPolicy>,
    /// The guest's last `android_set_abort_message`, reported with the abort it explains.
    abort_message: Mutex<Option<String>>,
    /// `openlog`'s ident, which tags later `syslog` lines.
    syslog_ident: Mutex<Option<String>>,
    /// The last [`LOG_CAPTURE_MAX`] records, for inspection.
    log_ring: Mutex<VecDeque<LogRecord>>,
    /// How many records the ring dropped, so a wrap is visible rather than silent.
    log_dropped: AtomicU64,
    /// Whether records also reach the host's standard error. On by default: that is what a real
    /// run wants, and a suite that does not want it says so.
    log_to_stderr: AtomicBool,

    // ---------------------------------------------------------------- phase 3b: files
    /// The guest's filesystem: one host directory every guest path is resolved inside.
    ///
    /// **`None` until the embedding names one, and that default is load-bearing.** A guest with no
    /// root has no filesystem at all and every path-taking symbol refuses by name. A default would
    /// have to be *somewhere* — the process's working directory, or a temporary one — and either
    /// would let untrusted guest code read and write host files nobody decided to expose. See
    /// [`Bionic::set_filesystem_root`] and `files`' module documentation.
    ///
    /// A `OnceLock` rather than a `Mutex<Option<..>>`: the root may be set once and never moved.
    /// Changing it while the guest runs would let a descriptor opened under one root be read under
    /// another, which is a confinement hole with a legitimate-looking API in front of it.
    fs: OnceLock<Filesystem>,
    /// The `st_dev` every file on that root reports. Derived once, from the root's own path.
    fs_device: OnceLock<u64>,
    /// The open `FILE *` streams, keyed by the guest address the object was placed at.
    ///
    /// **The guest's `FILE` bytes are never read**, which is what keeps the unverified
    /// [`FILE_BYTES`] from being able to produce a wrong answer; `stdio`'s module documentation
    /// has the whole argument.
    streams: Mutex<BTreeMap<GuestAddr, Stream>>,
    /// The open directory streams: the guest `DIR *`, and the seam's handle behind it.
    dirs: Mutex<BTreeMap<GuestAddr, i32>>,

    // ---------------------------------------------------------- phase 3c: thread lifecycle
    /// What makes a guest thread's CPU context, and the policy around it.
    ///
    /// **`None` until the embedding supplies one, and no default is possible.** The adapter
    /// cannot invent a `GuestCpuBackend`, and a backend is the only thing that can give a new
    /// guest thread the bionic TLS block D13 requires before it runs one instruction. An
    /// instance without one refuses `pthread_create` by name, naming
    /// [`Bionic::set_thread_host`].
    ///
    /// A `OnceLock` for the same reason the filesystem root is one: a backend that changed
    /// under a running guest thread would leave that thread's context orphaned from the arena
    /// its TLS block came from.
    thread_host: OnceLock<ThreadHost>,
    /// Every guest thread `pthread_create` started and has not yet reaped.
    guest_threads: Mutex<GuestThreads>,
    /// Signalled when a guest thread finishes, which is what `pthread_join` waits on.
    threads_done: Condvar,
    /// Guest threads that stopped without returning, oldest first.
    thread_failures: Mutex<Vec<GuestThreadFailure>>,
    /// The cooperative stop switch every created thread checks between run windows.
    ///
    /// D16's shape: a watchdog over a guest that never returns is built from short budget
    /// windows, because the halt flag is checked at terminals a counted budget makes exclusive.
    threads_stopping: AtomicBool,
}

/// The live guest threads, and who is waiting for whom.
///
/// One structure under one lock, because the deadlock check reads both: `records` says which
/// threads exist and `waits` says which of them is blocked on which, and a check that read them
/// under two locks could see a cycle that had already been broken, or miss one that had just
/// formed.
#[derive(Default)]
struct GuestThreads {
    /// `pthread_t` to what is known about it.
    records: BTreeMap<GuestThreadId, threads::ThreadRecord>,
    /// Joiner to the thread it is inside `pthread_join` on.
    ///
    /// The **only** source of truth for both questions a join has to answer: "is somebody
    /// already joining this thread" is a search of the values, and "would this join deadlock" is
    /// a walk of the chain. Keeping a second flag on the record beside it would be two places to
    /// update and one of them would eventually be missed.
    waits: BTreeMap<GuestThreadId, GuestThreadId>,
}

/// One loaded image, as `dl_iterate_phdr` reports it.
///
/// The guest-side form of [`DlPhdrInfo`]: the name has been copied into the pool and is a guest
/// address, because a callback receives `dlpi_name` as a `const char *` and dereferences it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestImage {
    /// `dlpi_addr`: the load bias.
    pub addr: GuestAddr,
    /// `dlpi_name`: a guest pointer to a NUL-terminated copy of the name, in the pool.
    pub name: GuestAddr,
    /// `dlpi_phdr`: the guest address of the program header table.
    pub phdr: GuestAddr,
    /// `dlpi_phnum`.
    pub phnum: u16,
}

impl core::fmt::Debug for Bionic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bionic")
            .field("arena", &format_args!("{:#x}", self.arena))
            .field("threads", &self.threads.live())
            .finish_non_exhaustive()
    }
}

impl Bionic {
    /// Map the per-thread arena and start with nothing attached.
    ///
    /// # Errors
    ///
    /// [`AbiError::Memory`] if the arena could not be mapped.
    pub fn new(space: Arc<GuestSpace>) -> AbiResult<Arc<Self>> {
        // Eager rather than lazy, and the exception to D10's "never commit speculatively" is
        // stated rather than assumed: the arena is [`ARENA_BYTES`], which is under one commit
        // granule (`omni_mem::DEFAULT_COMMIT_GRANULE`, a figure D10 measured rather than chose),
        // so lazy and eager cost exactly the same commit charge here. Eager
        // buys that the first `errno` write on a new thread cannot fail for a commit reason
        // inside a handler, where there is no good way to retry.
        //
        // `the_arena_fits_in_one_commit_granule` asserts the "under one granule" half, because
        // that is the part which stops being true when a phase adds a table. Phase 3b added two --
        // the `FILE` objects and the `struct dirent` slots -- taking the arena from **58,368**
        // bytes to **65,280**.
        //
        // The number this comment used to give was "17 KiB", which was right when D20 wrote it
        // (64 blocks x 272 bytes = 17,408) and had been wrong since **phase 2**, which widened the
        // per-thread block to 848 bytes for the `dl_phdr_info` slots and took the arena to 58,368
        // without anyone updating the sentence. A figure in a comment that nothing asserts is a
        // figure that drifts; the test is why this one now cannot.
        let arena = space.map_anonymous(
            Placement::Anywhere { align: space.page_size() },
            ARENA_BYTES,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )?;
        let bionic = Arc::new(Self {
            space,
            arena,
            threads: ThreadTable::new(),
            futex: AddressFutex::new(),
            clock: HostClock::new(),
            yielder: HostYield,
            tls: TlsRegistry::new(),
            owners: Arc::new(OwnerTable::new()),
            conds: CondWaiters::new(),
            names: NameRegistry::new(),
            atexit: AtexitRegistry::new(),
            // Seeded as C's `rand` is before any `srand`: the standard says the sequence is as
            // if `srand(1)` had been called.
            rand: AtomicU32::new(1),
            pool: Mutex::new(0),
            images: Mutex::new(Vec::new()),
            utc_zone: OnceLock::new(),
            env: Mutex::new(Vec::new()),
            properties: Mutex::new(Vec::new()),
            // Spelled out rather than reached by `Default`, because this is the open `AT_HWCAP`
            // decision and it must not be made by a derive nobody read. See `procenv`.
            hwcap: Mutex::new(HwcapPolicy::Undecided),
            abort_message: Mutex::new(None),
            syslog_ident: Mutex::new(None),
            log_ring: Mutex::new(VecDeque::new()),
            log_dropped: AtomicU64::new(0),
            log_to_stderr: AtomicBool::new(true),
            fs: OnceLock::new(),
            fs_device: OnceLock::new(),
            streams: Mutex::new(BTreeMap::new()),
            dirs: Mutex::new(BTreeMap::new()),
            thread_host: OnceLock::new(),
            guest_threads: Mutex::new(GuestThreads::default()),
            threads_done: Condvar::new(),
            thread_failures: Mutex::new(Vec::new()),
            threads_stopping: AtomicBool::new(false),
        });
        // `gmtime_r`'s `tm_zone` is a `const char *` the guest dereferences, so it has to point at
        // something for the whole life of the instance. Interned here, before any guest code runs
        // — which is F9's constraint: nothing in a handler may map, and the pool is already mapped
        // by the time a handler could reach it.
        let utc = bionic.intern("gmtime_r", b"UTC")?;
        let _ = bionic.utc_zone.set(utc);
        Ok(bionic)
    }

    /// `"UTC"` in the pool, which `gmtime_r` writes into `tm_zone`.
    ///
    /// Always set: [`Bionic::new`] interns it before returning, so there is no lazy path here and
    /// no way for a handler to have to allocate.
    #[must_use]
    pub fn utc_zone(&self) -> GuestAddr {
        // Unreachable: `new` sets it and nothing clears it. Falling back to the pool's base rather
        // than panicking, because a panic in a handler is reachable from guest code.
        *self.utc_zone.get().unwrap_or(&self.arena)
    }

    /// The page size the guest's own address space works at, which is what `AT_PAGESZ` answers.
    #[must_use]
    pub fn space_page_size(&self) -> usize {
        self.space.page_size()
    }

    // ------------------------------------------------------------------ environment and properties

    /// Give the guest an environment variable, visible to `getenv`.
    ///
    /// The **value** is copied into the adapter's pool, because `getenv` returns a pointer the
    /// caller may hold indefinitely. Setting a name twice replaces the entry and leaves the old
    /// value's pool bytes stranded — call this during setup, not in a loop.
    ///
    /// Call it **before any guest code runs**: it writes to the pool, and F9's constraint is that
    /// nothing maps or allocates guest memory from inside a handler.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if the name is empty or contains `=` — neither can name a variable —
    /// or if the pool is full.
    pub fn set_env(&self, name: &str, value: &str) -> AbiResult<()> {
        if name.is_empty() || name.contains('=') {
            return Err(AbiError::Refused {
                symbol: "getenv".to_string(),
                address: self.pool(),
                why: format!(
                    "`{name}` is not a usable environment variable name: an empty name names \
                     nothing, and `=` is the separator, so neither could ever be found again"
                ),
            });
        }
        let at = self.intern("getenv", value.as_bytes())?;
        let mut env = self.env.lock();
        let key = name.as_bytes().to_vec();
        match env.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = at,
            None => env.push((key, at)),
        }
        Ok(())
    }

    /// The pooled value for `name`, or `None`.
    pub(crate) fn lookup_env(&self, name: &[u8]) -> Option<GuestAddr> {
        self.env.lock().iter().find(|(existing, _)| existing == name).map(|(_, at)| *at)
    }

    /// Give the guest an Android system property, visible to `__system_property_get`.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] for a value that does not fit [`PROP_VALUE_MAX`] with its NUL. The
    /// refusal is here rather than a truncation at read time, because the guest sizes its buffer
    /// from that same constant and a truncated property value is a believable wrong answer.
    pub fn set_system_property(&self, name: &str, value: &str) -> AbiResult<()> {
        if value.len() + 1 > PROP_VALUE_MAX {
            return Err(AbiError::Refused {
                symbol: "__system_property_get".to_string(),
                address: self.pool(),
                why: format!(
                    "the property `{name}` was given a {}-byte value, and bionic's PROP_VALUE_MAX \
                     is {PROP_VALUE_MAX} bytes including the NUL. The guest sizes its own buffer \
                     from that constant, so the choice here is refusing or truncating, and a \
                     truncated property value is a believable wrong answer",
                    value.len() + 1
                ),
            });
        }
        let mut properties = self.properties.lock();
        let key = name.as_bytes().to_vec();
        match properties.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = value.to_string(),
            None => properties.push((key, value.to_string())),
        }
        Ok(())
    }

    /// The property value for `name`, or `None`.
    pub(crate) fn lookup_property(&self, name: &[u8]) -> Option<String> {
        self.properties.lock().iter().find(|(existing, _)| existing == name).map(|(_, v)| v.clone())
    }

    // ------------------------------------------------------------------ the AT_HWCAP decision

    /// What `getauxval(AT_HWCAP)` will answer.
    ///
    /// [`HwcapPolicy::Undecided`] until a host says otherwise, and under that the call **refuses**.
    #[must_use]
    pub fn hwcap_policy(&self) -> HwcapPolicy {
        *self.hwcap.lock()
    }

    /// State what `getauxval(AT_HWCAP)` should answer.
    ///
    /// **This is an open decision for M3 and both arms are measured** — advertising
    /// [`HWCAP_ATOMICS`] gives 53 hard interpreter halts, declining gives 106 fallback arms into a
    /// global spinlock that anti-scales 21x. `procenv`'s module documentation has the whole
    /// argument. A host calling this is making that choice explicitly, which is the only way it
    /// may be made.
    pub fn set_hwcap_policy(&self, policy: HwcapPolicy) {
        *self.hwcap.lock() = policy;
    }

    // ------------------------------------------------------------------ abort and logging

    /// The guest's last `android_set_abort_message`, if it set one.
    #[must_use]
    pub fn abort_message(&self) -> Option<String> {
        self.abort_message.lock().clone()
    }

    /// Set or clear the abort message.
    pub fn set_abort_message(&self, message: Option<String>) {
        *self.abort_message.lock() = message;
    }

    /// `openlog`'s ident, if one is set.
    #[must_use]
    pub fn syslog_ident(&self) -> Option<String> {
        self.syslog_ident.lock().clone()
    }

    /// Set or clear `openlog`'s ident.
    pub fn set_syslog_ident(&self, ident: Option<String>) {
        *self.syslog_ident.lock() = ident;
    }

    /// Record one log line, and emit it if this instance is emitting.
    pub(crate) fn log(&self, record: LogRecord) {
        if self.log_to_stderr.load(Ordering::Relaxed) {
            logging::emit(&record);
        }
        let mut ring = self.log_ring.lock();
        if ring.len() == LOG_CAPTURE_MAX {
            ring.pop_front();
            self.log_dropped.fetch_add(1, Ordering::Relaxed);
        }
        ring.push_back(record);
    }

    /// Every log record still in the ring, oldest first.
    #[must_use]
    pub fn log_records(&self) -> Vec<LogRecord> {
        self.log_ring.lock().iter().cloned().collect()
    }

    /// How many records the ring has dropped, so a wrap is visible.
    #[must_use]
    pub fn log_dropped(&self) -> u64 {
        self.log_dropped.load(Ordering::Relaxed)
    }

    /// Whether log records also reach the host's standard error. On by default.
    pub fn set_log_to_stderr(&self, enabled: bool) {
        self.log_to_stderr.store(enabled, Ordering::Relaxed);
    }

    /// First address of the static pool.
    #[must_use]
    pub fn pool(&self) -> GuestAddr {
        self.arena + MAX_GUEST_THREADS * THREAD_BLOCK_BYTES
    }

    // ------------------------------------------------------------------ phase 3b: files

    /// Give this guest instance a filesystem, confined to `root`.
    ///
    /// **Every guest path — `/data/...`, `/system/...`, `/proc/...` — is resolved inside this
    /// directory**, by rules applied before any host call is made; `omni_platform::fs::path` has
    /// the policy and enumerates the hostile cases. Until this is called the instance has no
    /// filesystem and every path-taking guest symbol refuses by name, naming this method.
    ///
    /// There is deliberately no default and no way to unset it. A default root would have to be
    /// the process's working directory or a temporary one, and either would hand untrusted guest
    /// code host files nobody decided to expose; allowing it to *change* would let a descriptor
    /// opened under one root be read under another.
    ///
    /// Call it during setup. It maps nothing and writes no guest memory, so it is not bound by
    /// F9's constraint — but the guest refuses every file call made before it.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if `root` does not exist, is not a directory, or if this instance
    /// already has one.
    pub fn set_filesystem_root(&self, root: impl AsRef<Path>) -> AbiResult<()> {
        let filesystem = Filesystem::new(root.as_ref()).map_err(|error| AbiError::Refused {
            symbol: "open".to_string(),
            address: self.arena,
            why: error.to_string(),
        })?;
        let device = omni_platform::fs::identity(filesystem.root());
        if self.fs.set(filesystem).is_err() {
            return Err(AbiError::Refused {
                symbol: "open".to_string(),
                address: self.arena,
                why: format!(
                    "this guest instance already has a filesystem root (`{}`). It may be set once \
                     and never moved: a descriptor opened under one root must not become readable \
                     under another",
                    self.fs.get().map_or_else(String::new, |fs| fs.root().display().to_string())
                ),
            });
        }
        let _ = self.fs_device.set(device);
        Ok(())
    }

    /// The guest's filesystem, or `None` if the embedding has not supplied a root.
    #[must_use]
    pub fn filesystem(&self) -> Option<&Filesystem> {
        self.fs.get()
    }

    /// The `st_dev` every file on this instance's root reports.
    ///
    /// One number per root, non-zero, derived from the root's own path — so two instances with two
    /// roots report two devices, which is what "these are different filesystems" means to the
    /// `(st_dev, st_ino)` identity test guest code makes.
    #[must_use]
    pub fn filesystem_device(&self) -> u64 {
        *self.fs_device.get().unwrap_or(&1)
    }

    /// First address of the `FILE` object table.
    #[must_use]
    pub fn files_base(&self) -> GuestAddr {
        self.pool() + POOL_BYTES
    }

    /// First address of the `struct dirent` table.
    #[must_use]
    pub fn dirents_base(&self) -> GuestAddr {
        self.files_base() + MAX_GUEST_FILES * FILE_BYTES
    }

    /// Register a stream at a guest address the boundary already placed — the three `__sF` ones.
    pub(crate) fn register_stream(&self, at: GuestAddr, fd: i32) {
        self.streams.lock().insert(at, Stream::new(fd));
    }

    /// Hand out a `FILE` object for `fd`, or `None` when the table is full.
    ///
    /// The object's bytes are zeroed, and that is the only time they are ever written. A zeroed
    /// bionic `FILE` has `_flags == 0`, which that library's own `__sfp` calls a free slot — so the
    /// bytes say "not an open stream", which is true and safe, rather than describing one.
    ///
    /// The arena is already mapped, in [`Bionic::new`], so this writes into memory this process
    /// owns rather than mapping any: F9's constraint is that a handler may not map, and `fopen` is
    /// a handler.
    pub(crate) fn open_stream(
        &self,
        view: &view::GuestView<'_>,
        fd: i32,
    ) -> AbiResult<Option<GuestAddr>> {
        let mut streams = self.streams.lock();
        let base = self.files_base();
        let free = (0..MAX_GUEST_FILES)
            .map(|index| base + index * FILE_BYTES)
            .find(|at| !streams.contains_key(at));
        let Some(at) = free else {
            return Ok(None);
        };
        view.mem().write_bytes(
            at,
            &[0u8; FILE_BYTES],
            Blame::new(view.symbol(), view.address(), 0),
        )?;
        streams.insert(at, Stream::new(fd));
        Ok(Some(at))
    }

    /// The stream a guest `FILE *` names.
    #[must_use]
    pub fn stream_of(&self, file: u64) -> Option<Stream> {
        GuestAddr::try_from(file).ok().and_then(|at| self.streams.lock().get(&at).copied())
    }

    /// Store a stream's flags back after an operation changed them.
    ///
    /// **The step that is easy to forget.** `omni_bionic::stdio` takes a `&mut Stream`, and a
    /// handler that dropped the mutated copy would leave `feof` answering false forever after an
    /// end of file.
    pub(crate) fn update_stream(&self, file: u64, stream: Stream) {
        if let Ok(at) = GuestAddr::try_from(file) {
            if let Some(slot) = self.streams.lock().get_mut(&at) {
                *slot = stream;
            }
        }
    }

    /// Release a `FILE` object's slot.
    pub(crate) fn close_stream(&self, file: u64) -> Option<Stream> {
        GuestAddr::try_from(file).ok().and_then(|at| self.streams.lock().remove(&at))
    }

    /// Every open stream's guest `FILE *`, which is what `fflush(NULL)` walks.
    #[must_use]
    pub fn stream_pointers(&self) -> Vec<u64> {
        self.streams.lock().keys().map(|at| *at as u64).collect()
    }

    /// How many streams this instance holds open.
    #[must_use]
    pub fn open_streams(&self) -> usize {
        self.streams.lock().len()
    }

    /// Give a directory stream a guest `DIR *`: the address of its own `struct dirent` slot.
    ///
    /// **The `DIR *` IS the slot**, which makes `readdir`'s contract — "the returned pointer stays
    /// valid until the next call on this `DIR`" — true by construction rather than by bookkeeping,
    /// and makes two streams over one directory structurally unable to overwrite each other's
    /// entry.
    pub(crate) fn attach_dir(&self, id: i32) -> Option<GuestAddr> {
        let mut dirs = self.dirs.lock();
        let base = self.dirents_base();
        let free = (0..MAX_GUEST_DIRS)
            .map(|index| base + index * DIRENT_BYTES)
            .find(|at| !dirs.contains_key(at))?;
        dirs.insert(free, id);
        Some(free)
    }

    /// The seam handle a guest `DIR *` names.
    #[must_use]
    pub fn dir_for(&self, dirp: u64) -> Option<i32> {
        GuestAddr::try_from(dirp).ok().and_then(|at| self.dirs.lock().get(&at).copied())
    }

    /// Release a directory stream's slot.
    pub(crate) fn detach_dir(&self, dirp: u64) -> Option<i32> {
        GuestAddr::try_from(dirp).ok().and_then(|at| self.dirs.lock().remove(&at))
    }

    /// How many directory streams this instance holds open.
    #[must_use]
    pub fn open_dirs(&self) -> usize {
        self.dirs.lock().len()
    }

    /// Take `len` zeroed, 8-byte-aligned bytes out of the pool.
    ///
    /// Called from host setup, never from a handler: see [`POOL_BYTES`].
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the pool is full, naming `symbol` — a refusal rather than a
    /// wrapped bump pointer overwriting somebody else's object.
    pub fn reserve(&self, symbol: &str, len: usize) -> AbiResult<GuestAddr> {
        let mut used = self.pool.lock();
        // Eight-byte aligned, because the pool holds `environ`'s vector of pointers as well as
        // strings, and a misaligned pointer array is not something to hand a guest.
        let start = (*used + 7) & !7;
        if len == 0 || start.saturating_add(len) > POOL_BYTES {
            return Err(AbiError::Refused {
                symbol: symbol.to_string(),
                address: self.pool(),
                why: format!(
                    "the adapter's {POOL_BYTES}-byte static pool has {} bytes left and this needs \
                     {len}",
                    POOL_BYTES.saturating_sub(start)
                ),
            });
        }
        let at = self.pool() + start;
        let mem = GuestMem::new(Arc::clone(&self.space));
        mem.write_bytes(at, &vec![0u8; len], Blame::new(symbol, self.pool(), 0))?;
        *used = start + len;
        Ok(at)
    }

    /// Copy `bytes` plus a NUL into the pool and return its guest address.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the pool is full.
    pub fn intern(&self, symbol: &str, bytes: &[u8]) -> AbiResult<GuestAddr> {
        let at = self.reserve(symbol, bytes.len() + 1)?;
        if !bytes.is_empty() {
            let mem = GuestMem::new(Arc::clone(&self.space));
            mem.write_bytes(at, bytes, Blame::new(symbol, self.pool(), 0))?;
        }
        Ok(at)
    }

    /// How many bytes of [`POOL_BYTES`] have been handed out.
    #[must_use]
    pub fn pool_used(&self) -> usize {
        *self.pool.lock()
    }

    /// Tell the adapter about a loaded image, so `dl_iterate_phdr` enumerates it.
    ///
    /// **`dl_iterate_phdr` is not a stub and cannot become one**: the C++ runtime in
    /// `libroblox.so` is statically linked, so the in-guest unwinder walks 11.5 MB of `.eh_frame`
    /// through this call and C++ exceptions break without it. An adapter with no image registered
    /// therefore *refuses* the call rather than reporting an empty process.
    ///
    /// Registration order is iteration order, which is what a dynamic linker reports: the main
    /// object first, then its dependencies in link order. Nothing here sorts.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if the name does not fit the pool.
    pub fn register_image(&self, info: &DlPhdrInfo) -> AbiResult<()> {
        let name = self.intern("dl_iterate_phdr", info.name.as_bytes())?;
        self.images.lock().push(GuestImage {
            addr: info.addr,
            name,
            phdr: info.phdr,
            phnum: info.phnum,
        });
        Ok(())
    }

    /// Every registered image, in registration order.
    #[must_use]
    pub fn images(&self) -> Vec<GuestImage> {
        self.images.lock().clone()
    }

    /// Publish this instance to the calling thread, and attach the thread if it is new.
    ///
    /// Hold the returned guard across [`Boundary::run`](crate::Boundary::run). Dropping it
    /// restores whatever was published before, so a caller that drives two instances from one
    /// host thread cannot leave the wrong one installed.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] when the arena has no block left, which is a refusal rather than a
    /// second thread sharing the first one's `errno`.
    pub fn activate(self: &Arc<Self>) -> AbiResult<Activation> {
        let base = self.arena;
        let slot = self
            .threads
            .attach_current(MAX_GUEST_THREADS, |index| base + index * THREAD_BLOCK_BYTES)
            .ok_or_else(|| AbiError::Refused {
                symbol: "pthread_self".to_string(),
                address: self.arena,
                why: format!(
                    "this guest instance already has {MAX_GUEST_THREADS} attached threads, and \
                     a {n}th would have to share another thread's errno slot",
                    n = MAX_GUEST_THREADS + 1
                ),
            })?;
        let active = Active { bionic: Arc::clone(self), thread: slot.id, block: slot.block };
        let previous = ACTIVE.with(|cell| cell.borrow_mut().replace(active));
        Ok(Activation { previous })
    }

    /// This thread's `pthread_t`, if it is attached.
    #[must_use]
    pub fn current_thread(&self) -> Option<GuestThreadId> {
        ACTIVE.with(|cell| cell.borrow().as_ref().map(|a| a.thread))
    }

    /// The per-thread arena's first address, for a test that wants to look at it.
    #[must_use]
    pub fn arena(&self) -> GuestAddr {
        self.arena
    }

    /// How many host threads have attached to this instance.
    #[must_use]
    pub fn attached(&self) -> usize {
        self.threads.live()
    }

    /// The futex, for a test that wants its activity counters.
    #[must_use]
    pub fn futex(&self) -> &AddressFutex {
        &self.futex
    }

    /// The monotonic and wall clocks the sync layer's timed waits measure against.
    ///
    /// **Nothing this phase binds reads it yet.** `pthread_cond_timedwait`, `sem_timedwait` and
    /// the rwlock's timed forms are the callers, and none of them is in the reachable set's first
    /// six sections — `pthread_cond_timedwait` is Tier C. It is wired and exercised here rather
    /// than left out, because the trait exists, the implementation is six lines of portable
    /// `std::time`, and an unwired capability is how the next phase discovers that the shape was
    /// wrong.
    #[must_use]
    pub fn clock(&self) -> &HostClock {
        &self.clock
    }

    /// The atexit list, so a caller can run the registered handlers at shutdown.
    #[must_use]
    pub fn atexit(&self) -> &AtexitRegistry {
        &self.atexit
    }

    /// The `rand` state. Process-wide, as C specifies.
    #[must_use]
    pub fn rand_state(&self) -> u32 {
        self.rand.load(Ordering::Relaxed)
    }

    /// Store the `rand` state.
    pub fn set_rand_state(&self, state: u32) {
        self.rand.store(state, Ordering::Relaxed);
    }

    // ------------------------------------------------------------------ phase 3c: threads

    /// Give this guest instance the ability to create threads.
    ///
    /// **Until this is called `pthread_create` refuses by name, naming this method.** There is no
    /// default and there cannot be one: a guest thread is a host thread driving a new
    /// [`GuestCpu`](omni_cpu::GuestCpu) context whose `TPIDR_EL0` points at a populated bionic
    /// TLS block (D13), and only a [`GuestCpuBackend`](omni_cpu::GuestCpuBackend) can produce
    /// one. The same shape as [`set_filesystem_root`](Bionic::set_filesystem_root) and
    /// [`HwcapPolicy::Undecided`](HwcapPolicy): a capability an embedding grants explicitly.
    ///
    /// # What this costs, measured
    ///
    /// **A guest thread costs 24.76-24.84 MiB of commit charge**, measured through this very
    /// path — n = 4 runs of 8 threads, release, `tests/thread_memory.rs`, each run creating them
    /// from real translated ARM64 code and joining them again. It agrees to within 1% with
    /// `omni-cpu/tests/bench.rs`'s **24.56 MiB/thread** for a raw context with no adapter and no
    /// guest stack (n = 1 run of 8 contexts), so what this layer adds per thread is small.
    ///
    /// Most of it is **not** the code cache: the same `omni-cpu` measurement at 8, 32 and 128 MiB
    /// of cache gives 24.56, 34.61 and 34.61 MiB/thread, so shrinking the cache does not help.
    /// 16 MiB of it is a fixed fast-dispatch table `A64EmitX64` holds by value and **writes in
    /// its constructor** for a feature D16 runs **disabled** —
    /// `crates/dynarmic-sys/patches/README.md` item 4 has the patch, and it is **not applied**,
    /// because D5 pins the vendored tree byte-for-byte unmodified.
    ///
    /// **What comes back is the figure the multi-instance requirement turns on, and it is 98.6%**:
    /// after the eight threads were joined, the residual over the pre-thread baseline was 2.79 to
    /// 3.18 MiB in total, 0.35 to 0.40 MiB per thread. So the cost is of *concurrent* guest
    /// threads rather than of threads ever created, and an instance whose guest creates threads
    /// is not an instance that costs the ~16.7 MiB of a loaded `libroblox.so`: it costs that plus
    /// about 24.8 MiB for every thread running at once. Use [`ThreadHost::with_limit`] to bound
    /// it; the default is [`MAX_GUEST_THREADS`], which is the arena's own capacity rather than a
    /// judgement about memory.
    ///
    /// # Errors
    ///
    /// [`AbiError::Refused`] if this instance already has one. It may be set once: a backend that
    /// changed under a running guest thread would leave that thread's TLS block orphaned from the
    /// arena it came from, and the stack-guard value would no longer be one per address space.
    pub fn set_thread_host(&self, host: ThreadHost) -> AbiResult<()> {
        if self.thread_host.set(host).is_err() {
            return Err(AbiError::Refused {
                symbol: "pthread_create".to_string(),
                address: self.arena,
                why: "this guest instance already has a thread host. It may be set once: a second \
                      CPU backend would allocate TLS blocks out of a second arena, and bionic \
                      copies ONE stack-guard value into every thread of a process — so a canary \
                      stored on a frame in one thread and checked in another would fail, which is \
                      a termination rather than an error"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// The thread host, or `None` if the embedding has not supplied one.
    #[must_use]
    pub fn thread_host(&self) -> Option<&ThreadHost> {
        self.thread_host.get()
    }

    /// The arena's thread table, which owns the `errno` blocks and the `pthread_t` counter.
    pub(crate) fn threads_table(&self) -> &ThreadTable {
        &self.threads
    }

    /// The guest address space, so a thread can give its stack back.
    pub(crate) fn space_ref(&self) -> &Arc<GuestSpace> {
        &self.space
    }

    /// Publish this instance to the calling thread **on a slot that was reserved for it**.
    ///
    /// The half of [`activate`](Bionic::activate) a created guest thread uses: its identity and
    /// its block were allocated by `pthread_create`, in the parent, so that the `pthread_t` the
    /// parent wrote is the one this thread's `pthread_self()` will return.
    pub(crate) fn activate_slot(self: &Arc<Self>, slot: ThreadSlot) -> Activation {
        let active = Active { bionic: Arc::clone(self), thread: slot.id, block: slot.block };
        let previous = ACTIVE.with(|cell| cell.borrow_mut().replace(active));
        Activation { previous }
    }

    /// How many guest threads this instance created and are still running.
    ///
    /// **Running**, not "not yet reaped": a joinable thread that has finished and is waiting for
    /// its `pthread_join` holds no context, no stack and no arena block, so counting it against
    /// the live limit would refuse a `pthread_create` that nothing was standing in the way of.
    #[must_use]
    pub fn live_guest_threads(&self) -> usize {
        self.guest_threads
            .lock()
            .records
            .values()
            .filter(|record| !record.state.is_finished())
            .count()
    }

    /// How many guest threads this instance created and has not reaped, running or not.
    #[must_use]
    pub fn guest_thread_records(&self) -> usize {
        self.guest_threads.lock().records.len()
    }

    /// Whether this instance knows a `pthread_t` — that is, whether `pthread_create` produced it.
    #[must_use]
    pub fn knows_guest_thread(&self, thread: u64) -> bool {
        self.guest_threads.lock().records.contains_key(&GuestThreadId(thread))
    }

    /// The state of a guest thread, for a test or an embedding that wants to look.
    #[must_use]
    pub fn guest_thread_state(&self, thread: u64) -> Option<GuestThreadState> {
        self.guest_threads.lock().records.get(&GuestThreadId(thread)).map(|r| r.state.clone())
    }

    /// Every guest thread that stopped for a reason other than returning, oldest first.
    ///
    /// **This is the only place a detached thread's failure can surface.** Nobody joins a
    /// detached thread, so a fault in one would otherwise be silent; it is recorded here and, for
    /// a joinable thread, also reported out of `pthread_join` as a refusal.
    #[must_use]
    pub fn guest_thread_failures(&self) -> Vec<GuestThreadFailure> {
        self.thread_failures.lock().clone()
    }

    /// Ask every created guest thread to stop at its next run-window boundary.
    ///
    /// **D16's mechanism, and its limits are stated rather than implied.** The switch is read
    /// between run windows, so a thread stops after at most one window of guest instructions
    /// ([`ThreadHost::with_step_window`]); it is *not* an interrupt, and a thread blocked in a
    /// guest mutex or inside `pthread_join` stops only once that returns. It is idempotent and
    /// cannot be taken back: an instance that has asked its guest threads to stop is shutting
    /// down.
    pub fn stop_guest_threads(&self) {
        self.threads_stopping.store(true, Ordering::Release);
    }

    /// Whether [`stop_guest_threads`](Bionic::stop_guest_threads) has been called.
    #[must_use]
    pub fn guest_threads_stopping(&self) -> bool {
        self.threads_stopping.load(Ordering::Acquire)
    }

    /// Record a guest thread that did not finish by returning.
    pub(crate) fn record_thread_failure(&self, failure: GuestThreadFailure) {
        self.thread_failures.lock().push(failure);
    }

    /// Put a new thread in the registry, before it starts.
    pub(crate) fn register_guest_thread(
        &self,
        id: GuestThreadId,
        detached: bool,
        start_routine: GuestAddr,
    ) {
        self.guest_threads.lock().records.insert(
            id,
            threads::ThreadRecord {
                state: GuestThreadState::Running,
                detached,
                handle: None,
                start_routine,
            },
        );
    }

    /// Give a registered thread its host handle, once `spawn` has returned one.
    ///
    /// **A detached thread can have finished and been reaped before this runs**, which is why
    /// the missing-record case drops the handle rather than asserting: dropping a `JoinHandle`
    /// detaches the host thread, which is exactly what a detached guest thread wants.
    pub(crate) fn attach_guest_thread_handle(
        &self,
        id: GuestThreadId,
        handle: std::thread::JoinHandle<()>,
    ) {
        let mut guard = self.guest_threads.lock();
        match guard.records.get_mut(&id) {
            Some(record) => record.handle = Some(handle),
            None => drop(handle),
        }
    }

    /// Remove a thread that was registered and then failed to start.
    pub(crate) fn forget_guest_thread(&self, id: GuestThreadId) {
        self.guest_threads.lock().records.remove(&id);
    }

    /// Record how a guest thread ended and wake anything joining it.
    ///
    /// A **detached** thread is removed here instead: nobody will join it, so keeping the record
    /// would be a leak a guest could drive in a loop.
    pub(crate) fn finish_guest_thread(&self, id: GuestThreadId, state: GuestThreadState) {
        {
            let mut guard = self.guest_threads.lock();
            let detached = guard.records.get(&id).is_some_and(|record| record.detached);
            if detached {
                guard.records.remove(&id);
            } else if let Some(record) = guard.records.get_mut(&id) {
                record.state = state;
            }
        }
        self.threads_done.notify_all();
    }

    /// `pthread_join`'s whole decision, under one lock.
    ///
    /// `Err(String)` is a refusal: a thread that stopped without returning has no `void *`, and
    /// `0` with a null `retval` would be indistinguishable from one that returned `NULL`.
    pub(crate) fn join_guest_thread(
        &self,
        me: GuestThreadId,
        thread: u64,
    ) -> Result<JoinOutcome, String> {
        use omni_bionic::errno::consts;
        let target = GuestThreadId(thread);
        // POSIX names this one explicitly, and it is the only deadlock a single thread can cause
        // on its own.
        if target == me {
            return Ok(JoinOutcome::Errno(consts::EDEADLK));
        }
        let mut guard = self.guest_threads.lock();
        let Some(record) = guard.records.get(&target) else {
            return Ok(JoinOutcome::Errno(consts::ESRCH));
        };
        if record.detached {
            return Ok(JoinOutcome::Errno(consts::EINVAL));
        }
        if guard.waits.values().any(|waiting_for| *waiting_for == target) {
            // POSIX: joining a thread another thread is already joining is undefined, and EINVAL
            // is the "not a joinable thread" answer. Letting both through would have two joiners
            // reaping one host thread.
            return Ok(JoinOutcome::Errno(consts::EINVAL));
        }
        // **The deadlock check, and it is a hostile-input defence rather than a nicety.** Two
        // guest threads joining each other is two host threads blocked for ever on guest input.
        // Walking the wait chain forward from the target is bounded by the number of records,
        // because each step moves to a different thread and a repeat would be the cycle itself.
        let mut at = target;
        for _ in 0..=guard.records.len() {
            match guard.waits.get(&at) {
                Some(next) if *next == me => return Ok(JoinOutcome::Errno(consts::EDEADLK)),
                Some(next) => at = *next,
                None => break,
            }
        }

        guard.waits.insert(me, target);
        while guard.records.get(&target).is_some_and(|record| !record.state.is_finished()) {
            self.threads_done.wait(&mut guard);
        }
        guard.waits.remove(&me);
        let Some(record) = guard.records.remove(&target) else {
            // Cannot normally happen: only `finish_guest_thread` removes a record while a thread
            // is running, and only for a detached one, which this call already refused.
            return Ok(JoinOutcome::Errno(consts::ESRCH));
        };
        let handle = record.handle;
        let state = record.state;
        let start_routine = record.start_routine;
        drop(guard);
        // Reap the host thread. It has already recorded its state, so this returns as soon as it
        // finishes unwinding its own frame.
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        match state {
            GuestThreadState::Returned(value) => Ok(JoinOutcome::Returned(value)),
            GuestThreadState::Running => Ok(JoinOutcome::Errno(consts::ESRCH)),
            GuestThreadState::Stopped => Err(format!(
                "the guest joined thread {thread:#x} (start routine {start_routine:#x}), which \
                 this runtime had asked to stop and which therefore never returned a value. \
                 There is no `void *` to report: 0 with a null retval would be indistinguishable \
                 from a thread that returned NULL. See `Bionic::stop_guest_threads`"
            )),
            GuestThreadState::Failed(why) => Err(format!(
                "the guest joined thread {thread:#x} (start routine {start_routine:#x}), which \
                 stopped without returning: {why}. There is no `void *` for a thread that never \
                 produced one, and 0 with a null retval would report that it returned NULL"
            )),
        }
    }

    /// `pthread_detach`'s whole decision, as the value it returns.
    pub(crate) fn detach_guest_thread(&self, thread: u64) -> i32 {
        use omni_bionic::errno::consts;
        let id = GuestThreadId(thread);
        let mut guard = self.guest_threads.lock();
        let Some(record) = guard.records.get(&id) else {
            // Either an id this instance never handed out, or a detached thread that has already
            // finished and been reaped — both are "no such thread", which is what ESRCH says.
            return consts::ESRCH;
        };
        if record.detached {
            // POSIX: "the value specified by thread does not refer to a joinable thread". A
            // second detach answering 0 would make it indistinguishable from the first, and in a
            // real implementation it is a use-after-free of the thread's own descriptor.
            return consts::EINVAL;
        }
        if guard.waits.values().any(|waiting_for| *waiting_for == id) {
            return consts::EINVAL;
        }
        if record.state.is_finished() {
            // Finished and nobody is joining it: reap it now rather than leaving a record no call
            // will ever remove.
            let record = guard.records.remove(&id).expect("just looked it up");
            drop(guard);
            if let Some(handle) = record.handle {
                let _ = handle.join();
            }
            return 0;
        }
        guard.records.get_mut(&id).expect("just looked it up").detached = true;
        0
    }

    /// Bind every symbol this phase implements into `builder`, and return how many.
    ///
    /// Symbols this phase does **not** implement are left exactly as the boundary left them:
    /// `Unbound`, with a real address whose call names the symbol. That is the design (see
    /// [`Binding::Unbound`](crate::Binding::Unbound)) and not a gap — a `fopen` bound to a stub
    /// that returned a plausible `FILE*` would surface three thousand initializers later.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`] if the thunk region cannot hold another slot.
    pub fn bind_into(&self, builder: &BoundaryBuilder) -> AbiResult<usize> {
        for (symbol, handler) in INLINE {
            builder.bind_inline(symbol, *handler)?;
        }
        for (symbol, handler) in REENTRANT {
            builder.bind_reentrant(symbol, *handler)?;
        }
        Ok(INLINE.len() + REENTRANT.len())
    }

    /// Every symbol this phase binds, in table order.
    pub fn bound_symbols() -> impl Iterator<Item = &'static str> {
        INLINE.iter().map(|(s, _)| *s).chain(REENTRANT.iter().map(|(s, _)| *s))
    }

    /// Every symbol serviced **inside** the run loop, in table order.
    pub fn inline_symbols() -> impl Iterator<Item = &'static str> {
        INLINE.iter().map(|(s, _)| *s)
    }

    /// Every symbol serviced on the **exit** path, in table order.
    pub fn reentrant_symbols() -> impl Iterator<Item = &'static str> {
        REENTRANT.iter().map(|(s, _)| *s)
    }

    /// Declare the eighteen `STT_OBJECT` imports into `builder` and fill them in.
    ///
    /// **Call this before the loader resolves symbols**, because a data import only gets an
    /// address if it was declared with a size — [`BoundaryBuilder::declare_data`] is explicit that
    /// there is no default — and the loader's answer is what the guest's `GOT` ends up holding.
    ///
    /// Returns how many were declared. `builder` must be over the same [`GuestSpace`] this
    /// instance was built on; one over a different space produces a typed
    /// [`AbiError::BadPointer`] out of the first write rather than a
    /// silently unfilled object.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`] if the data area cannot hold them, [`AbiError::Refused`] for a
    /// zero [`stack_guard`](data::GuestProcess::stack_guard), and
    /// [`AbiError::BadPointer`] if a write into the data area or the
    /// pool is refused.
    pub fn declare_data_into(
        &self,
        builder: &BoundaryBuilder,
        process: &GuestProcess,
    ) -> AbiResult<usize> {
        data::install(self, builder, process)
    }
}

impl Drop for Bionic {
    fn drop(&mut self) {
        // The arena is this instance's own mapping and nothing else refers to it. A failure to
        // give it back is not reportable from `drop` and is not worth aborting over: the space
        // itself is about to go, and the commit charge goes with it.
        let _ = self.space.unmap(self.arena, ARENA_BYTES);
    }
}

/// The instance published to one thread, plus that thread's identity and block.
#[derive(Clone)]
pub struct Active {
    /// The instance.
    pub bionic: Arc<Bionic>,
    /// This thread's `pthread_t`.
    pub thread: GuestThreadId,
    /// This thread's block in the arena.
    pub block: GuestAddr,
}

thread_local! {
    // The instance this thread's guest code belongs to. A `RefCell<Option<..>>` rather than a
    // raw pointer, because the whole reason this is not a `static` is that the lifetime is a
    // caller's and not the program's.
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

/// Restores the previously published instance when dropped.
pub struct Activation {
    previous: Option<Active>,
}

impl Drop for Activation {
    fn drop(&mut self) {
        ACTIVE.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

impl core::fmt::Debug for Activation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Activation")
    }
}

/// The instance published to this thread, or a typed refusal naming the symbol.
///
/// Cloned out rather than borrowed: the borrow would have to be held across a handler that can
/// re-enter guest code and arrive back here, and an `Arc` clone is one relaxed increment.
pub(crate) fn active(symbol: &str, address: GuestAddr) -> AbiResult<Active> {
    ACTIVE.with(|cell| cell.borrow().clone()).ok_or_else(|| AbiError::BionicNotActive {
        symbol: symbol.to_string(),
        address,
    })
}

/// The state and the view one inline handler works through.
pub(crate) fn enter<'a>(
    call: &'a ImportCall<'_, '_>,
    active: &'a Active,
) -> GuestView<'a> {
    GuestView::new(call.mem(), call.symbol(), call.address(), active)
}

/// Every symbol serviced **inside** the run loop: pure host work, no guest code.
static INLINE: &[(&str, ImportFn)] = handlers::INLINE;

/// Every symbol serviced on the **exit** path, because it calls guest code back.
static REENTRANT: &[(&str, ReentrantFn)] = handlers::REENTRANT;

#[cfg(test)]
mod tests {
    use super::*;

    /// The arena is one commit granule, which is what makes eagerly committing it free.
    ///
    /// **The assertion that stops being true quietly.** `Bionic::new` commits the whole arena
    /// eagerly and justifies it by "lazy and eager cost the same when it is under one granule"
    /// (D10 forbids committing speculatively otherwise). Phase 3b added two tables to it — the
    /// `FILE` objects and the `struct dirent` slots — and a third would be the one that makes the
    /// justification false without changing a line of the code that gives it.
    #[test]
    fn the_arena_fits_in_one_commit_granule() {
        // **The granule is a MEASURED quantity and is referenced rather than restated.** D10 set
        // it by measurement (4 KiB measured *worse* than the VEH fault it rejected), and
        // `omni_mem::DEFAULT_COMMIT_GRANULE` is where that number lives. A literal here would be
        // a fourth copy of a figure this project's own rule says appears once.
        let granule = omni_mem::DEFAULT_COMMIT_GRANULE;
        assert!(
            ARENA_BYTES <= granule,
            "the arena is {ARENA_BYTES} bytes against a commit granule of {granule}: eagerly \
             committing it is no longer free, and `Bionic::new`'s exception to D10 no longer holds"
        );
        // The two ceiling relations are compile-time assertions beside the constants they
        // relate, because both sides are constants; see `MAX_GUEST_DIRS` and the `const _` under
        // it. Whether the *accessors* agree with this arithmetic is a separate question and is
        // the next test.
    }

    /// **The four tables are where the accessors say they are**, in order, adjacent, and inside
    /// the arena.
    ///
    /// # This replaces an assertion that could not fail
    ///
    /// `the_arena_fits_in_one_commit_granule` used to end with
    /// `assert_eq!(ARENA_BYTES, MAX_GUEST_THREADS * THREAD_BLOCK_BYTES + POOL_BYTES + ...)`,
    /// which is a character-for-character restatement of [`ARENA_BYTES`]'s own definition. D23
    /// claimed that test pinned "the four tables against the order the accessors assume"; it did
    /// not, and **nothing did**. Swapping the bodies of [`Bionic::files_base`] and
    /// [`Bionic::dirents_base`], or dropping [`POOL_BYTES`] from the first of them, left the
    /// whole workspace suite passing while `fopen` handed out `FILE` objects on top of the pool's
    /// interned strings. Found by an independent review of phase 3c, and it is the second time in
    /// this project a *total* has been consistent while its *membership* was not.
    ///
    /// So this walks the bases **out of the accessors themselves** and checks that each region
    /// starts exactly where the previous one ends and that the last one ends exactly at the end
    /// of the arena. Restating the sum would reproduce the same defect one term wider, which is
    /// why the sum is not restated here either: every address below is read back from the object.
    #[test]
    fn the_arena_tables_are_where_the_accessors_say_they_are() {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let bionic = Bionic::new(space).expect("a bionic instance");

        // Read out of the object, never recomputed: an accessor that disagrees with the layout is
        // exactly what this is for.
        let arena = bionic.arena();
        let regions: [(&str, GuestAddr, usize); 4] = [
            ("thread blocks", arena, MAX_GUEST_THREADS * THREAD_BLOCK_BYTES),
            ("pool", bionic.pool(), POOL_BYTES),
            ("FILE objects", bionic.files_base(), MAX_GUEST_FILES * FILE_BYTES),
            ("dirent slots", bionic.dirents_base(), MAX_GUEST_DIRS * DIRENT_BYTES),
        ];

        let mut cursor = arena;
        for (name, start, len) in regions {
            assert!(len > 0, "the {name} table is empty, so nothing can come out of it");
            assert_eq!(
                start, cursor,
                "the {name} table starts at {start:#x} and the previous one ends at                  {cursor:#x}: the accessors and the layout disagree, so two tables overlap or a                  gap of the arena is unreachable"
            );
            cursor += len;
        }
        assert_eq!(
            cursor,
            arena + ARENA_BYTES,
            "the four tables do not fill the arena exactly: {} bytes of it are named by no              accessor, or an accessor names memory past its end",
            (arena + ARENA_BYTES).abs_diff(cursor)
        );

        // And the two objects the guest is actually handed come out of the tables they belong
        // to, rather than out of whatever the arithmetic happened to produce. `open_stream` and
        // `attach_dir` are the only two allocators over these tables.
        let mem = GuestMem::new(Arc::clone(&bionic.space));
        let state = Active {
            bionic: Arc::clone(&bionic),
            thread: GuestThreadId(1),
            block: arena,
        };
        let view = GuestView::new(&mem, "fopen", arena, &state);
        let file = bionic.open_stream(&view, 3).expect("a FILE object").expect("a free slot");
        assert!(
            file >= bionic.files_base() && file + FILE_BYTES <= bionic.dirents_base(),
            "a FILE object at {file:#x} is outside the FILE table"
        );
        let dir = bionic.attach_dir(7).expect("a dirent slot");
        assert!(
            dir >= bionic.dirents_base() && dir + DIRENT_BYTES <= arena + ARENA_BYTES,
            "a dirent slot at {dir:#x} is outside the dirent table"
        );
    }
}
