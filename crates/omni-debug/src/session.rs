//! The lab session: one library loaded on the translating backend, driven from a single guest
//! thread.
//!
//! Everything a caller can do — resolve a symbol, read or write guest memory, dump a module, call a
//! function with crafted inputs, set breakpoints, intercept entry/exit, replace a return value,
//! trace calls and syscalls — is expressed against this one type. There is no OS access here; the
//! session is identical on Windows, Linux and macOS because the guest it drives is arm64 on all
//! three (D5).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use omni_cpu::dynarmic::{DynarmicBackend, DynarmicCpu, DynarmicOptions};
use omni_cpu::{ExitReason, GuestCpu, RunLimit, ThunkCall, ThunkContext, VReg, XReg};
use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::ElfImage;
use omni_mem::{
    Backing, CommitPolicy, GuestAddr, GuestSpace, GuestSpaceConfig, MapExecutability, Placement,
    Protection, RegionKind,
};

use crate::{DebugError, Result};

/// How many guest instructions a single [`call_function`](Session::call_function) may run before it
/// is declared non-terminating. Untrusted guest code (Global Constraint 11) can loop forever; this
/// bounds it. Generous: a whole-buffer checksum of a megabyte is a few million instructions.
pub const DEFAULT_CALL_BUDGET: u64 = 200_000_000;

/// Bytes of guest stack the driven thread runs on.
const STACK_BYTES: usize = 1 << 20;

/// One symbol, resolved to a guest address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    /// The symbol name as it appears in `.dynstr`.
    pub name: String,
    /// The guest address it resolves to: the module's load bias plus `st_value`.
    pub address: GuestAddr,
    /// `st_size`.
    pub size: u64,
    /// `"func"`, `"object"`, `"ifunc"`, ... from `st_info`.
    pub kind: &'static str,
    /// The module the symbol is defined in.
    pub module: String,
}

/// One line of the guest memory map, shaped like a `/proc/self/maps` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEntry {
    /// Start address.
    pub start: GuestAddr,
    /// End address, exclusive.
    pub end: GuestAddr,
    /// `r`, `w`, `x` in the usual places, `-` otherwise.
    pub perms: [u8; 3],
    /// Bytes of private commit charge this region holds.
    pub committed: usize,
    /// What is mapped: `"file"`, `"anon"`, `"host"`, or the module name when it belongs to one.
    pub what: String,
}

impl MapEntry {
    /// The permission string, e.g. `"r-x"`.
    #[must_use]
    pub fn perms_str(&self) -> String {
        String::from_utf8_lossy(&self.perms).into_owned()
    }
}

/// A snapshot of the guest register file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registers {
    /// `X0`..`X30`.
    pub x: [u64; 31],
    /// Stack pointer.
    pub sp: GuestAddr,
    /// Program counter.
    pub pc: GuestAddr,
    /// The four condition flags, as an `NZCV` pstate word (top nibble).
    pub nzcv: u32,
    /// The bionic thread pointer.
    pub tpidr_el0: GuestAddr,
    /// `V0`..`V31`, full 128 bits each.
    pub v: [u128; 32],
}

/// Why a driven run stopped: either the guest returned through the sentinel, or it reached a user
/// breakpoint set for interactive stepping. Watches (intercept/trace) never end a run — they are
/// serviced inline — so they are not a variant here.
#[derive(Debug, Clone)]
pub enum Stop {
    /// The guest returned. Carries the full outcome.
    Returned(CallOutcome),
    /// A user breakpoint was reached; the instruction there has **not** run. Registers and
    /// [`backtrace`](Session::backtrace) are valid at this point; [`resume`](Session::resume)
    /// continues past it.
    Breakpoint {
        /// The breakpoint address (also the current `PC`).
        address: GuestAddr,
        /// Events recorded since the run began.
        events: Vec<TraceEvent>,
    },
}

/// The result of calling a guest function.
#[derive(Debug, Clone)]
pub struct CallOutcome {
    /// `X0` at return: the AAPCS64 integer/pointer result.
    pub ret: u64,
    /// `X1` at return, for a function returning a 128-bit value or a second word.
    pub ret1: u64,
    /// How many guest instructions the call executed.
    pub instructions: u64,
    /// Any trace/intercept/syscall events recorded during the call, in order.
    pub events: Vec<TraceEvent>,
}

/// What kind of event a [`TraceEvent`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceKind {
    /// Control reached a traced or intercepted function's entry.
    Enter,
    /// Control returned from an intercepted function (its exit hook).
    Exit,
    /// A guest `SVC` was observed.
    Syscall,
    /// An intercepted function's return value was replaced before its body ran.
    Replaced,
}

/// One thing that happened while guest code ran under observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceEvent {
    /// Which kind.
    pub kind: TraceKind,
    /// The guest address the event is about: the function entry, the return site, or the `SVC`.
    pub address: GuestAddr,
    /// For [`TraceKind::Enter`]/[`TraceKind::Syscall`]: `X0`..`X7` (and for a syscall, `X8` is the
    /// number). For [`TraceKind::Exit`]/[`TraceKind::Replaced`]: `X0` is the return value.
    pub regs: [u64; 8],
    /// For a syscall, the syscall number (`X8`); otherwise 0.
    pub syscall_nr: u64,
}

/// What to do when an intercepted function is entered. See [`Session::intercept`].
#[derive(Debug, Clone, Copy)]
pub struct HookAction {
    /// Record a [`TraceKind::Enter`] event with the arguments.
    pub record_entry: bool,
    /// Record a [`TraceKind::Exit`] event with the return value (a one-shot breakpoint is planted
    /// at the caller's return address to catch it).
    pub record_exit: bool,
    /// If set, the body never runs: `X0` is set to this and control returns to the caller at once,
    /// and a [`TraceKind::Replaced`] event is recorded.
    pub replace_return: Option<u64>,
}

impl Default for HookAction {
    fn default() -> Self {
        Self { record_entry: true, record_exit: true, replace_return: None }
    }
}

/// A disassembly-adjacent view: the raw instruction words at an address. Kept deliberately small —
/// this crate does not carry a disassembler; it hands back the bytes a caller (or the agent above)
/// can decode, which is enough to confirm a breakpoint or a patch landed where it was meant to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disassembly {
    /// The address the words start at.
    pub address: GuestAddr,
    /// The 32-bit A64 encodings, in order.
    pub words: Vec<u32>,
}

/// A loaded module, with its exports resolved to guest addresses.
struct Module {
    name: String,
    base: GuestAddr,
    start: GuestAddr,
    end: GuestAddr,
    exports: BTreeMap<String, SymbolInfo>,
    _backing: Arc<Backing>,
}

/// A function marked for observation during a driven run.
#[derive(Clone, Copy)]
struct Watch {
    action: HookAction,
}

/// The per-process store of syscall events, keyed by a session token handed to the `SVC` handler as
/// its [`ThunkContext`]. A bare `fn` handler cannot capture, so the token is how it finds its
/// session's log; the map itself is safe shared state, never a raw pointer.
fn syscall_log() -> &'static Mutex<BTreeMap<usize, Vec<TraceEvent>>> {
    static LOG: OnceLock<Mutex<BTreeMap<usize, Vec<TraceEvent>>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(BTreeMap::new()))
}

static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);

/// The lab debug session.
pub struct Session {
    space: Arc<GuestSpace>,
    #[allow(dead_code)]
    backend: DynarmicBackend,
    cpu: DynarmicCpu,
    modules: Vec<Module>,
    stack_top: GuestAddr,
    sentinel: GuestAddr,
    breakpoints: BTreeSet<GuestAddr>,
    watches: BTreeMap<GuestAddr, Watch>,
    trace_syscalls: bool,
    token: usize,
    call_budget: u64,
}

impl Session {
    /// Create an empty session: a fresh guest address space, a translating backend with a
    /// **per-thread** code cache (so breakpoints are allowed — a shared cache refuses them, D38),
    /// one guest thread with a bionic TLS block (D13), a stack and a return sentinel.
    ///
    /// # Errors
    ///
    /// [`DebugError::Cpu`] or [`DebugError::Mem`] if the space, backend, thread, stack or sentinel
    /// could not be set up.
    pub fn new() -> Result<Self> {
        let space = Arc::new(high_guest_space()?);

        // Per-thread cache, not the x64 default shared one: a breakpoint for this thread cannot be
        // planted in code every thread shares (D38), and the whole point of the lab is to plant
        // them. INTERRUPTIBLE stays on so a runaway guest is stoppable (Global Constraint 11).
        let options = DynarmicOptions { shared_code_cache: false, ..DynarmicOptions::default() };
        let backend = DynarmicBackend::new(Arc::clone(&space), options)?;
        let mut cpu = backend.create_thread_with_tls()?;

        let page = space.page_size();
        let stack_base = space.map_anonymous(
            Placement::Anywhere { align: page },
            STACK_BYTES,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )?;
        let stack_top = (stack_base + STACK_BYTES) & !0xF;

        // A page of its own for the sentinel, so the address the guest's `RET` lands on is one
        // nothing else owns. It never executes — `run` reports `Returned` before fetching there.
        let sentinel = space.map_anonymous(
            Placement::Anywhere { align: page },
            page,
            Protection::ReadExecute,
            CommitPolicy::Eager,
        )?;
        cpu.set_return_sentinel(sentinel)?;

        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);

        Ok(Self {
            space,
            backend,
            cpu,
            modules: Vec::new(),
            stack_top,
            sentinel,
            breakpoints: BTreeSet::new(),
            watches: BTreeMap::new(),
            trace_syscalls: false,
            token,
            call_budget: DEFAULT_CALL_BUDGET,
        })
    }

    /// A session with one library already loaded, named by its file stem.
    ///
    /// # Errors
    ///
    /// As [`new`](Session::new) and [`load`](Session::load).
    pub fn with_library(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "module".to_string());
        let mut session = Self::new()?;
        session.load(&name, path)?;
        Ok(session)
    }

    /// Load a library into the session under `name`, applying every relocation and sealing RELRO
    /// exactly as the production loader does, and resolving its exports to guest addresses.
    ///
    /// Imports are left unresolved (bound to null): the lab exercises a library's own code with
    /// crafted inputs, and a function that reaches an unresolved import stops with
    /// [`ExitReason::Thunk`], reported rather than papered over.
    ///
    /// # Errors
    ///
    /// [`DebugError::Elf`] if the file is not a loadable AArch64 `ET_DYN`, or [`DebugError::Mem`]/
    /// [`DebugError::Cpu`] if it could not be mapped.
    pub fn load(&mut self, name: &str, path: impl AsRef<Path>) -> Result<&SymbolInfo> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| {
            DebugError::BadRequest(format!("cannot read {}: {e}", path.display()))
        })?;
        self.load_bytes(name, path, &bytes)
    }

    fn load_bytes(&mut self, name: &str, path: &Path, bytes: &[u8]) -> Result<&SymbolInfo> {
        let elf = ElfImage::parse(bytes)?;
        let backing = Backing::open(path, MapExecutability::Executable)?;
        let object = loader::load(
            &self.space,
            &backing,
            &elf,
            &ProviderRegistry::empty_provider(),
            &LoaderConfig { name: Some(name.to_string()), ..LoaderConfig::default() },
        )?;

        let mut exports = BTreeMap::new();
        for sym in elf.exported_symbols()? {
            let info = SymbolInfo {
                name: sym.name.to_string(),
                address: object.base + sym.sym.st_value as usize,
                size: sym.sym.st_size,
                kind: sym.sym.type_name(),
                module: name.to_string(),
            };
            exports.insert(sym.name.to_string(), info);
        }

        // Any code the loader wrote (relocations) into what will run must be seen as fresh by the
        // translator, which never fetched it yet — harmless here, load-bearing once a module is
        // reloaded over an old one.
        if let Ok(range) = omni_cpu::GuestRange::new(object.start, object.end - object.start) {
            let _ = self.cpu.invalidate_code(range);
        }

        let module = Module {
            name: name.to_string(),
            base: object.base,
            start: object.start,
            end: object.end,
            exports,
            _backing: backing,
        };
        self.modules.push(module);
        // Return a stable reference to a representative symbol is awkward; hand back the module's
        // first export so the caller has something, or synthesize a module marker.
        let m = self.modules.last().unwrap();
        Ok(m.exports.values().next().unwrap_or_else(|| {
            // A library with no exports is legal but useless for the lab; still, don't panic.
            unreachable!("every real library exports at least one symbol")
        }))
    }

    /// The names of the loaded modules.
    #[must_use]
    pub fn modules(&self) -> Vec<String> {
        self.modules.iter().map(|m| m.name.clone()).collect()
    }

    /// The load bias, start and end of a loaded module.
    ///
    /// # Errors
    ///
    /// [`DebugError::NoSuchModule`] if nothing is loaded under that name.
    pub fn module_span(&self, name: &str) -> Result<(GuestAddr, GuestAddr, GuestAddr)> {
        let m = self.module(name)?;
        Ok((m.base, m.start, m.end))
    }

    fn module(&self, name: &str) -> Result<&Module> {
        self.modules
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| DebugError::NoSuchModule(name.to_string()))
    }

    /// Resolve a symbol to a guest address, searching every loaded module (first match wins).
    ///
    /// # Errors
    ///
    /// [`DebugError::NoSuchSymbol`] if no loaded module exports it.
    pub fn resolve_symbol(&self, name: &str) -> Result<SymbolInfo> {
        for m in &self.modules {
            if let Some(info) = m.exports.get(name) {
                return Ok(info.clone());
            }
        }
        Err(DebugError::NoSuchSymbol(name.to_string()))
    }

    /// Every export of a module, ascending by address.
    ///
    /// # Errors
    ///
    /// [`DebugError::NoSuchModule`] if nothing is loaded under that name.
    pub fn list_symbols(&self, module: &str) -> Result<Vec<SymbolInfo>> {
        let m = self.module(module)?;
        let mut out: Vec<SymbolInfo> = m.exports.values().cloned().collect();
        out.sort_by_key(|s| s.address);
        Ok(out)
    }

    /// The guest memory map, one row per region a guest would see in `/proc/self/maps`.
    #[must_use]
    pub fn list_maps(&self) -> Vec<MapEntry> {
        let module_of = |addr: GuestAddr| -> Option<&str> {
            self.modules
                .iter()
                .find(|m| addr >= m.start && addr < m.end)
                .map(|m| m.name.as_str())
        };
        self.space
            .mapped_regions()
            .into_iter()
            .map(|r| {
                let (rd, wr, ex) = perms_of(r.protection);
                let what = match r.kind {
                    RegionKind::File { .. } => module_of(r.start)
                        .map(str::to_string)
                        .unwrap_or_else(|| "file".to_string()),
                    RegionKind::Anonymous => module_of(r.start)
                        .map(str::to_string)
                        .unwrap_or_else(|| "anon".to_string()),
                    RegionKind::Host => "host".to_string(),
                    RegionKind::Free => "free".to_string(),
                };
                MapEntry {
                    start: r.start,
                    end: r.start + r.len,
                    perms: [rd, wr, ex],
                    committed: r.committed,
                    what,
                }
            })
            .collect()
    }

    /// Read `len` bytes of guest memory starting at `address`. Holes and uncommitted granules read
    /// as zero, exactly as they are; nothing is committed by reading.
    ///
    /// # Errors
    ///
    /// [`DebugError::BadRequest`] for a zero length or a range that leaves the address space.
    pub fn read_mem(&self, address: GuestAddr, len: usize) -> Result<Vec<u8>> {
        if len == 0 {
            return Err(DebugError::BadRequest("read of zero length".into()));
        }
        let mut out = vec![0u8; len];
        // `held_ranges` gives exactly the sub-ranges that hold committed private memory or file
        // views; the rest is genuinely zero. So only those are copied.
        for (start, hlen) in self.space.held_ranges(address, len) {
            let offset = start - address;
            let ptr = self.space.ptr(start, hlen)?;
            // SAFETY: `held_ranges` reported `[start, start+hlen)` as backed by committed memory
            // this space owns, `ptr` checked it is inside the space, and identity mapping (D4) makes
            // it a real host pointer. No guest thread runs concurrently (the session is not `Sync`
            // and holds `&self`), so the bytes cannot change under the copy.
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, out[offset..offset + hlen].as_mut_ptr(), hlen);
            }
        }
        Ok(out)
    }

    /// The 32-bit A64 instruction words at `address` — `count` of them. A convenience over
    /// [`read_mem`](Session::read_mem) for confirming a breakpoint or patch landed.
    ///
    /// # Errors
    ///
    /// As [`read_mem`](Session::read_mem).
    pub fn read_words(&self, address: GuestAddr, count: usize) -> Result<Disassembly> {
        let bytes = self.read_mem(address, count * 4)?;
        let words = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Ok(Disassembly { address, words })
    }

    /// Write `bytes` into guest memory at `address`, **including into read-only code**.
    ///
    /// A page that is not already writable is raised to writable for the duration of the write and
    /// restored afterwards; on a file-backed page that raise is a copy-on-write into private memory
    /// (D11), so the file on disk is never touched and other instances that share it are unaffected.
    /// The translator is then told the bytes changed ([`GuestCpu::invalidate_code`]) so a patched
    /// instruction really takes effect rather than the stale translation running on.
    ///
    /// # Errors
    ///
    /// [`DebugError::BadRequest`] for empty input or an out-of-range address; [`DebugError::Mem`]
    /// if a page could not be made writable or restored.
    pub fn write_mem(&mut self, address: GuestAddr, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Err(DebugError::BadRequest("write of zero length".into()));
        }
        let page = self.space.page_size();
        let end = address
            .checked_add(bytes.len())
            .ok_or_else(|| DebugError::BadRequest("write range wraps the address space".into()))?;

        // Walk the pages the write touches, restoring each page's protection after. A page may be
        // shared with the next region; operating per page keeps the restore exact.
        let first_page = address & !(page - 1);
        let mut p = first_page;
        let mut restores: Vec<(GuestAddr, Protection)> = Vec::new();
        while p < end {
            let region = self
                .space
                .region_at(p)
                .ok_or_else(|| DebugError::BadRequest(format!("write into unmapped page {p:#x}")))?;
            let (_, wr, _) = perms_of(region.protection);
            if wr != b'w' {
                self.space.protect(p, page, Protection::ReadWrite)?;
                restores.push((p, region.protection));
            }
            p += page;
        }

        let ptr = self.space.ptr(address, bytes.len())?;
        // SAFETY: every page of `[address, end)` was just confirmed mapped and made writable above,
        // `ptr` checked the range is inside the space, and identity mapping (D4) makes it a real
        // host pointer. The session is not `Sync`, so no guest thread writes these bytes concurrently.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        }

        for (addr, prot) in restores {
            self.space.protect(addr, page, prot)?;
        }

        // The write may have changed code. Invalidate the whole touched span so a stale translation
        // cannot run on. Cheap, and correctness beats a narrower range here.
        if let Ok(range) = omni_cpu::GuestRange::new(first_page, (end - first_page).max(4)) {
            self.cpu.invalidate_code(range)?;
        }
        Ok(())
    }

    /// Dump a loaded module out of guest memory: the bytes as they sit in the address space, after
    /// relocation (and, for a packed library, after it has unpacked itself). This is the primitive
    /// that turns a self-decrypting `.so` into a file on disk once the guest has run its unpacker.
    ///
    /// The dump is the contiguous span `[start, end)` of the loaded object; holes between segments
    /// read as zero. The ELF header and program headers are present at the front, so the result is
    /// recognisable as — and, for an unpacked image, close to — a loadable object.
    ///
    /// # Errors
    ///
    /// [`DebugError::NoSuchModule`] if nothing is loaded under that name.
    pub fn dump_module(&self, name: &str) -> Result<Vec<u8>> {
        let m = self.module(name)?;
        self.read_mem(m.start, m.end - m.start)
    }

    /// Set a breakpoint at a guest address. Execution stops **before** the instruction there runs.
    ///
    /// # Errors
    ///
    /// [`DebugError::Cpu`] if the backend refused it (e.g. a shared code cache, which this session
    /// never uses).
    pub fn set_breakpoint(&mut self, address: GuestAddr) -> Result<()> {
        self.cpu.add_breakpoint(address)?;
        self.breakpoints.insert(address);
        Ok(())
    }

    /// Remove a breakpoint. Returns whether there was one.
    ///
    /// # Errors
    ///
    /// [`DebugError::Cpu`] if the backend could not remove it.
    pub fn clear_breakpoint(&mut self, address: GuestAddr) -> Result<bool> {
        let had = self.cpu.remove_breakpoint(address)?;
        self.breakpoints.remove(&address);
        Ok(had)
    }

    /// Intercept a function: observe its entry and/or exit, or replace its return value so the body
    /// never runs. The interception takes effect during [`call_function`](Session::call_function)
    /// (the driven run loop is what services it).
    ///
    /// # Errors
    ///
    /// [`DebugError::Cpu`] if the entry breakpoint could not be planted.
    pub fn intercept(&mut self, address: GuestAddr, action: HookAction) -> Result<()> {
        self.cpu.add_breakpoint(address)?;
        self.watches.insert(address, Watch { action });
        Ok(())
    }

    /// Stop intercepting a function.
    ///
    /// # Errors
    ///
    /// [`DebugError::Cpu`] if the breakpoint could not be removed.
    pub fn clear_intercept(&mut self, address: GuestAddr) -> Result<bool> {
        let was = self.watches.remove(&address).is_some();
        if was && !self.breakpoints.contains(&address) {
            self.cpu.remove_breakpoint(address)?;
        }
        Ok(was)
    }

    /// Mark a set of functions to be traced (an [`HookAction`] recording entry only) during the
    /// next driven run.
    ///
    /// # Errors
    ///
    /// As [`intercept`](Session::intercept).
    pub fn trace_calls(&mut self, addresses: &[GuestAddr]) -> Result<()> {
        for &a in addresses {
            self.intercept(
                a,
                HookAction { record_entry: true, record_exit: false, replace_return: None },
            )?;
        }
        Ok(())
    }

    /// Record every guest `SVC` executed during the next driven run. In the lab there is no kernel,
    /// so a traced syscall is observed and the guest simply carries on; the value is the record.
    ///
    /// # Errors
    ///
    /// [`DebugError::Cpu`] if the backend has no in-loop supervisor-call hook.
    pub fn trace_syscalls(&mut self, on: bool) -> Result<()> {
        if on {
            self.cpu.set_svc_handler(record_syscall, ThunkContext(self.token))?;
            syscall_log().lock().unwrap().entry(self.token).or_default();
        }
        self.trace_syscalls = on;
        Ok(())
    }

    /// Call a guest function at `address` with up to eight integer/pointer arguments (`X0`..`X7`),
    /// returning its result and any events recorded along the way.
    ///
    /// This is the lever the whole crate exists for: a library's function is exercised with crafted
    /// inputs directly, with no game, login or anti-cheat path around it. Breakpoints, interception,
    /// `replace_return` and syscall tracing all take effect here, because this is the loop that
    /// drives the guest.
    ///
    /// # Errors
    ///
    /// [`DebugError::BadRequest`] for more than eight arguments; [`DebugError::DidNotReturn`] if the
    /// guest faulted, executed an unsupported instruction, called an unresolved import, or ran past
    /// the instruction budget without returning; [`DebugError::Cpu`] on a backend failure.
    pub fn call_function(&mut self, address: GuestAddr, args: &[u64]) -> Result<CallOutcome> {
        if args.len() > 8 {
            return Err(DebugError::BadRequest(format!(
                "call_function takes at most 8 register arguments, got {}",
                args.len()
            )));
        }

        // Set up the call frame: args in X0..Xn, LR at the sentinel, SP at the stack top.
        for (i, &v) in args.iter().enumerate() {
            self.cpu.set_x(XReg::new(i as u8)?, v);
        }
        self.cpu.set_sp(self.stack_top);
        self.cpu.set_x(XReg::new(30)?, self.sentinel as u64);

        if self.trace_syscalls {
            // Fresh log for this call.
            syscall_log().lock().unwrap().insert(self.token, Vec::new());
        }

        // No stop set: user breakpoints are stepped over and observed, not paused on. call_function
        // is "run it and give me the answer"; use run_until_stop/resume to step interactively.
        let empty = BTreeSet::new();
        match drive(
            &mut self.cpu,
            &self.space,
            address,
            self.sentinel,
            &self.watches,
            &empty,
            self.call_budget,
            self.trace_syscalls,
            self.token,
        )? {
            Stop::Returned(outcome) => Ok(outcome),
            Stop::Breakpoint { address, .. } => Err(DebugError::DidNotReturn(format!(
                "stopped at breakpoint {address:#x} (call_function does not pause on breakpoints)"
            ))),
        }
    }

    /// Start guest execution at `address` with up to eight arguments and run until the guest
    /// returns or a **user breakpoint** ([`set_breakpoint`](Session::set_breakpoint)) is reached.
    /// This is the interactive counterpart to [`call_function`](Session::call_function): on a
    /// breakpoint stop, inspect [`get_registers`](Session::get_registers)/
    /// [`backtrace`](Session::backtrace), then [`resume`](Session::resume).
    ///
    /// # Errors
    ///
    /// As [`call_function`](Session::call_function).
    pub fn run_until_stop(&mut self, address: GuestAddr, args: &[u64]) -> Result<Stop> {
        if args.len() > 8 {
            return Err(DebugError::BadRequest(format!(
                "run_until_stop takes at most 8 register arguments, got {}",
                args.len()
            )));
        }
        for (i, &v) in args.iter().enumerate() {
            self.cpu.set_x(XReg::new(i as u8)?, v);
        }
        self.cpu.set_sp(self.stack_top);
        self.cpu.set_x(XReg::new(30)?, self.sentinel as u64);
        if self.trace_syscalls {
            syscall_log().lock().unwrap().insert(self.token, Vec::new());
        }
        drive(
            &mut self.cpu,
            &self.space,
            address,
            self.sentinel,
            &self.watches,
            &self.breakpoints,
            self.call_budget,
            self.trace_syscalls,
            self.token,
        )
    }

    /// Continue a run stopped at a breakpoint: step past the current instruction and drive on until
    /// the next breakpoint or a return.
    ///
    /// # Errors
    ///
    /// [`DebugError::BadRequest`] if the guest is not stopped at one of this session's breakpoints;
    /// otherwise as [`call_function`](Session::call_function).
    pub fn resume(&mut self) -> Result<Stop> {
        let at = self.cpu.pc();
        if !self.breakpoints.contains(&at) {
            return Err(DebugError::BadRequest(format!(
                "resume: the guest is not stopped at a breakpoint (PC {at:#x})"
            )));
        }
        // Driving from the breakpoint address makes the backend suppress it for one fetch, execute
        // the instruction under it, and continue — the resume-past-a-breakpoint step.
        drive_from(
            &mut self.cpu,
            &self.space,
            at,
            self.sentinel,
            &self.watches,
            &self.breakpoints,
            self.call_budget,
            self.trace_syscalls,
            self.token,
        )
    }

    /// A full register snapshot of the driven thread as it stands now (after the most recent call or
    /// at a breakpoint stop).
    #[must_use]
    pub fn get_registers(&self) -> Registers {
        let mut x = [0u64; 31];
        for (i, slot) in x.iter_mut().enumerate() {
            *slot = self.cpu.x(XReg::new(i as u8).unwrap());
        }
        let mut v = [0u128; 32];
        for (i, slot) in v.iter_mut().enumerate() {
            *slot = self.cpu.v(VReg::new(i as u8).unwrap());
        }
        Registers {
            x,
            sp: self.cpu.sp(),
            pc: self.cpu.pc(),
            nzcv: (self.cpu.nzcv().to_pstate() as u32),
            tpidr_el0: self.cpu.tpidr_el0(),
            v,
        }
    }

    /// Walk the AArch64 frame-pointer chain from the current state, returning return addresses from
    /// innermost to outermost. The current `PC` is first, then `LR`, then each saved `LR` up the
    /// `X29` chain. Stops at a frame pointer that leaves the stack or fails to advance.
    #[must_use]
    pub fn backtrace(&self) -> Vec<GuestAddr> {
        let mut out = vec![self.cpu.pc()];
        let lr = self.cpu.x(XReg::new(30).unwrap()) as GuestAddr;
        if lr != 0 && lr != self.sentinel {
            out.push(lr);
        }
        let mut fp = self.cpu.x(XReg::new(29).unwrap()) as GuestAddr;
        let mut guard = 0;
        while fp != 0 && guard < 64 {
            guard += 1;
            let Ok(frame) = self.read_mem(fp, 16) else { break };
            let saved_fp = u64::from_le_bytes(frame[0..8].try_into().unwrap()) as GuestAddr;
            let saved_lr = u64::from_le_bytes(frame[8..16].try_into().unwrap()) as GuestAddr;
            if saved_lr == 0 || saved_lr == self.sentinel {
                break;
            }
            out.push(saved_lr);
            if saved_fp <= fp {
                break; // the chain must climb; a non-increasing FP is the end (or corruption)
            }
            fp = saved_fp;
        }
        out
    }

    /// Set the instruction budget for a single [`call_function`](Session::call_function).
    pub fn set_call_budget(&mut self, instructions: u64) {
        self.call_budget = instructions;
    }

    /// Place `bytes` in a fresh writable guest region and return its address. This is how a caller
    /// hands crafted input to [`call_function`](Session::call_function): allocate a buffer, then
    /// pass its address as an argument.
    ///
    /// # Errors
    ///
    /// [`DebugError::Mem`] if the region could not be mapped or written.
    pub fn alloc_data(&mut self, bytes: &[u8]) -> Result<GuestAddr> {
        let page = self.space.page_size();
        let size = bytes.len().max(1).next_multiple_of(page);
        let addr = self.space.map_anonymous(
            Placement::Anywhere { align: page },
            size,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )?;
        if !bytes.is_empty() {
            self.write_mem(addr, bytes)?;
        }
        Ok(addr)
    }

    /// Place a snippet of A64 machine code (32-bit words) in a fresh read-execute guest region and
    /// return its entry address, ready for [`call_function`](Session::call_function). This is what
    /// lets an agent exercise a hand-crafted sequence — a decryptor stub, a single instruction under
    /// test — with no library needed at all.
    ///
    /// # Errors
    ///
    /// [`DebugError::BadRequest`] for empty input; [`DebugError::Mem`] if the region could not be
    /// mapped.
    pub fn load_code(&mut self, words: &[u32]) -> Result<GuestAddr> {
        if words.is_empty() {
            return Err(DebugError::BadRequest("load_code needs at least one instruction".into()));
        }
        let page = self.space.page_size();
        let size = (words.len() * 4).next_multiple_of(page);
        let addr = self.space.map_anonymous(
            Placement::Anywhere { align: page },
            size,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )?;
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        self.write_mem(addr, &bytes)?;
        self.space.protect(addr, size, Protection::ReadExecute)?;
        if let Ok(range) = omni_cpu::GuestRange::new(addr, size) {
            self.cpu.invalidate_code(range)?;
        }
        Ok(addr)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        syscall_log().lock().unwrap().remove(&self.token);
    }
}

/// The `SVC` handler installed by [`Session::trace_syscalls`]. A bare `fn`: it finds its session's
/// log through the token in its [`ThunkContext`], records the call, and returns so the guest carries
/// on (there is no kernel in the lab to service it).
fn record_syscall(call: &mut ThunkCall<'_>) {
    let token = call.context().0;
    let event = TraceEvent {
        kind: TraceKind::Syscall,
        address: call.address(),
        regs: [
            call.x(0),
            call.x(1),
            call.x(2),
            call.x(3),
            call.x(4),
            call.x(5),
            call.x(6),
            call.x(7),
        ],
        syscall_nr: call.x(8),
    };
    if let Ok(mut log) = syscall_log().lock() {
        log.entry(token).or_default().push(event);
    }
}

/// The driven run loop, factored out so it can borrow the cpu and the watch map at once.
///
/// It runs the guest from `from` until it returns through `sentinel`, handling each stop: a watched
/// address records its event (and, for `replace_return`, forces the return); a breakpoint is stepped
/// over and execution continues; a fault, unsupported instruction, unresolved-import thunk or budget
/// exhaustion ends the call with a typed error.
#[allow(clippy::too_many_arguments)]
fn drive(
    cpu: &mut DynarmicCpu,
    space: &GuestSpace,
    from: GuestAddr,
    sentinel: GuestAddr,
    watches: &BTreeMap<GuestAddr, Watch>,
    stop_at: &BTreeSet<GuestAddr>,
    budget: u64,
    trace_syscalls: bool,
    token: usize,
) -> Result<Stop> {
    drive_from(cpu, space, from, sentinel, watches, stop_at, budget, trace_syscalls, token)
}

/// The driven run loop proper. `from` is where the guest resumes (PC is set to it by the first
/// `run`).
#[allow(clippy::too_many_arguments)]
fn drive_from(
    cpu: &mut DynarmicCpu,
    _space: &GuestSpace,
    from: GuestAddr,
    sentinel: GuestAddr,
    watches: &BTreeMap<GuestAddr, Watch>,
    stop_at: &BTreeSet<GuestAddr>,
    budget: u64,
    trace_syscalls: bool,
    token: usize,
) -> Result<Stop> {
    let mut events: Vec<TraceEvent> = Vec::new();
    let mut pending_exits: BTreeMap<GuestAddr, GuestAddr> = BTreeMap::new(); // return site -> fn entry
    let mut installed_exit_bps: BTreeSet<GuestAddr> = BTreeSet::new();
    let mut total: u64 = 0;
    let mut pc = from;

    // The slice budget: run in chunks so a runaway guest is caught by `total`, not left forever.
    let slice = budget.min(50_000_000).max(1);

    loop {
        if total >= budget {
            return Err(DebugError::DidNotReturn(format!(
                "no return after {total} instructions (budget {budget}); likely an infinite loop"
            )));
        }
        let exit = cpu.run(pc, RunLimit::Instructions(slice))?;
        total = total.saturating_add(cpu.last_run_instructions());
        match exit {
            ExitReason::Returned { pc: at } if at == sentinel => {
                if trace_syscalls {
                    if let Ok(mut log) = syscall_log().lock() {
                        if let Some(v) = log.get_mut(&token) {
                            events.append(v); // drains the recorded syscalls into this call's events
                        }
                    }
                }
                return Ok(Stop::Returned(CallOutcome {
                    ret: cpu.x(XReg::new(0)?),
                    ret1: cpu.x(XReg::new(1)?),
                    instructions: total,
                    events,
                }));
            }
            ExitReason::StepLimitReached { pc: at, .. } => {
                pc = at; // budget slice ended mid-run; carry on
            }
            ExitReason::Breakpoint { pc: at } => {
                // A watched entry?
                if let Some(watch) = watches.get(&at) {
                    let regs = read_x0_x7(cpu);
                    if let Some(value) = watch.action.replace_return {
                        // Force the return: X0 = value, jump to LR, body never runs.
                        cpu.set_x(XReg::new(0)?, value);
                        let lr = cpu.x(XReg::new(30)?) as GuestAddr;
                        events.push(TraceEvent {
                            kind: TraceKind::Replaced,
                            address: at,
                            regs: [value, 0, 0, 0, 0, 0, 0, 0],
                            syscall_nr: 0,
                        });
                        pc = lr;
                        continue;
                    }
                    if watch.action.record_entry {
                        events.push(TraceEvent {
                            kind: TraceKind::Enter,
                            address: at,
                            regs,
                            syscall_nr: 0,
                        });
                    }
                    if watch.action.record_exit {
                        let ret_site = cpu.x(XReg::new(30)?) as GuestAddr;
                        if ret_site != sentinel && !installed_exit_bps.contains(&ret_site) {
                            cpu.add_breakpoint(ret_site)?;
                            installed_exit_bps.insert(ret_site);
                        }
                        pending_exits.insert(ret_site, at);
                    }
                }
                // An exit site we planted?
                else if let Some(entry) = pending_exits.remove(&at) {
                    events.push(TraceEvent {
                        kind: TraceKind::Exit,
                        address: entry,
                        regs: [cpu.x(XReg::new(0)?), 0, 0, 0, 0, 0, 0, 0],
                        syscall_nr: 0,
                    });
                    if installed_exit_bps.remove(&at) {
                        cpu.remove_breakpoint(at)?;
                    }
                }

                // A user breakpoint the caller wants to pause on: leave PC where it is (the
                // instruction has not run) and hand control back for inspection.
                if stop_at.contains(&at) {
                    return Ok(Stop::Breakpoint { address: at, events });
                }

                // Otherwise resume past it. Running *from* a breakpoint address makes the backend
                // suppress that one breakpoint for a single fetch, execute the instruction under it,
                // and carry on (cpu.rs: `suppressed_breakpoint`) — so the next iteration's `run(at)`
                // does exactly the resume-past-a-breakpoint step, with no manual single-step that
                // could overshoot the block and land silently on the next breakpoint.
                pc = at;
            }
            ExitReason::Thunk { pc: at } => {
                return Err(DebugError::DidNotReturn(format!(
                    "the function called an unresolved import at {at:#x}: load a provider or start \
                     the call past it"
                )));
            }
            other => {
                return Err(DebugError::DidNotReturn(other.to_string()));
            }
        }
    }
}

fn read_x0_x7(cpu: &DynarmicCpu) -> [u64; 8] {
    let mut r = [0u64; 8];
    for (i, slot) in r.iter_mut().enumerate() {
        *slot = cpu.x(XReg::new(i as u8).unwrap());
    }
    r
}

/// `(r, w, x)` permission bytes for a protection.
fn perms_of(p: Protection) -> (u8, u8, u8) {
    match p {
        Protection::None => (b'-', b'-', b'-'),
        Protection::Read => (b'r', b'-', b'-'),
        Protection::ReadWrite => (b'r', b'w', b'-'),
        Protection::ReadExecute => (b'r', b'-', b'x'),
        Protection::ReadWriteExecute => (b'r', b'w', b'x'),
    }
}

/// A guest address space whose top is at least 64 GiB up, matching what the loader and the golden
/// gates use (`harness::high_guest_space`).
fn high_guest_space() -> Result<GuestSpace> {
    let space = GuestSpace::new()?;
    if space.end() - 1 >= 1usize << 36 {
        return Ok(space);
    }
    drop(space);
    Ok(GuestSpace::with_config(GuestSpaceConfig {
        base_alignment: 1 << 36,
        ..GuestSpaceConfig::default()
    })?)
}
