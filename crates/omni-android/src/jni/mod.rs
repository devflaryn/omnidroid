//! **JNI without a JVM** (D7): the `JavaVM`, the `JNIEnv`, and the Java side the host drives.
//!
//! M3 delivered steps 1-5 of `jni-surface.md` §8 — the library mapped, relocated, its imports
//! answered and all 3,594 static initializers run. This module is **steps 6 to 12**: the tables
//! `JNI_OnLoad` reaches through, the class and member surface its registration helpers resolve,
//! and the scripted sequence of downcalls that ART would make from `MainGameActivity` and
//! `NativeHelper`.
//!
//! # The one structural fact everything here is shaped around
//!
//! **The Java side is the initiator.** `jni-surface.md` §0: `libroblox.so` will not reach a first
//! frame by itself — the host must *drive* it with a scripted sequence of downcalls. That makes
//! this an **orchestration** problem and not an interpretation one, and it is why the module
//! splits the way it does: [`script`] owns the order, [`classes`] owns the answers, and the
//! engine responds.
//!
//! | module | what it owns |
//! |---|---|
//! | [`slots`] | the 233 + 8 function-table entries, in `jni.h` order — the specification |
//! | [`refs`] | handles, and why a `jobject` the guest hands back is checked |
//! | [`values`] | Java strings, modified UTF-8, and the descriptor grammar |
//! | [`classes`] | the class registry, and why 409 members is not a JVM |
//! | [`pool`] | pinned guest buffers for `GetStringUTFChars` and the array families |
//! | [`mod@env`] | the `JNIEnv` handlers, and the refusal every unimplemented slot gets |
//! | [`script`] | §8 steps 7-12, as an ordered list of downcalls a host can run |
//! | [`input`] | §8 row 26: `vk.e.onTouch`, host pointer events to `nativePassInput` |
//! | [`keys`] | `vk.g`, host keys to `nativePassKeyEvent`, for a declared hardware keyboard |
//!
//! # Only what the engine uses, and the rest refuse by name
//!
//! `ARCHITECTURE.md` §5's rule — the import list is the specification — applied one level up.
//! **59 of 233** `JNINativeInterface` slots and **2 of 8** `JavaVM` slots are ever dereferenced
//! (§0). The other 174 still occupy entries in a table the guest loads from, so each gets a real
//! thunk address whose call is [`AbiError::JniRefused`] **naming itself**. Nothing here returns a
//! believable value for a function it does not implement: §8.1 ranks "`FindClass` returning
//! `NULL` where the caller does not check" third among the expected failure modes precisely
//! because a plausible answer fails thousands of instructions later.
//!
//! # Where it stops, and why
//!
//! **Step 13 — `initializeNativeCode` — is deliberately not here.** It needs `ALooper`,
//! `AAssetManager` and `ANativeWindow`, none of which exists yet, and §8.1 failure mode 4 is
//! `ALooper_forThread()` returning null making the call return `0` and Java-side startup fail
//! *silently*. Reaching step 13 without a looper would produce exactly that.

pub mod classes;
pub mod env;
pub mod input;
pub mod keys;
pub mod pool;
pub mod refs;
pub mod script;
pub mod slots;
pub mod surface;
pub mod values;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omni_mem::{CommitPolicy, GuestAddr, GuestSpace, Placement, Protection};
use parking_lot::Mutex;

use crate::boundary::{BoundaryBuilder, ImportFn, ReentrantFn};
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

use classes::Registry;
use pool::Pool;
use refs::Handles;
use values::Value;

/// How many threads one instance can hand a `JNIEnv` to.
///
/// **A policy number.** Each costs eight bytes of the arena. The startup path uses one; the game
/// thread `GameActivity_onCreate` spawns is the second, and the engine's worker pools are M8's
/// problem. A thread past this gets a refusal naming the function rather than a second thread
/// sharing the first one's `JNIEnv`, which is the shape that makes two threads share one pending
/// exception.
pub const MAX_JNI_THREADS: usize = 64;

/// How many upcalls one instance records before it stops recording.
///
/// The record is the *measurement* of what the engine asked the Java side for, which is the
/// output §8 steps 21-23 will be read against. Bounded because a sink called once per frame would
/// otherwise grow without limit, and [`Jni::calls_dropped`] reports the shortfall rather than
/// letting the list quietly become a sample.
pub const MAX_CALL_RECORDS: usize = 8192;

/// `sizeof(JavaVMAttachArgs)` on LP64: `jint version` with four bytes of padding, `char* name`,
/// `jobject group`.
///
/// **ASSUMED from the NDK header, not verified — there is still no NDK on this machine**, which
/// is the same standing obligation as `FILE_BYTES` and `dl_phdr_info` (D21, D23). The safety
/// argument is the strongest of that family: the only field this layer reads is `name` at offset
/// 8, the version at offset 0 is read as four bytes, and both are forced once the field order is
/// right because every remaining field is a pointer on LP64.
pub const ATTACH_ARGS_BYTES: usize = 24;

/// Offset of `JavaVMAttachArgs::name`. See [`ATTACH_ARGS_BYTES`].
pub const ATTACH_ARGS_NAME_OFFSET: usize = 8;

/// One upcall the engine made into the Java side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    /// The declaring class, in JNI form.
    pub class: String,
    /// The member name.
    pub member: String,
    /// Its descriptor.
    pub descriptor: String,
    /// The arguments, rendered: a string as its text, a number as its digits, a reference as its
    /// object index or `null`.
    pub args: Vec<String>,
}

/// One `RegisterNatives` binding the engine made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// The class it registered against.
    pub class: String,
    /// The method name.
    pub member: String,
    /// Its descriptor.
    pub descriptor: String,
    /// The guest function it bound.
    pub function: GuestAddr,
}

/// What one thread's `JNIEnv` knows.
#[derive(Debug, Default)]
pub(crate) struct ThreadState {
    /// Whether `AttachCurrentThread` has run, or the host attached it.
    pub(crate) attached: bool,
    /// The name `JavaVMAttachArgs` carried. §2.2: the engine formats one from `gettid`, and
    /// "`AttachCurrentThread` must honour the thread-name field".
    pub(crate) name: Option<String>,
    /// The pending exception, as a handle into [`Handles`]. JNI keeps this per thread, and two
    /// threads sharing one is the reason [`MAX_JNI_THREADS`] refuses rather than reuses a slot.
    pub(crate) pending: Option<u64>,
}

/// The mutable half of an instance.
#[derive(Debug)]
pub(crate) struct JniState {
    pub(crate) handles: Handles,
    pub(crate) registry: Registry,
    pub(crate) threads: Vec<ThreadState>,
    pub(crate) calls: Vec<CallRecord>,
    pub(crate) calls_dropped: u64,
    pub(crate) registrations: Vec<Registration>,
    /// What `ExceptionDescribe` printed, in order. A device writes it to logcat; here it is kept,
    /// because the whole value of `ExceptionDescribe` to a host driving this is the text.
    pub(crate) described: Vec<String>,
    /// Which slot of [`JniState::threads`] a host thread has been given.
    assigned: HashMap<std::thread::ThreadId, usize>,
    /// The objects [`classes::Answer::StaticInstance`] fields hold, by field: a **global
    /// reference this instance owns**, which is what keeps the object alive between reads (an
    /// object is freed with its last reference, and the guest is only ever handed locals).
    pub(crate) statics: BTreeMap<classes::FieldId, u64>,
}

impl JniState {
    /// Record one upcall into the Java side.
    pub(crate) fn record(
        &mut self,
        class: classes::ClassId,
        member: &classes::Member,
        arguments: &[Value],
    ) {
        if self.calls.len() >= MAX_CALL_RECORDS {
            self.calls_dropped += 1;
            return;
        }
        let args = arguments.iter().map(|value| render_value(self, value)).collect();
        let record = CallRecord {
            class: self.registry.class_name(class).to_string(),
            member: member.name.clone(),
            descriptor: member.descriptor.clone(),
            args,
        };
        self.calls.push(record);
    }
}

/// One JNI instance: the tables in guest memory, the registry, and the handle tables.
///
/// Per instance rather than process-wide for the same reason [`crate::bionic::Bionic`] is: this
/// runtime hosts three concurrent guests (the memory figures in `STATUS.md`), and one static
/// would give them one shared handle table and one shared pending exception.
pub struct Jni {
    space: Arc<GuestSpace>,
    mem: GuestMem,
    arena: GuestAddr,
    arena_bytes: usize,
    /// The cell holding the `JNIInvokeInterface*`. Its **address** is the `JavaVM*`.
    vm: GuestAddr,
    /// The first per-thread `JNIEnv` cell.
    envs: GuestAddr,
    /// The `JNINativeInterface` table.
    env_functions: GuestAddr,
    /// The `JNIInvokeInterface` table.
    vm_functions: GuestAddr,
    state: Mutex<JniState>,
    pool: Mutex<Pool>,
    /// How many times each named JNI function has been serviced.
    ///
    /// **Its own lock, and that is load-bearing rather than tidy.** Every handler counts itself
    /// first and then takes the state lock, so a census folded into [`JniState`] would make the
    /// state lock re-entrant — which `parking_lot::Mutex` is not, and which would deadlock on the
    /// first `FindClass`.
    census: Mutex<BTreeMap<&'static str, u64>>,
    /// Thunk address to `JNINativeInterface` index, filled by [`Jni::install_into`].
    env_slots: Mutex<BTreeMap<GuestAddr, usize>>,
    /// Thunk address to `JNIInvokeInterface` index.
    vm_slots: Mutex<BTreeMap<GuestAddr, usize>>,
    /// Bytes of the tables written into guest memory, so a caller can assert they were.
    installed: AtomicU64,
}

impl core::fmt::Debug for Jni {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Jni")
            .field("vm", &format_args!("{:#x}", self.vm))
            .field("env_functions", &format_args!("{:#x}", self.env_functions))
            .field("installed_slots", &self.installed.load(Ordering::Relaxed))
            .finish()
    }
}

impl Jni {
    /// Map the arena and build the registry.
    ///
    /// **Everything this writes into guest memory happens here**, before any CPU exists, which is
    /// task 2's review finding F9: a handler must not map or write to the address space while
    /// generated code is live. The one exception is the pinned pool, which is *reserved* here and
    /// written from a handler — into a range that is already mapped, which is a write and not a
    /// mapping change.
    ///
    /// # Errors
    ///
    /// [`AbiError::Memory`] if the address space could not supply the arena or the pool.
    pub fn new(space: Arc<GuestSpace>) -> AbiResult<Arc<Self>> {
        let page = space.page_size();
        let env_table = slots::ENV_SLOTS.len() * slots::SLOT_BYTES;
        let vm_table = slots::VM_SLOTS.len() * slots::SLOT_BYTES;
        let cells = (1 + MAX_JNI_THREADS) * slots::SLOT_BYTES;
        let arena_bytes = (cells + env_table + vm_table + page - 1) & !(page - 1);
        // Eagerly committed: it is one page and the guest loads a function pointer out of it on
        // the first JNI call there is, so nothing is saved by faulting it in.
        let arena = space.map_anonymous(
            Placement::Anywhere { align: page },
            arena_bytes,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )?;
        let vm = arena;
        let envs = arena + slots::SLOT_BYTES;
        let env_functions = envs + MAX_JNI_THREADS * slots::SLOT_BYTES;
        let vm_functions = env_functions + env_table;

        let mem = GuestMem::new(Arc::clone(&space));
        let blame = Blame::new("Jni::new", arena, 0);
        // Every thread's `JNIEnv` is a cell holding the one function table, which is exactly what
        // a `JNIEnv*` is: `struct _JNIEnv { const JNINativeInterface* functions; }`. Distinct
        // cells so two threads have distinct `JNIEnv*`s, as JNI requires.
        for index in 0..MAX_JNI_THREADS {
            mem.write_u64(envs + index * slots::SLOT_BYTES, env_functions as u64, blame)?;
        }
        mem.write_u64(vm, vm_functions as u64, blame)?;

        let pool = Pool::reserve(Arc::clone(&space))?;
        let mut threads = Vec::with_capacity(MAX_JNI_THREADS);
        threads.resize_with(MAX_JNI_THREADS, ThreadState::default);
        Ok(Arc::new(Self {
            space,
            mem,
            arena,
            arena_bytes,
            vm,
            envs,
            env_functions,
            vm_functions,
            state: Mutex::new(JniState {
                // Mixed from the arena address, which is what ASLR varies per process and per
                // instance. See `refs`'s module docs for what this does and does not buy.
                handles: Handles::new((arena as u64).rotate_left(17) ^ 0x5a5a_a5a5),
                registry: Registry::with_declared(),
                threads,
                calls: Vec::new(),
                calls_dropped: 0,
                registrations: Vec::new(),
                described: Vec::new(),
                assigned: HashMap::new(),
                statics: BTreeMap::new(),
            }),
            pool: Mutex::new(pool),
            census: Mutex::new(BTreeMap::new()),
            env_slots: Mutex::new(BTreeMap::new()),
            vm_slots: Mutex::new(BTreeMap::new()),
            installed: AtomicU64::new(0),
        }))
    }

    /// The `JavaVM*` to pass to `JNI_OnLoad`.
    #[must_use]
    pub fn java_vm(&self) -> GuestAddr {
        self.vm
    }

    /// The `JNINativeInterface` table's address, for a test that reads it back.
    #[must_use]
    pub fn env_functions(&self) -> GuestAddr {
        self.env_functions
    }

    /// The `JNIInvokeInterface` table's address.
    #[must_use]
    pub fn vm_functions(&self) -> GuestAddr {
        self.vm_functions
    }

    /// Checked guest memory over this instance's address space.
    #[must_use]
    pub fn mem(&self) -> &GuestMem {
        &self.mem
    }

    /// The `JNIEnv*` for thread slot `index`.
    #[must_use]
    pub fn env_for(&self, index: usize) -> GuestAddr {
        self.envs + index * slots::SLOT_BYTES
    }

    /// Declare all 241 slots into `builder`, bind them, and write their addresses into the two
    /// guest tables.
    ///
    /// Returns how many slots were installed, which is `233 + 8`.
    ///
    /// **Call it before [`BoundaryBuilder::finish`]** and before any guest code runs. The symbols
    /// are spelled `JNIEnv::FindClass` and `JavaVM::GetEnv`, which no ELF import can collide with
    /// — `.dynstr` names have no `::` in them — so this may be interleaved with the loader's own
    /// resolution in any order.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`] if the thunk region cannot hold 241 more slots;
    /// [`AbiError::BadPointer`] if a write into the arena was refused.
    pub fn install_into(&self, builder: &BoundaryBuilder) -> AbiResult<usize> {
        let blame = Blame::new("Jni::install_into", self.arena, 0);
        let mut env_map = self.env_slots.lock();
        let mut vm_map = self.vm_slots.lock();
        for (index, name) in slots::ENV_SLOTS.iter().enumerate() {
            let symbol = format!("JNIEnv::{name}");
            let address = if env::is_reentrant(index) {
                builder.bind_reentrant(&symbol, env::reentrant_slot as ReentrantFn)?
            } else {
                builder.bind_inline(&symbol, env::inline_slot as ImportFn)?
            };
            self.mem.write_u64(
                self.env_functions + index * slots::SLOT_BYTES,
                address as u64,
                blame,
            )?;
            env_map.insert(address, index);
        }
        for (index, name) in slots::VM_SLOTS.iter().enumerate() {
            let symbol = format!("JavaVM::{name}");
            let address = builder.bind_inline(&symbol, env::vm_slot as ImportFn)?;
            self.mem.write_u64(
                self.vm_functions + index * slots::SLOT_BYTES,
                address as u64,
                blame,
            )?;
            vm_map.insert(address, index);
        }
        let total = slots::ENV_SLOTS.len() + slots::VM_SLOTS.len();
        self.installed.store(total as u64, Ordering::Release);
        Ok(total)
    }

    /// How many slots [`install_into`](Jni::install_into) wrote. Zero before it runs.
    #[must_use]
    pub fn installed_slots(&self) -> u64 {
        self.installed.load(Ordering::Acquire)
    }

    /// Which `JNINativeInterface` slot a thunk address is.
    #[must_use]
    pub fn env_slot_of(&self, address: GuestAddr) -> Option<usize> {
        self.env_slots.lock().get(&address).copied()
    }

    /// Which `JNIInvokeInterface` slot a thunk address is.
    #[must_use]
    pub fn vm_slot_of(&self, address: GuestAddr) -> Option<usize> {
        self.vm_slots.lock().get(&address).copied()
    }

    /// Publish this instance to the calling thread and give it a `JNIEnv`.
    ///
    /// Held across [`Boundary::run`](crate::Boundary::run) exactly as
    /// [`Bionic::activate`](crate::bionic::Bionic::activate) is, and for the same reason: a
    /// handler is a bare `fn` with no captured state, so a thread-local is how it reaches the
    /// instance. A handler that finds none refuses with [`AbiError::JniNotActive`] naming the
    /// function.
    ///
    /// A thread keeps the same slot for the life of the instance, because the engine caches the
    /// `JNIEnv*` it is given.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] past [`MAX_JNI_THREADS`].
    pub fn activate(self: &Arc<Self>) -> AbiResult<JniActivation> {
        let id = std::thread::current().id();
        let index = {
            let mut state = self.state.lock();
            match state.assigned.get(&id) {
                Some(index) => *index,
                None => {
                    let next = state.assigned.len();
                    if next >= MAX_JNI_THREADS {
                        return Err(AbiError::JniRefused {
                            function: "Jni::activate".to_string(),
                            address: self.arena,
                            detail: format!(
                                "{MAX_JNI_THREADS} threads already have a JNIEnv from this \
                                 instance, which is the cap; a further thread is refused rather \
                                 than given another thread's env, which would share its pending \
                                 exception"
                            ),
                        });
                    }
                    state.assigned.insert(id, next);
                    next
                }
            }
        };
        let previous = ACTIVE.with(|cell| {
            cell.borrow_mut().replace(ActiveJni { jni: Arc::clone(self), thread: index })
        });
        Ok(JniActivation { previous })
    }

    /// Attach the calling thread without waiting for the engine to do it.
    ///
    /// On a device `JNI_OnLoad` runs on a thread ART has already attached. The host may say so
    /// here; leaving it unattached is also correct and is the path the engine's own scoped-attach
    /// helper at `0x2174c04` takes — `GetEnv` answers [`slots::JNI_EDETACHED`] and it attaches.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniNotActive`] if this thread has no activation.
    pub fn attach_current_thread(&self, name: Option<&str>) -> AbiResult<GuestAddr> {
        let index = current_thread("Jni::attach_current_thread", self.arena)?;
        let mut state = self.state.lock();
        state.threads[index].attached = true;
        if let Some(name) = name {
            state.threads[index].name = Some(name.to_string());
        }
        Ok(self.env_for(index))
    }

    /// Whether thread slot `index` is attached.
    #[must_use]
    pub fn is_attached(&self, index: usize) -> bool {
        self.state.lock().threads.get(index).is_some_and(|t| t.attached)
    }

    /// The names threads attached with, in slot order. §2.2: the engine formats one from `gettid`
    /// and passes it in `JavaVMAttachArgs`, and honouring it is part of the contract.
    #[must_use]
    pub fn thread_names(&self) -> Vec<Option<String>> {
        self.state.lock().threads.iter().map(|t| t.name.clone()).collect()
    }

    /// Every upcall the engine has made into the Java side, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<CallRecord> {
        self.state.lock().calls.clone()
    }

    /// How many upcalls were not recorded because [`MAX_CALL_RECORDS`] was reached.
    #[must_use]
    pub fn calls_dropped(&self) -> u64 {
        self.state.lock().calls_dropped
    }

    /// Every `RegisterNatives` binding the engine has made.
    #[must_use]
    pub fn registrations(&self) -> Vec<Registration> {
        self.state.lock().registrations.clone()
    }

    /// Every class, member and descriptor the engine asked for that this layer does not declare.
    ///
    /// **The measurement M5 is built on.** A Tier 0 miss is a `CHECK_NOT_NULL` abort waiting to
    /// happen, and this is where it is visible before it happens rather than after.
    #[must_use]
    pub fn misses(&self) -> Vec<classes::Miss> {
        self.state.lock().registry.misses().to_vec()
    }

    /// How many times each JNI function has been serviced.
    #[must_use]
    pub fn census(&self) -> BTreeMap<&'static str, u64> {
        self.census.lock().clone()
    }

    /// What `ExceptionDescribe` reported, in order.
    #[must_use]
    pub fn described(&self) -> Vec<String> {
        self.state.lock().described.clone()
    }

    /// Charge one crossing to a JNI function. See the field's documentation for why this has its
    /// own lock.
    pub(crate) fn count(&self, function: &'static str) {
        *self.census.lock().entry(function).or_insert(0) += 1;
    }

    /// The mutable state, for a handler.
    ///
    /// **Never held across a call into guest code.** A handler that re-entered the guest while
    /// holding this would deadlock on the guest's next JNI call, and the `Call…Method…` family is
    /// written to drop it before [`ReentrantCall::call_guest`](crate::ReentrantCall::call_guest).
    /// It is also never held while taking the pinned pool's lock, and the pool's is never held
    /// while taking this one — the two are ordered by not overlapping at all.
    pub(crate) fn state(&self) -> parking_lot::MutexGuard<'_, JniState> {
        self.state.lock()
    }

    /// Which of this instance's locks are held **right now**, by anyone, as `(name, held)`.
    ///
    /// **`state` is not the only one, which is the point.** A JNI call takes `env_slots` to find
    /// its index, `census` to count itself, and then `state` and sometimes `pool` — so a thread
    /// reported as stopped inside a JNI handler may be waiting on any of the four, and probing
    /// one of them proves nothing about the other three. M6 lost a measurement to exactly that:
    /// `state_is_locked()` answered `false` for a thread that was demonstrably inside
    /// `GetFieldID`, which was read as "not a lock" when it only meant "not *that* lock".
    ///
    /// `try_lock` on each, so it costs nothing until something asks, and each answer is only true
    /// of the instant it was taken.
    #[must_use]
    pub fn locks_held(&self) -> Vec<(&'static str, bool)> {
        vec![
            ("state", self.state.try_lock().is_none()),
            ("pool", self.pool.try_lock().is_none()),
            ("census", self.census.try_lock().is_none()),
            ("env_slots", self.env_slots.try_lock().is_none()),
            ("vm_slots", self.vm_slots.try_lock().is_none()),
        ]
    }

    /// Whether the state lock is held **right now**, by anyone.
    ///
    /// # The invariant this exists to check rather than assert
    ///
    /// [`state`](Jni::state)'s documentation says the lock is never held across a call into guest
    /// code, because a handler that did would deadlock on the guest's next JNI call. That is a
    /// statement about every one of the 233 slots, and nothing enforces it — it is kept by each
    /// of them dropping the guard before `ReentrantCall::call_guest`, which is exactly the shape
    /// `docs/VERIFICATION.md` entry 13 is about: a justification carried by prose across a gap.
    ///
    /// So a diagnostic can ask. A run that has stopped with a thread inside a `Get…ID` and no
    /// crossing anywhere is either blocked on this lock or is not, and those need completely
    /// different work. `try_lock` rather than a flag, so it costs nothing until something asks.
    #[must_use]
    pub fn state_is_locked(&self) -> bool {
        self.state.try_lock().is_none()
    }

    /// The name of a declared class, for a message.
    #[must_use]
    pub fn class_name(&self, class: classes::ClassId) -> String {
        self.state.lock().registry.class_name(class).to_string()
    }

    /// The declared class of the **instance** a `jobject` names, or `None`.
    ///
    /// `None` for a handle this instance did not issue, for one that has been deleted, and for an
    /// object that is not an instance — a `jclass`, a `String`, an array. Those are all "not an
    /// instance of the class you asked about", which is the only question a caller of this has.
    ///
    /// Added for `AAssetManager_fromJava`, which is handed a `jobject` and must establish that it
    /// really is an `android.content.res.AssetManager`: accepting any non-null value would turn a
    /// wrong argument into an asset manager that answers null for every asset, thousands of
    /// instructions from the mistake.
    #[must_use]
    pub fn instance_class_name(&self, handle: u64) -> Option<String> {
        let state = self.state.lock();
        let object = state.handles.object("Jni::instance_class_name", self.arena, handle).ok()?;
        match object {
            refs::Object::Instance { class, .. } => {
                Some(state.registry.class_name(*class).to_string())
            }
            _ => None,
        }
    }

    /// Record one upcall. See [`JniState::record`].
    pub(crate) fn record_call(
        &self,
        class: classes::ClassId,
        member: &classes::Member,
        arguments: &[Value],
    ) {
        self.state.lock().record(class, member, arguments);
    }

    /// Mark a thread attached and keep the name `JavaVMAttachArgs` carried.
    pub(crate) fn attach_thread(&self, thread: usize, name: Option<String>) {
        let mut state = self.state.lock();
        state.threads[thread].attached = true;
        if name.is_some() {
            state.threads[thread].name = name;
        }
    }

    /// Copy `bytes` into the pinned pool. **The state lock must not be held.**
    pub(crate) fn pin(
        &self,
        function: &str,
        address: GuestAddr,
        mem: &GuestMem,
        kind: pool::PinKind,
        owner: refs::ObjectId,
        bytes: &[u8],
    ) -> AbiResult<GuestAddr> {
        self.pool.lock().pin(function, address, mem, kind, owner, bytes)
    }

    /// Release a pinned buffer, honouring `mode`.
    ///
    /// The two locks are taken **one after the other and never together**: the pool's to read the
    /// buffer back, then the state's to put it in the object, then the pool's again to free it.
    /// See [`Jni::state`].
    pub(crate) fn release_pin(
        &self,
        function: &str,
        address: GuestAddr,
        mem: &GuestMem,
        at: GuestAddr,
        mode: i32,
    ) -> AbiResult<()> {
        let (pin, bytes) = {
            let pool = self.pool.lock();
            let pin = pool.pinned(function, address, at)?;
            if pin.kind.writes_back() && mode != slots::JNI_ABORT {
                let (pin, bytes) = pool.read_back(function, address, mem, at)?;
                (pin, Some(bytes))
            } else {
                (pin, None)
            }
        };
        if let Some(bytes) = bytes {
            let mut state = self.state.lock();
            write_back(&mut state, function, address, pin, &bytes)?;
        }
        if mode != slots::JNI_COMMIT {
            self.pool.lock().release(function, address, at)?;
        }
        Ok(())
    }

    /// How many references and objects are live, and the high-water mark of each.
    #[must_use]
    pub fn reference_stats(&self) -> (usize, usize, usize) {
        let state = self.state.lock();
        (state.handles.live_references(), state.handles.live_objects(), state.handles.peak_references())
    }

    /// The first address of the pinned pool, for a diagnostic that has to say whether a guest
    /// pointer is one this layer handed out.
    #[must_use]
    pub fn pool_base(&self) -> GuestAddr {
        self.pool.lock().base()
    }

    /// How many bytes the pinned pool reserves.
    #[must_use]
    pub fn pool_bytes(&self) -> usize {
        self.pool.lock().bytes()
    }

    /// How many pinned buffers are live and how many bytes they hold.
    ///
    /// A non-zero count after a call that should have released everything is a leak, and it is
    /// the one this layer can see: the engine pairs every `Get…Chars` with a `Release…`.
    #[must_use]
    pub fn pin_stats(&self) -> (usize, usize, usize) {
        let pool = self.pool.lock();
        (pool.live_pins(), pool.pinned_bytes(), pool.peak_pinned_bytes())
    }

    /// **Decide what a member answers**, replacing whatever the table declared.
    ///
    /// This is the facility D7's argument rests on, made explicit: 90% of the 409-member surface
    /// is Roblox's own thin Kotlin shell *whose behaviour Omnidroid gets to define*, and this is
    /// where an embedding defines it. [`classes::Answer::Unanswered`] is the table's way of
    /// saying "the layer does not know and will refuse"; this is the host's way of saying it
    /// does.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if the class or the member is not declared, naming both — a
    /// silent no-op here would leave the host believing it had decided something it had not.
    pub fn define(
        &self,
        class: &str,
        member: &str,
        descriptor: &str,
        is_static: bool,
        answer: classes::Answer,
    ) -> AbiResult<()> {
        let mut state = self.state.lock();
        let refuse = |detail: String| AbiError::JniRefused {
            function: "Jni::define".to_string(),
            address: self.arena,
            detail,
        };
        let Some(id) = state.registry.find(class) else {
            return Err(refuse(format!("`{class}` is not declared")));
        };
        let Some(method) = state.registry.method(id, member, descriptor, is_static) else {
            return Err(refuse(format!(
                "`{class}` declares no {}method `{member}{descriptor}`",
                if is_static { "static " } else { "" }
            )));
        };
        state.registry.member_mut(method).expect("just resolved").answer = answer;
        Ok(())
    }

    /// Decide what a **field** answers, as [`define`](Jni::define) decides a method.
    ///
    /// Two calls rather than one because a class may declare a field and a method under the same
    /// name — Java allows it and `jni-surface.md` §3.1's `Configuration` nearly does, with
    /// `getLocales()` beside eighteen fields — so a single `define` would have to guess which was
    /// meant.
    ///
    /// **What needs it:** `AConfiguration` and the Java `Configuration` object answer the same
    /// question, and a host that decided one and left the other at its declared default would
    /// have the engine reading two different screen widths. Making both the host's decision is
    /// the only way they cannot disagree.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if the class or the field is not declared, naming both.
    pub fn define_field(
        &self,
        class: &str,
        field: &str,
        descriptor: &str,
        is_static: bool,
        answer: classes::Answer,
    ) -> AbiResult<()> {
        let mut state = self.state.lock();
        let refuse = |detail: String| AbiError::JniRefused {
            function: "Jni::define_field".to_string(),
            address: self.arena,
            detail,
        };
        let Some(id) = state.registry.find(class) else {
            return Err(refuse(format!("`{class}` is not declared")));
        };
        let Some(found) = state.registry.field(id, field, descriptor, is_static) else {
            return Err(refuse(format!(
                "`{class}` declares no {}field `{field}` of type `{descriptor}`",
                if is_static { "static " } else { "" }
            )));
        };
        state.registry.field_mut(found).expect("just resolved").answer = answer;
        Ok(())
    }

    /// Decide what a declared **method** answers -- [`Self::define_field`]'s twin, for a value
    /// only the embedding can know, measured when it sets the instance up.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if the class or the method is not declared, naming both.
    pub fn define_method(
        &self,
        class: &str,
        method: &str,
        descriptor: &str,
        is_static: bool,
        answer: classes::Answer,
    ) -> AbiResult<()> {
        let mut state = self.state.lock();
        let refuse = |detail: String| AbiError::JniRefused {
            function: "Jni::define_method".to_string(),
            address: self.arena,
            detail,
        };
        let Some(id) = state.registry.find(class) else {
            return Err(refuse(format!("`{class}` is not declared")));
        };
        let Some(found) = state.registry.method(id, method, descriptor, is_static) else {
            return Err(refuse(format!(
                "`{class}` declares no {}method `{method}{descriptor}`",
                if is_static { "static " } else { "" }
            )));
        };
        state.registry.member_mut(found).expect("just resolved").answer = answer;
        Ok(())
    }

    /// The class registry, for a host that wants to declare more classes before running.
    pub fn with_registry<T>(&self, f: impl FnOnce(&mut Registry) -> T) -> T {
        f(&mut self.state.lock().registry)
    }

    /// Create a host-defined object and hand back a **local** reference to it.
    ///
    /// What the startup script builds its `DeviceParams` and `InitParams` arguments with.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if the handle tables are full.
    pub fn new_object(&self, class: &str) -> AbiResult<u64> {
        let mut state = self.state.lock();
        let Some(id) = state.registry.find(class) else {
            return Err(AbiError::JniRefused {
                function: "Jni::new_object".to_string(),
                address: self.arena,
                detail: format!("`{class}` is not declared, so an instance of it cannot be made"),
            });
        };
        state.handles.new_local(
            "Jni::new_object",
            self.arena,
            refs::Object::Instance { class: id, fields: BTreeMap::new() },
        )
    }

    /// Store `value` in the instance field `field` of `object`, as the Java side's constructor
    /// or builder would have -- for the parameter objects a host builds and the engine reads back
    /// through an accessor answered [`classes::Answer::Field`].
    ///
    /// **The stored object is anchored by a global reference this instance keeps**, because a
    /// Java field keeps its referent alive: a host that deleted its own local afterwards must not
    /// leave the field naming a freed slot. The anchor is never released -- a field is never
    /// cleared here -- which bounds the cost at one reference per call; hosts call this a handful
    /// of times per surface, not per frame.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] when `object` is not an instance, or its class declares no
    /// instance field `field` of an object type; [`AbiError::JniBadHandle`] for a handle this
    /// instance did not issue.
    pub fn set_object_field(&self, object: u64, field: &str, value: u64) -> AbiResult<()> {
        const NAME: &str = "Jni::set_object_field";
        let mut state = self.state.lock();
        let holder = state.handles.resolve_id(NAME, self.arena, object)?;
        let held = state.handles.resolve_id(NAME, self.arena, value)?;
        let class = match state.handles.object_of(holder) {
            Some(refs::Object::Instance { class, .. }) => *class,
            _ => {
                return Err(AbiError::JniRefused {
                    function: NAME.to_string(),
                    address: self.arena,
                    detail: "the holder is not an instance of a declared class".to_string(),
                })
            }
        };
        let Some(index) = state.registry.class(class).and_then(|c| {
            c.fields.iter().position(|f| f.name == field && !f.is_static && f.descriptor.starts_with('L'))
        }) else {
            return Err(AbiError::JniRefused {
                function: NAME.to_string(),
                address: self.arena,
                detail: format!(
                    "`{}` declares no object-typed instance field `{field}`",
                    state.registry.class_name(class)
                ),
            });
        };
        let _anchor = state.handles.reference_to(NAME, self.arena, refs::RefKind::Global, held)?;
        if let Some(refs::Object::Instance { fields, .. }) = state.handles.object_of_mut(holder) {
            fields.insert(
                classes::FieldId { class, member: index as u16 },
                values::Value::Object(Some(held)),
            );
        }
        Ok(())
    }

    /// Execute `sput-object value, class->field:descriptor` for a static field the app's own Java
    /// code assigns -- one declared [`classes::Answer::Assigned`] -- as the scripted startup does
    /// where the app's bytecode does ([`script::JavaStatement`]).
    ///
    /// `value` is a handle this instance issued, or `0` for Java `null`, which clears the field.
    /// **The stored object is anchored by a global reference this instance keeps**, as a class's
    /// static keeps its referent alive, and a later store releases the anchor it replaces -- so
    /// the host's own local may be deleted afterwards and the field still names the object.
    ///
    /// **Assignability is checked**, against the declared superclass chain: a field typed
    /// `Landroid/content/Context;` takes a `MainGameActivity` and refuses a `java/util/List`. The
    /// Java verifier would never have let the second through, so storing it would be this layer
    /// inventing a state the app cannot reach.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] when the class or the static field is not declared, when the field
    /// is not declared `Assigned` (a field the host does not model as Java-written must not be
    /// written by it), or when the value is not an instance of a class the field's type admits;
    /// [`AbiError::JniBadHandle`] for a handle this instance did not issue.
    pub fn put_static_object(
        &self,
        class: &str,
        field: &str,
        descriptor: &str,
        value: u64,
    ) -> AbiResult<()> {
        const NAME: &str = "Jni::put_static_object";
        let mut state = self.state.lock();
        let refuse = |detail: String| AbiError::JniRefused {
            function: NAME.to_string(),
            address: self.arena,
            detail,
        };
        let Some(id) = state.registry.find(class) else {
            return Err(refuse(format!("`{class}` is not declared")));
        };
        let Some(found) = state.registry.field(id, field, descriptor, true) else {
            return Err(refuse(format!("`{class}` declares no static field `{field}` of type `{descriptor}`")));
        };
        let answer = state.registry.field_member(found).map(|member| member.answer);
        if answer != Some(classes::Answer::Assigned) {
            return Err(refuse(format!(
                "`{class}.{field}` is declared {answer:?}, not as a static the Java side assigns, \
                 so a Java statement storing into it would be writing state this layer answers \
                 some other way"
            )));
        }
        let held = if value == 0 {
            None
        } else {
            let object = state.handles.resolve_id(NAME, self.arena, value)?;
            let wanted = descriptor
                .strip_prefix('L')
                .and_then(|rest| rest.strip_suffix(';'))
                .and_then(|name| state.registry.find(name));
            let admits = match (state.handles.object_of(object), wanted) {
                (Some(refs::Object::Instance { class: of, .. }), Some(wanted)) => {
                    state.registry.extends(*of, wanted)
                }
                _ => false,
            };
            if !admits {
                let got = state
                    .handles
                    .object_of(object)
                    .map_or_else(|| "a freed object".to_string(), |held| render(&state, held));
                return Err(refuse(format!(
                    "`{class}.{field}` is typed `{descriptor}` and the value is {got}, which that \
                     type does not admit on the declared superclass chain"
                )));
            }
            Some(object)
        };
        // The new anchor first, then the old one released: a store of the object the field
        // already holds must not free it in between.
        let anchor = match held {
            Some(object) => {
                Some(state.handles.reference_to(NAME, self.arena, refs::RefKind::Global, object)?)
            }
            None => None,
        };
        let replaced = match anchor {
            Some(anchor) => state.statics.insert(found, anchor),
            None => state.statics.remove(&found),
        };
        if let Some(old) = replaced {
            state.handles.delete(NAME, self.arena, refs::RefKind::Global, old)?;
        }
        Ok(())
    }

    /// A **local reference to the `jclass`** of a declared class.
    ///
    /// What a `static` native method's second argument is: JNI hands a static native
    /// `(JNIEnv*, jclass)` where an instance one gets `(JNIEnv*, jobject)`, and the scripted
    /// sequence is entirely static methods.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if the class is not declared.
    pub fn class_reference(&self, class: &str) -> AbiResult<u64> {
        let mut state = self.state.lock();
        let Some(id) = state.registry.find(class) else {
            return Err(AbiError::JniRefused {
                function: "Jni::class_reference".to_string(),
                address: self.arena,
                detail: format!("`{class}` is not declared, so there is no jclass for it"),
            });
        };
        state.handles.new_local("Jni::class_reference", self.arena, refs::Object::Class(id))
    }

    /// Create a `java.lang.String` and hand back a local reference to it.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if the handle tables are full.
    pub fn new_string(&self, text: &str) -> AbiResult<u64> {
        let mut state = self.state.lock();
        state.handles.new_local(
            "Jni::new_string",
            self.arena,
            refs::Object::String(values::JavaString::from_str(text)),
        )
    }

    /// Read a host-side value back out of a handle, for a test or a script that wants to check
    /// what the engine was handed.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniBadHandle`] for a handle this instance did not issue.
    pub fn describe(&self, handle: u64) -> AbiResult<String> {
        let state = self.state.lock();
        let object = state.handles.object("Jni::describe", self.arena, handle)?;
        Ok(render(&state, object))
    }
}

impl Drop for Jni {
    fn drop(&mut self) {
        // As `Bionic::drop` and `Pool::drop`: the arena is this instance's own mapping, the space
        // is going with it, and a failure here is neither reportable nor worth aborting over.
        let _ = self.space.unmap(self.arena, self.arena_bytes);
    }
}

/// Put a released array buffer back into the object it was a copy of.
///
/// The length is the *object's*, not the buffer's, and the two are compared: a buffer whose
/// length no longer matches means the object was replaced between the get and the release, and
/// writing the old bytes into the new object would be the plausible wrong answer.
fn write_back(
    state: &mut JniState,
    function: &str,
    address: GuestAddr,
    pin: pool::Pinned,
    bytes: &[u8],
) -> AbiResult<()> {
    let refuse = |detail: String| AbiError::JniRefused {
        function: function.to_string(),
        address,
        detail,
    };
    let Some(object) = state.handles.object_of_mut(pin.owner) else {
        return Err(refuse(
            "the array this buffer was a copy of has been freed, so there is nowhere to write it \
             back to"
                .to_string(),
        ));
    };
    match (object, pin.kind) {
        (refs::Object::ByteArray(values), pool::PinKind::ByteArray) => {
            if bytes.len() != values.len() {
                return Err(refuse(format!(
                    "the buffer holds {} bytes and the array now has {}",
                    bytes.len(),
                    values.len()
                )));
            }
            for (slot, byte) in values.iter_mut().zip(bytes) {
                *slot = *byte as i8;
            }
        }
        (refs::Object::IntArray(values), pool::PinKind::IntArray) => {
            if bytes.len() != values.len() * 4 {
                return Err(refuse("the buffer's length no longer matches the array".to_string()));
            }
            for (slot, chunk) in values.iter_mut().zip(bytes.chunks_exact(4)) {
                *slot = i32::from_le_bytes(chunk.try_into().expect("chunks_exact(4)"));
            }
        }
        (refs::Object::LongArray(values), pool::PinKind::LongArray) => {
            if bytes.len() != values.len() * 8 {
                return Err(refuse("the buffer's length no longer matches the array".to_string()));
            }
            for (slot, chunk) in values.iter_mut().zip(bytes.chunks_exact(8)) {
                *slot = i64::from_le_bytes(chunk.try_into().expect("chunks_exact(8)"));
            }
        }
        (refs::Object::FloatArray(values), pool::PinKind::FloatArray) => {
            if bytes.len() != values.len() * 4 {
                return Err(refuse("the buffer's length no longer matches the array".to_string()));
            }
            for (slot, chunk) in values.iter_mut().zip(bytes.chunks_exact(4)) {
                *slot = f32::from_le_bytes(chunk.try_into().expect("chunks_exact(4)"));
            }
        }
        (object, kind) => {
            return Err(refuse(format!(
                "the buffer is {} and the object it names is {}",
                kind.name(),
                object.kind_name()
            )))
        }
    }
    Ok(())
}

/// Render an object for a call record or a diagnostic.
pub(crate) fn render(state: &JniState, object: &refs::Object) -> String {
    match object {
        refs::Object::String(text) => text.to_string_lossy(),
        refs::Object::Class(id) => format!("class {}", state.registry.class_name(*id)),
        refs::Object::Instance { class, .. } => {
            format!("instance of {}", state.registry.class_name(*class))
        }
        refs::Object::ByteArray(bytes) => format!("byte[{}]", bytes.len()),
        refs::Object::IntArray(values) => format!("int[{}]", values.len()),
        refs::Object::LongArray(values) => format!("long[{}]", values.len()),
        refs::Object::FloatArray(values) => format!("float[{}]", values.len()),
        refs::Object::ObjectArray { element, elements } => {
            format!("{}[{}]", state.registry.class_name(*element), elements.len())
        }
        refs::Object::DirectByteBuffer { address, capacity } => {
            format!("ByteBuffer({address:#x}, {capacity})")
        }
        refs::Object::Throwable { class, message } => {
            format!("{}: {message}", state.registry.class_name(*class))
        }
    }
}

/// Render a [`Value`] for a call record.
fn render_value(state: &JniState, value: &Value) -> String {
    match value {
        Value::Void => "void".to_string(),
        Value::Boolean(v) => v.to_string(),
        Value::Byte(v) => v.to_string(),
        Value::Char(v) => v.to_string(),
        Value::Short(v) => v.to_string(),
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        Value::Float(v) => v.to_string(),
        Value::Double(v) => v.to_string(),
        Value::Text(text) => text.clone(),
        Value::Object(None) => "null".to_string(),
        Value::Object(Some(id)) => match state.handles.object_of(*id) {
            Some(object) => render(state, object),
            None => format!("<freed object {}>", id.index()),
        },
    }
}

/// The instance published to one thread, and which `JNIEnv` slot it has.
#[derive(Clone)]
struct ActiveJni {
    jni: Arc<Jni>,
    thread: usize,
}

thread_local! {
    // The JNI instance this thread's guest code belongs to. The same shape and the same argument
    // as `bionic::ACTIVE`: a handler is a bare `fn` and this is the only channel it has.
    static ACTIVE: RefCell<Option<ActiveJni>> = const { RefCell::new(None) };
}

/// Restores the previously published instance when dropped.
pub struct JniActivation {
    previous: Option<ActiveJni>,
}

impl Drop for JniActivation {
    fn drop(&mut self) {
        ACTIVE.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

impl core::fmt::Debug for JniActivation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("JniActivation")
    }
}

/// The instance published to this thread, or a typed refusal naming the JNI function.
pub(crate) fn active(function: &str, address: GuestAddr) -> AbiResult<(Arc<Jni>, usize)> {
    active_opt()
        .ok_or_else(|| AbiError::JniNotActive { function: function.to_string(), address })
}

/// The instance published to this thread, without building an error message.
///
/// A handler on the ≈33 ns path calls this and formats the refusal only if it is `None`, so the
/// path that works allocates nothing — the same reasoning
/// [`Boundary::start_census`](crate::Boundary::start_census) applies to its own counter.
pub(crate) fn active_opt() -> Option<(Arc<Jni>, usize)> {
    ACTIVE.with(|cell| cell.borrow().clone()).map(|active| (active.jni, active.thread))
}

/// This thread's `JNIEnv` slot, or a refusal.
fn current_thread(function: &str, address: GuestAddr) -> AbiResult<usize> {
    active(function, address).map(|(_, index)| index)
}

/// A [`Jni`] as something a **created guest thread** carries.
///
/// See [`ThreadLocalInstance`](crate::bionic::ThreadLocalInstance). A wrapper rather than an impl
/// on `Jni` itself because [`Jni::activate`] takes `&Arc<Self>`.
///
/// Publishing this to a guest thread is what gives that thread **its own `JNIEnv`**: `activate`
/// assigns the thread the next free slot of [`MAX_JNI_THREADS`], so the game thread's pending
/// exception and its local references are its own. It refuses past that cap rather than handing
/// out a second thread's env, and the refusal is reported as a thread failure — which is the
/// right direction: two threads sharing one pending exception is the failure no later test sees.
pub struct JniThreadInstance(Arc<Jni>);

impl core::fmt::Debug for JniThreadInstance {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("JniThreadInstance")
    }
}

impl crate::bionic::ThreadLocalInstance for JniThreadInstance {
    fn name(&self) -> &'static str {
        "Jni"
    }

    fn publish(&self) -> AbiResult<Box<dyn core::any::Any>> {
        Ok(Box::new(self.0.activate()?))
    }
}

impl Jni {
    /// This instance, as something a created guest thread carries.
    ///
    /// **An embedding with a `Jni` must pass this to
    /// [`ThreadHost::with_instance`](crate::bionic::ThreadHost::with_instance)**, or a guest
    /// thread that reaches any JNI function refuses and dies. The GameActivity glue's game thread
    /// does: `android_app_entry` runs on it and the engine's own scoped-attach helper calls
    /// `GetEnv` from it.
    #[must_use]
    pub fn thread_instance(self: &Arc<Self>) -> Arc<dyn crate::bionic::ThreadLocalInstance> {
        Arc::new(JniThreadInstance(Arc::clone(self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance() -> Arc<Jni> {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        Jni::new(space).expect("a JNI instance")
    }

    /// A `JNIEnv*` is a pointer to a pointer to the function table, and a `JavaVM*` is a pointer
    /// to a pointer to the invoke table. Read back out of guest memory, because that is what the
    /// guest will do.
    #[test]
    fn the_env_and_vm_are_shaped_the_way_the_guest_dereferences_them() {
        let jni = instance();
        let blame = Blame::new("test", 0, 0);
        for index in 0..MAX_JNI_THREADS {
            let env = jni.env_for(index);
            assert_eq!(
                jni.mem().read_u64(env, blame).expect("mapped"),
                jni.env_functions() as u64,
                "thread {index}'s JNIEnv must point at the one function table"
            );
        }
        assert_eq!(
            jni.mem().read_u64(jni.java_vm(), blame).expect("mapped"),
            jni.vm_functions() as u64
        );
        // Distinct per thread, which is what JNI requires and what keeps one thread's pending
        // exception out of another's.
        assert_ne!(jni.env_for(0), jni.env_for(1));
    }

    /// Before `install_into`, every table entry is zero. A guest that reached a JNI function
    /// through an uninstalled table would branch to zero, which is the failure the whole thunk
    /// region exists to replace — so this asserts that installing is what fills them.
    #[test]
    fn the_tables_are_empty_until_they_are_installed() {
        let jni = instance();
        let blame = Blame::new("test", 0, 0);
        assert_eq!(jni.installed_slots(), 0);
        for index in 0..slots::ENV_SLOTS.len() {
            let at = jni.env_functions() + index * slots::SLOT_BYTES;
            assert_eq!(jni.mem().read_u64(at, blame).expect("mapped"), 0, "slot {index}");
        }
    }

    /// A thread keeps its slot, because the engine caches the `JNIEnv*` it is given.
    #[test]
    fn a_thread_keeps_the_same_env_across_activations() {
        let jni = instance();
        let first = {
            let _a = jni.activate().expect("activated");
            current_thread("test", 0).expect("a slot")
        };
        let second = {
            let _a = jni.activate().expect("activated");
            current_thread("test", 0).expect("a slot")
        };
        assert_eq!(first, second);
    }

    /// A handler on a thread with no activation refuses by name rather than constructing a
    /// default instance — the same argument `BionicNotActive` rests on, one level up: a
    /// per-call default would give two guest threads their own private handle table.
    #[test]
    fn a_handler_with_no_activation_refuses_by_name() {
        let error = active("FindClass", 0x1234).expect_err("no activation");
        match error {
            AbiError::JniNotActive { function, address } => {
                assert_eq!(function, "FindClass");
                assert_eq!(address, 0x1234);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_host_can_attach_the_calling_thread_and_the_name_is_kept() {
        let jni = instance();
        let _a = jni.activate().expect("activated");
        assert!(!jni.is_attached(0), "a thread starts detached, which is what GetEnv reports");
        let env = jni.attach_current_thread(Some("main")).expect("attached");
        assert_eq!(env, jni.env_for(0));
        assert!(jni.is_attached(0));
        assert_eq!(jni.thread_names()[0].as_deref(), Some("main"));
    }

    /// The whole declared surface, as a membership check on the registry the instance builds.
    #[test]
    fn a_fresh_instance_declares_the_whole_surface_and_has_missed_nothing() {
        let jni = instance();
        let (classes, members) =
            jni.with_registry(|registry| (registry.class_count(), registry.member_count()));
        // The hand-written table plus the generated one, which overlap: the lower bound is the
        // larger of the two and the upper bound is their sum.
        assert!(classes >= classes::DECLARED.len().max(surface::DEX_CLASSES), "{classes}");
        assert!(classes <= classes::DECLARED.len() + surface::DEX_CLASSES, "{classes}");
        assert!(members > surface::DEX_MEMBERS, "the declared surface is {members} members");
        assert!(jni.misses().is_empty());
        assert!(jni.calls().is_empty());
        assert_eq!(jni.calls_dropped(), 0);
    }
}
