//! **The guest's `libEGL.so` and `libGLESv2.so`: real EGL and OpenGL ES, forwarded to the host's.**
//!
//! # Why this exists: measured
//!
//! This host's only Vulkan device is Mesa lavapipe (`VK_PHYSICAL_DEVICE_TYPE_CPU`). With the network
//! working, the engine fetched its flags, reached `APP_READY` Home, and then refused the device by
//! its own rule -- `Vulkan: Device llvmpipe (LLVM 21.1.8, 256 bits) is emulated, skipping`,
//! `Mode 6 failed: Unable to pick Vulkan device` (D8: that refusal is the engine's and is not worked
//! around here) -- and fell back to OpenGL ES, whose first call, `eglGetDisplay`, killed the thread:
//! nothing implemented it. `libroblox.so` links `libEGL.so` and `libGLESv2.so` (`DT_NEEDED`) and
//! imports **91** of their symbols directly (17 `egl*`, 74 `gl*`, all core ES 2.0 / EGL 1.x); every
//! GLES 3 and extension entry point arrives through `eglGetProcAddress`.
//!
//! # The shape: identity mapping, and a calling convention to translate
//!
//! Vulkan is decoded structure by structure (see [`crate::vulkan`]). GLES is not, because there is
//! nothing to decode: its arguments are integers, floats and pointers into memory the guest owns,
//! and under identity mapping (ARCHITECTURE section 1) a guest pointer **is** a host pointer and a GL
//! object name is the driver's integer. What does need translating is where each argument *is*: the
//! guest passes AAPCS64 (integers in `x0`-`x7` then the stack, floats in `v0`-`v7` then the stack,
//! each bank counted separately) and the host expects SysV or the Microsoft x64 convention.
//!
//! So [`signatures`] -- generated from the Khronos registries by `tools/gen_gles_signatures.py`,
//! never typed by hand -- gives every command's parameter classes in AAPCS64's terms (which guest
//! register or stack slot each argument is read from) and its exact C widths (which typed
//! `extern "C"` function-pointer type the host call goes through). There is one generated caller per
//! distinct host shape (176 of them), and the Rust compiler applies the host's ABI. Nothing here
//! assumes SysV: on Windows the same callers put `glUniform4f`'s `GLint` in `rcx` and its first
//! float in `xmm1`, because that is what `extern "C"` means there.
//!
//! # Where values do not pass through, each decided by reading
//!
//! | call | why it is not a plain forward | what this layer does |
//! |---|---|---|
//! | `glGetString`, `glGetStringi`, `eglQueryString` | the host returns a pointer into its own static data, which this layer's own validated imports (`strlen`, `strstr`, `memcpy`) refuse because it is not guest memory | the host's exact text is copied once into a guest pool and that address returned ([`gl`]) |
//! | `glMapBufferRange`, `glMapBufferOES`, `glUnmapBuffer`, `glFlushMappedBufferRange`, `glGetBufferPointerv` | the driver's mapping is host memory, for the same reason | a guest-memory **shadow**, filled from the mapping when the access bits make its contents defined, copied back on flush/unmap as the access bits say ([`gl`]) |
//! | `eglGetDisplay(EGL_DEFAULT_DISPLAY)` | Android's default display has no host meaning | the host's display for the guest window's system ([`GlesHost::default_display`]) |
//! | `eglCreateWindowSurface` | its window is an `ANativeWindow *` of this layer's | the host window behind it, resolved exactly as `vkCreateAndroidSurfaceKHR` resolves it ([`egl`]) |
//! | `eglGetConfigAttrib(EGL_NATIVE_VISUAL_ID)` | Android's value is a `WINDOW_FORMAT_*`; the host's is an X visual id | translated from the host config's channel sizes, recorded |
//! | `eglChooseConfig` with `EGL_RECORDABLE_ANDROID` / `EGL_FRAMEBUFFER_TARGET_ANDROID` | a desktop EGL rejects them with `EGL_BAD_ATTRIBUTE` (MEASURED, Mesa 26.0.8 on X11) | dropped when the host lacks the extension, recorded |
//! | `eglGetProcAddress` | the guest must get a thunk, never a host address | a thunk when the host has the name, NULL when it does not or the registry has no such GLES/EGL command |
//! | callbacks (`glDebugMessageCallback*`, `eglDebugMessageControlKHR`, `eglSetBlobCacheFuncsANDROID`) | the host would call guest ARM64 from its own threads | refused by name |
//!
//! **Pointers the host keeps past the call are still guest memory**, and that is fine under identity
//! mapping and worth saying: a client-side vertex array (`glVertexAttribPointer` with no
//! `GL_ARRAY_BUFFER` bound) is read by the driver at the *draw*, possibly from its own worker
//! threads; that is guest memory the guest still owns, and a fault on an uncommitted page of it is
//! the demand pager's (`omni_platform::fault` serves host code reading guest memory as it serves
//! the JIT).
//!
//! # The census
//!
//! Unconditional, like [`crate::vulkan`]'s and for `VERIFICATION.md` entry 15's reason: every call
//! is counted by name (an atomic per entry, never dropped), every `eglGetProcAddress` is recorded
//! with its answer, every substitution is recorded, and [`Gles::presents`] counts the
//! `eglSwapBuffers` the host answered `EGL_TRUE` for. [`Gles::report`] prints it as `GLES:` lines.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use omni_mem::{GuestAddr, GuestSpace};
use parking_lot::Mutex;

use crate::boundary::{BoundaryBuilder, ImportCall, ImportFn};
use crate::error::{AbiError, AbiResult};

mod egl;
mod gl;
pub mod host;
#[allow(clippy::all, missing_docs)]
pub mod signatures;

pub use host::{DisplayOpened, GlesHost, HostProc, SurfaceMade};
pub use signatures::{Shape, Signature, SHAPES, SIGNATURES};

/// The `soname`s the guest links, in `DT_NEEDED` order.
pub const SONAMES: [&str; 2] = ["libEGL.so", "libGLESv2.so"];

/// How many anonymous slots `eglGetProcAddress` hands out of, for names that are not bound by name.
///
/// **Measured, like [`crate::vulkan::MAX_PROC_SLOTS`].** A name the engine can ask for has to be in
/// `libroblox.so` as a NUL-terminated string. MEASURED on `Roblox-2.738.1397.apk`: 188 distinct
/// strings match `(gl|egl)[A-Z]...`; 164 of them are commands of the `gles2`/EGL registries, 91 of
/// those are imported directly and the rest are candidates for this pool. 256 is that bound with
/// room; the 257th distinct name is a refusal naming the function, not a NULL -- a full pool is a
/// fact about this layer, and NULL means "the host does not have it".
pub const MAX_PROC_SLOTS: usize = 256;

/// How many `eglGetProcAddress` requests and how many substitutions are kept in order.
pub const MAX_RECORDS: usize = 512;

/// The most arguments any GLES or EGL command takes (15, `glTexSubImage3DOES`-sized ones) plus one.
pub const MAX_ARGS: usize = 16;

/// Whether a registry origin is a **core** version -- the names a real `libGLESv2.so`/`libEGL.so`
/// exports and the guest can import directly.
#[must_use]
pub fn is_core(signature: &Signature) -> bool {
    signature.origin.starts_with("GL_ES_VERSION_") || signature.origin.starts_with("EGL_VERSION_")
}

/// Every command bound **by name**: the core ES 2.0-3.2 and EGL 1.0-1.5 commands, in table order.
pub fn core_signatures() -> impl Iterator<Item = &'static Signature> {
    SIGNATURES.iter().filter(|s| is_core(s))
}

/// How many function slots [`Gles::bind_into`] adds to a [`BoundaryBuilder`]: every core command
/// plus the [`MAX_PROC_SLOTS`] pool.
#[must_use]
pub fn bound_symbol_count() -> usize {
    core_signatures().count() + MAX_PROC_SLOTS
}

/// The symbol a pool slot is bound under -- not a C identifier, so it cannot collide with
/// anything in `libroblox.so`'s `.dynstr` (the Vulkan pool's argument).
#[must_use]
pub fn proc_slot_symbol(index: usize) -> String {
    format!("gles::proc[{index}]")
}

/// The registry's signature for `name`, if the `gles2` or EGL registry has such a command.
#[must_use]
pub fn signature(name: &str) -> Option<&'static Signature> {
    static INDEX: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        SIGNATURES.iter().enumerate().map(|(i, s)| (s.name, i)).collect()
    });
    index.get(name).map(|&i| &SIGNATURES[i])
}

/// What `eglGetProcAddress` answered for one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcAnswer {
    /// A thunk of this boundary.
    Thunk(GuestAddr),
    /// NULL: the registry has the command and the host's lookup answered NULL for it.
    NullFromHost,
    /// NULL: neither the `gles2` nor the EGL registry has a command of that name, so there is no
    /// prototype to call it with and no GLES implementation can have it.
    NullNotInRegistry,
}

/// One `eglGetProcAddress` call, as the census records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRequest {
    /// The name asked for.
    pub name: String,
    /// What was answered.
    pub answer: ProcAnswer,
    /// The guest address the call came from.
    pub caller: GuestAddr,
}

/// One place this layer answered with something other than the host's own value, as the census
/// records it. The `GLES:` lines print every one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Substitution {
    /// The guest call it happened in.
    pub call: &'static str,
    /// What was substituted, for what, and why, in one line.
    pub what: String,
    /// How many times this exact substitution was made.
    pub times: u64,
}

// --------------------------------------------------------------------------- the table

/// A special handler: the calls whose values do not simply pass through.
type SpecialFn = fn(&Gles, &mut ImportCall<'_, '_>, &Call) -> AbiResult<()>;

/// What a slot stands for, fixed when it is bound (a named slot) or handed out (a pool slot).
#[derive(Clone, Copy)]
struct Assigned {
    signature: &'static Signature,
    special: Option<SpecialFn>,
}

struct Entry {
    assigned: OnceLock<Assigned>,
    /// The host function, looked up once. `None` inside means the host does not have it.
    host: OnceLock<Option<HostProc>>,
    calls: AtomicU64,
}

impl Entry {
    fn new() -> Self {
        Self { assigned: OnceLock::new(), host: OnceLock::new(), calls: AtomicU64::new(0) }
    }
}

/// Every slot, frozen once [`Gles::bind_into`] has run.
struct Table {
    entries: Vec<Entry>,
    by_address: HashMap<GuestAddr, u32>,
    addresses: Vec<GuestAddr>,
    /// Entries `0..named` are bound by name; the rest are the pool.
    named: usize,
}

/// One guest call in progress, with its arguments already read in AAPCS64 order.
struct Call {
    name: &'static str,
    signature: &'static Signature,
    lanes: [u64; MAX_ARGS],
    address: GuestAddr,
    caller: GuestAddr,
    entry: u32,
}

impl Call {
    fn refuse(&self, why: String) -> AbiError {
        AbiError::Refused { symbol: self.name.to_string(), address: self.address, why }
    }
}

struct State {
    host: Option<Arc<dyn GlesHost>>,
    /// Name -> entry, for named slots and for every pool slot handed out.
    by_name: HashMap<&'static str, u32>,
    next_pool: usize,
    /// What [`GlesHost::select`] said it loaded, once it has.
    selected: Option<String>,
    requests: Vec<ProcRequest>,
    requests_dropped: usize,
    substitutions: Vec<Substitution>,
    substitutions_dropped: usize,
    /// The guest string pool: its region, how much is used, and what is already in it.
    strings: gl::StringPool,
    /// Live buffer mappings, by `(host context, buffer name)`.
    maps: BTreeMap<(u64, u32), gl::Mapping>,
    /// Window surfaces the host made, and their display.
    window_surfaces: BTreeMap<u64, u64>,
}

/// One guest's EGL and GLES: the slots, the host behind them, and the census.
///
/// Its own instance with its own activation, for [`Vulkan`](crate::Vulkan)'s reason: an
/// [`ImportFn`] is a bare `fn`, so per-instance state reaches a handler only through a guard held
/// across [`Boundary::run`](crate::Boundary::run) or published to a created guest thread.
pub struct Gles {
    space: Arc<GuestSpace>,
    table: OnceLock<Table>,
    state: Mutex<State>,
    presents: AtomicU64,
}

impl core::fmt::Debug for Gles {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Gles").field("presents", &self.presents()).finish_non_exhaustive()
    }
}

impl Gles {
    /// An instance with nothing bound. Takes no address space until a string or a mapping needs it.
    #[must_use]
    pub fn new(space: Arc<GuestSpace>) -> Arc<Self> {
        Arc::new(Self {
            space,
            table: OnceLock::new(),
            state: Mutex::new(State {
                host: None,
                by_name: HashMap::new(),
                next_pool: 0,
                selected: None,
                requests: Vec::new(),
                requests_dropped: 0,
                substitutions: Vec::new(),
                substitutions_dropped: 0,
                strings: gl::StringPool::default(),
                maps: BTreeMap::new(),
                window_surfaces: BTreeMap::new(),
            }),
            presents: AtomicU64::new(0),
        })
    }

    /// Attach the host EGL this instance forwards to. Last writer wins; nothing is migrated.
    ///
    /// **An instance with no host still binds**: every call refuses at its first use naming this
    /// method, rather than the import being unbound (which names nothing about GLES at all).
    pub fn set_host(&self, host: Arc<dyn GlesHost>) {
        self.state.lock().host = Some(host);
    }

    /// Bind every core ES 2.0-3.2 and EGL 1.0-1.5 command by name, and the `eglGetProcAddress`
    /// pool, into `builder`. Returns how many function slots that was ([`bound_symbol_count`]).
    ///
    /// Must run before the loader resolves the guest's imports: the 91 `egl*`/`gl*` imports then
    /// resolve to these slots instead of to unbound ones.
    ///
    /// # Errors
    ///
    /// [`AbiError::RegionFull`] if the region cannot hold them; refused if already bound.
    pub fn bind_into(&self, builder: &BoundaryBuilder) -> AbiResult<usize> {
        if self.table.get().is_some() {
            return Err(AbiError::Refused {
                symbol: "Gles::bind_into".to_string(),
                address: 0,
                why: "this GLES instance is already bound into a boundary; a second binding would \
                      leave it answering for two boundaries' thunk addresses"
                    .to_string(),
            });
        }
        let mut entries = Vec::new();
        let mut by_address = HashMap::new();
        let mut addresses = Vec::new();
        let mut by_name = HashMap::new();
        for signature in core_signatures() {
            let at = builder.bind_inline(signature.name, dispatch as ImportFn)?;
            let entry = Entry::new();
            let _ = entry.assigned.set(Assigned { signature, special: special_for(signature.name) });
            by_address.insert(at, entries.len() as u32);
            by_name.insert(signature.name, entries.len() as u32);
            addresses.push(at);
            entries.push(entry);
        }
        let named = entries.len();
        for index in 0..MAX_PROC_SLOTS {
            let at = builder.bind_inline(&proc_slot_symbol(index), dispatch as ImportFn)?;
            by_address.insert(at, entries.len() as u32);
            addresses.push(at);
            entries.push(Entry::new());
        }
        let count = entries.len();
        self.table
            .set(Table { entries, by_address, addresses, named })
            .map_err(|_| AbiError::Refused {
                symbol: "Gles::bind_into".to_string(),
                address: 0,
                why: "bound twice concurrently".to_string(),
            })?;
        self.state.lock().by_name = by_name;
        Ok(count)
    }

    /// Publish this instance to the calling thread until the guard is dropped.
    #[must_use]
    pub fn activate(self: &Arc<Self>) -> GlesActivation {
        let previous = ACTIVE.with(|cell| cell.borrow_mut().replace(Arc::clone(self)));
        GlesActivation { previous }
    }

    /// This instance as something a created guest thread carries -- the engine's GL renderer runs
    /// on a thread it spawned, not on the one the embedding calls from.
    #[must_use]
    pub fn thread_instance(self: &Arc<Self>) -> Arc<dyn crate::bionic::ThreadLocalInstance> {
        Arc::new(GlesThreadInstance(Arc::clone(self)))
    }

    /// The thunk bound for a core command, by name.
    #[must_use]
    pub fn thunk_for(&self, name: &str) -> Option<GuestAddr> {
        let table = self.table.get()?;
        let index = *self.state.lock().by_name.get(name)?;
        table.addresses.get(index as usize).copied()
    }

    /// Every `eglSwapBuffers` (and `...WithDamage`) the host answered `EGL_TRUE` for.
    #[must_use]
    pub fn presents(&self) -> u64 {
        self.presents.load(Ordering::Relaxed)
    }

    /// Every call made, by name -- **none dropped**: one atomic per slot.
    #[must_use]
    pub fn call_counts(&self) -> BTreeMap<&'static str, u64> {
        let mut out = BTreeMap::new();
        if let Some(table) = self.table.get() {
            for entry in &table.entries {
                let calls = entry.calls.load(Ordering::Relaxed);
                if let (Some(assigned), true) = (entry.assigned.get(), calls > 0) {
                    *out.entry(assigned.signature.name).or_insert(0) += calls;
                }
            }
        }
        out
    }

    /// Every `eglGetProcAddress`, oldest first, up to [`MAX_RECORDS`].
    #[must_use]
    pub fn requests(&self) -> Vec<ProcRequest> {
        self.state.lock().requests.clone()
    }

    /// Every substitution, oldest first.
    #[must_use]
    pub fn substitutions(&self) -> Vec<Substitution> {
        self.state.lock().substitutions.clone()
    }

    /// What the host said it loaded, once a call made it choose.
    #[must_use]
    pub fn selected(&self) -> Option<String> {
        self.state.lock().selected.clone()
    }

    /// The census as `GLES:` lines. Printed whether or not anything happened, so an empty census
    /// is a reading rather than a missing line.
    #[must_use]
    pub fn report(&self) -> String {
        let counts = self.call_counts();
        let state = self.state.lock();
        let mut out = String::new();
        let total: u64 = counts.values().sum();
        out.push_str(&format!(
            "GLES: host {}\n",
            state.selected.as_deref().unwrap_or("not selected (no EGL or GL call reached the host)")
        ));
        out.push_str(&format!(
            "GLES: {total} call(s) to {} entry point(s); {} present(s) (eglSwapBuffers answered \
             EGL_TRUE)\n",
            counts.len(),
            self.presents()
        ));
        let mut by_count: Vec<(&&str, &u64)> = counts.iter().collect();
        by_count.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        out.push_str(&format!(
            "GLES: calls by name: {}\n",
            by_count.iter().map(|(n, c)| format!("{n}={c}")).collect::<Vec<_>>().join(", ")
        ));
        out.push_str(&format!(
            "GLES: eglGetProcAddress asked {} time(s){}: {}\n",
            state.requests.len() + state.requests_dropped,
            if state.requests_dropped > 0 {
                format!(" ({} not kept)", state.requests_dropped)
            } else {
                String::new()
            },
            state
                .requests
                .iter()
                .map(|r| match r.answer {
                    ProcAnswer::Thunk(_) => r.name.clone(),
                    ProcAnswer::NullFromHost => format!("{} -> NULL (host)", r.name),
                    ProcAnswer::NullNotInRegistry => format!("{} -> NULL (not a GLES/EGL command)", r.name),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
        out.push_str(&format!(
            "GLES: {} substitution(s){}\n",
            state.substitutions.len(),
            if state.substitutions_dropped > 0 {
                format!(" ({} more not kept)", state.substitutions_dropped)
            } else {
                String::new()
            }
        ));
        for s in &state.substitutions {
            out.push_str(&format!("GLES:   {} x{}: {}\n", s.call, s.times, s.what));
        }
        out.push_str(&format!(
            "GLES: {} buffer mapping(s) live, {} window surface(s) live, {} byte(s) of guest \
             string pool used",
            state.maps.len(),
            state.window_surfaces.len(),
            state.strings.used()
        ));
        out
    }

    // ------------------------------------------------------------------------ internals

    fn require_host(&self, call: &Call) -> AbiResult<Arc<dyn GlesHost>> {
        self.state.lock().host.clone().ok_or_else(|| {
            call.refuse(
                "this GLES instance has no host EGL attached, so there is nothing to forward to. \
                 `Gles::set_host` is what an embedding calls with an implementation of \
                 `omni_android::gles::GlesHost` -- `omni_gfx::gles::GfxGlesHost` is the one this \
                 workspace ships. An EGL_FALSE here would blame a driver that was never asked"
                    .to_string(),
            )
        })
    }

    /// Make the host choose its EGL for the guest's window system, once.
    fn ensure_selected(&self, call: &Call) -> AbiResult<Arc<dyn GlesHost>> {
        let host = self.require_host(call)?;
        if self.state.lock().selected.is_some() {
            return Ok(host);
        }
        let window = egl::instance_window(call)?;
        let description = host.select(window)?;
        self.state.lock().selected = Some(description);
        Ok(host)
    }

    /// The host function behind `entry`, looked up once.
    fn host_proc(&self, call: &Call, entry: &Entry, name: &str) -> AbiResult<Option<HostProc>> {
        if let Some(found) = entry.host.get() {
            return Ok(*found);
        }
        let host = self.ensure_selected(call)?;
        let found = host.proc_address(name)?;
        let _ = entry.host.set(found);
        Ok(found)
    }

    /// Call the host's `name` with `lanes` in the registry's shape for it.
    ///
    /// For the special handlers, which call helpers (`glGetIntegerv`, `eglGetCurrentContext`, ...)
    /// with arguments of their own, some of them host pointers to locals.
    fn host_call(&self, call: &Call, name: &'static str, lanes: &[u64]) -> AbiResult<u64> {
        let table = self.table.get().ok_or_else(|| call.refuse(unbound()))?;
        let index = *self.state.lock().by_name.get(name).ok_or_else(|| {
            call.refuse(format!("this layer needs the host's `{name}` and it is not bound by name"))
        })?;
        let entry = &table.entries[index as usize];
        let signature = entry.assigned.get().expect("named entries are assigned at bind").signature;
        let proc = self.host_proc(call, entry, name)?.ok_or_else(|| {
            call.refuse(format!(
                "answering `{}` needs the host's `{name}`, and the host EGL/GLES has no such \
                 entry point",
                call.name
            ))
        })?;
        debug_assert_eq!(lanes.len(), signature.abi.len());
        let shape = &SHAPES[signature.shape as usize];
        // SAFETY: `proc` is the host function for `name` (`HostProc::new`'s contract), and `shape`
        // is the registry's shape for that same name.
        Ok(unsafe { (shape.call)(proc.address(), lanes) })
    }

    /// Forward the guest's call unchanged: the guest's arguments, the host's function, the host's
    /// answer.
    fn forward(&self, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
        let r = self.forward_value(call)?;
        write_return(c, call.signature, r);
        Ok(())
    }

    /// [`forward`](Gles::forward) without writing the return register.
    fn forward_value(&self, call: &Call) -> AbiResult<u64> {
        let table = self.table.get().ok_or_else(|| call.refuse(unbound()))?;
        let entry = &table.entries[call.entry as usize];
        let proc = self.host_proc(call, entry, call.name)?.ok_or_else(|| {
            call.refuse(format!(
                "the guest called `{name}` from {caller:#x}, and the host EGL/GLES ({host}) has no \
                 such entry point. `{name}` is {origin}, and a direct call to it has no NULL to \
                 test first -- answering anything would be inventing the function",
                name = call.name,
                caller = call.caller,
                host = self.selected().unwrap_or_default(),
                origin = call.signature.origin,
            ))
        })?;
        let n = call.signature.params.len();
        let shape = &SHAPES[call.signature.shape as usize];
        // SAFETY: as `host_call`: the host's function for this very name, in the registry's shape.
        Ok(unsafe { (shape.call)(proc.address(), &call.lanes[..n]) })
    }

    fn note(&self, call: &'static str, what: String) {
        let mut state = self.state.lock();
        if let Some(existing) = state.substitutions.iter_mut().find(|s| s.call == call && s.what == what)
        {
            existing.times += 1;
            return;
        }
        if state.substitutions.len() >= MAX_RECORDS {
            state.substitutions_dropped += 1;
            return;
        }
        state.substitutions.push(Substitution { call, what, times: 1 });
    }

    fn note_present(&self) {
        self.presents.fetch_add(1, Ordering::Relaxed);
    }

    fn space(&self) -> &Arc<GuestSpace> {
        &self.space
    }

    /// `eglGetProcAddress`'s decision, recorded. See [`ProcAnswer`].
    fn resolve(&self, call: &Call, name: &str) -> AbiResult<ProcAnswer> {
        let table = self.table.get().ok_or_else(|| call.refuse(unbound()))?;
        let answer = match signature(name) {
            None => ProcAnswer::NullNotInRegistry,
            Some(signature) => {
                let known = self.state.lock().by_name.get(signature.name).copied();
                match known {
                    Some(index) => {
                        let entry = &table.entries[index as usize];
                        match self.host_proc(call, entry, signature.name)? {
                            Some(_) => ProcAnswer::Thunk(table.addresses[index as usize]),
                            None => ProcAnswer::NullFromHost,
                        }
                    }
                    None => {
                        let host = self.ensure_selected(call)?;
                        match host.proc_address(signature.name)? {
                            None => ProcAnswer::NullFromHost,
                            Some(found) => {
                                ProcAnswer::Thunk(self.hand_out(call, table, signature, found)?)
                            }
                        }
                    }
                }
            }
        };
        let mut state = self.state.lock();
        if state.requests.len() < MAX_RECORDS {
            state.requests.push(ProcRequest { name: name.to_string(), answer, caller: call.caller });
        } else {
            state.requests_dropped += 1;
        }
        Ok(answer)
    }

    fn hand_out(
        &self,
        call: &Call,
        table: &Table,
        signature: &'static Signature,
        found: HostProc,
    ) -> AbiResult<GuestAddr> {
        let mut state = self.state.lock();
        if let Some(&index) = state.by_name.get(signature.name) {
            return Ok(table.addresses[index as usize]);
        }
        if state.next_pool >= MAX_PROC_SLOTS {
            return Err(call.refuse(format!(
                "the guest asked `eglGetProcAddress` for `{}`, and all {MAX_PROC_SLOTS} of this \
                 layer's procedure slots are handed out. That is a fact about this layer's pool \
                 (MAX_PROC_SLOTS, measured from the binary's strings), not about the host, so it \
                 is not answered NULL",
                signature.name
            )));
        }
        let index = table.named + state.next_pool;
        state.next_pool += 1;
        let entry = &table.entries[index];
        let _ = entry.assigned.set(Assigned { signature, special: special_for(signature.name) });
        let _ = entry.host.set(Some(found));
        state.by_name.insert(signature.name, index as u32);
        Ok(table.addresses[index])
    }

    fn state(&self) -> parking_lot::MutexGuard<'_, State> {
        self.state.lock()
    }
}

fn unbound() -> String {
    "this GLES instance was never bound into a boundary (`Gles::bind_into`)".to_string()
}

fn write_return(c: &mut ImportCall<'_, '_>, signature: &Signature, r: u64) {
    match signature.ret {
        b'V' => c.ret().void(),
        b'F' => c.ret().f32(f32::from_bits(r as u32)),
        _ => c.ret().u64(r),
    }
}

/// Which calls are not plain forwards. See the module documentation's table.
fn special_for(name: &str) -> Option<SpecialFn> {
    Some(match name {
        "eglGetDisplay" => egl::get_display,
        "eglGetProcAddress" => egl::get_proc_address,
        "eglQueryString" => egl::query_string,
        "eglChooseConfig" => egl::choose_config,
        "eglGetConfigAttrib" => egl::get_config_attrib,
        "eglCreateWindowSurface" => egl::create_window_surface,
        "eglDestroySurface" => egl::destroy_surface,
        "eglTerminate" => egl::terminate,
        "eglSwapBuffers" | "eglSwapBuffersWithDamageKHR" | "eglSwapBuffersWithDamageEXT" => {
            egl::swap_buffers
        }
        "eglGetPlatformDisplay"
        | "eglGetPlatformDisplayEXT"
        | "eglCreatePlatformWindowSurface"
        | "eglCreatePlatformWindowSurfaceEXT"
        | "eglCreatePixmapSurface"
        | "eglCreatePlatformPixmapSurface"
        | "eglCreatePlatformPixmapSurfaceEXT" => egl::refuse_native,
        "eglDebugMessageControlKHR"
        | "eglSetBlobCacheFuncsANDROID"
        | "glDebugMessageCallback"
        | "glDebugMessageCallbackKHR" => refuse_callback,
        "eglCreateNativeClientBufferANDROID" | "eglGetNativeClientBufferANDROID" => {
            egl::refuse_native
        }
        "glGetString" | "glGetStringi" => gl::get_string,
        "glMapBufferRange" | "glMapBufferRangeEXT" => gl::map_buffer_range,
        "glMapBufferOES" => gl::map_buffer_oes,
        "glUnmapBuffer" | "glUnmapBufferOES" => gl::unmap_buffer,
        "glFlushMappedBufferRange" | "glFlushMappedBufferRangeEXT" => gl::flush_mapped_range,
        "glGetBufferPointerv" | "glGetBufferPointervOES" => gl::get_buffer_pointer,
        _ => return None,
    })
}

/// The callback-taking entry points: the host would call guest ARM64 from its own threads, where
/// there is no guest CPU context -- and a NULL registration would silently discard what the engine
/// asked for.
fn refuse_callback(_: &Gles, _: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    Err(call.refuse(format!(
        "the guest called `{name}` from {caller:#x} with the arguments ({callback}), one of them a guest function. The \
         host would call it from inside its own driver, possibly on a thread of its own, and \
         translated ARM64 can be entered only through this runtime's guest CPU. Passing NULL \
         instead would discard a callback the engine asked for, and pretending success would \
         leave it waiting for calls that never come",
        name = call.name,
        caller = call.caller,
        callback = call.lanes[..call.signature.params.len()]
            .iter()
            .map(|lane| format!("{lane:#x}"))
            .collect::<Vec<_>>()
            .join(", "),
    )))
}

// ------------------------------------------------------------------------------ the handler

/// Every slot lands here. The slot's address says which command it is; the registry says where
/// each argument is; the command is either a special or a forward.
fn dispatch(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let address = c.address();
    let gles = active(c.symbol(), address)?;
    let Some(table) = gles.table.get() else {
        return Err(AbiError::Refused {
            symbol: c.symbol().to_string(),
            address,
            why: "the GLES instance published to this thread is not the one this slot was bound \
                  by: it has no table"
                .to_string(),
        });
    };
    let Some(&index) = table.by_address.get(&address) else {
        return Err(AbiError::Refused {
            symbol: c.symbol().to_string(),
            address,
            why: "the GLES instance published to this thread did not bind this slot -- two \
                  boundaries' instances were mixed"
                .to_string(),
        });
    };
    let entry = &table.entries[index as usize];
    entry.calls.fetch_add(1, Ordering::Relaxed);
    let Some(assigned) = entry.assigned.get().copied() else {
        return Err(AbiError::Refused {
            symbol: c.symbol().to_string(),
            address,
            why: format!(
                "the guest branched to {address:#x} from {caller:#x}, a GLES procedure slot this \
                 instance never handed out. Every such address comes from `eglGetProcAddress`, \
                 so this is a pointer the guest computed",
                caller = c.caller()
            ),
        });
    };
    let signature = assigned.signature;
    let mut lanes = [0u64; MAX_ARGS];
    {
        let mut args = c.args();
        for (lane, class) in lanes.iter_mut().zip(signature.params.bytes()) {
            *lane = match class {
                b'F' => u64::from(args.next_f32()?.to_bits()),
                b'D' => args.next_f64()?.to_bits(),
                _ => args.next_u64()?,
            };
        }
    }
    let call = Call {
        name: signature.name,
        signature,
        lanes,
        address,
        caller: c.caller(),
        entry: index,
    };
    match assigned.special {
        Some(special) => special(&gles, c, &call),
        None => gles.forward(c, &call),
    }
}

// ----------------------------------------------------------------------------- the activation

thread_local! {
    // The `Gles` published to one thread, for `Gles`'s own documented reason.
    static ACTIVE: RefCell<Option<Arc<Gles>>> = const { RefCell::new(None) };
}

/// Restores the previously published instance when dropped.
pub struct GlesActivation {
    previous: Option<Arc<Gles>>,
}

impl Drop for GlesActivation {
    fn drop(&mut self) {
        ACTIVE.with(|cell| *cell.borrow_mut() = self.previous.take());
    }
}

impl core::fmt::Debug for GlesActivation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GlesActivation")
    }
}

/// The instance published to this thread, or a refusal naming the call.
pub(crate) fn active(symbol: &str, address: GuestAddr) -> AbiResult<Arc<Gles>> {
    ACTIVE.with(|cell| cell.borrow().clone()).ok_or_else(|| AbiError::Refused {
        symbol: symbol.to_string(),
        address,
        why: "no GLES instance is published to this thread. `Gles::activate` publishes one for as \
              long as its guard is held, and a created guest thread carries one only if \
              `Gles::thread_instance` was given to the `ThreadHost` -- the engine's renderer runs \
              on a thread it spawned"
            .to_string(),
    })
}

struct GlesThreadInstance(Arc<Gles>);

impl crate::bionic::ThreadLocalInstance for GlesThreadInstance {
    fn name(&self) -> &'static str {
        "Gles"
    }

    fn publish(&self) -> AbiResult<Box<dyn core::any::Any>> {
        Ok(Box::new(self.0.activate()))
    }
}

#[cfg(test)]
mod tests;
