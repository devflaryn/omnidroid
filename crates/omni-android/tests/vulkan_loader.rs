//! **The Vulkan loader, opened by real translated ARM64 code.**
//!
//! ```text
//! cargo test -p omni-android --release --test vulkan_loader
//! ```
//!
//! # Why this file exists separately from the gate
//!
//! The measurement this whole module is for — *which renderer does Roblox pick, and which Vulkan
//! entry points does it ask for* — can only be taken by the engine itself, in
//! `tests/gameactivity.rs`. That gate does not reach graphics today: it is blocked on the network
//! seam, several thousand instructions earlier. `VERIFICATION.md` entry 4 is unambiguous about
//! what to do with evidence that cannot be produced — a test that cannot run must fail, and a
//! claim that rests on a run nobody has made is not evidence.
//!
//! So nothing here depends on the APK. Every assertion below is about what the **guest** got
//! back, from real translated ARM64 code branching into the real thunk region, driving the exact
//! instruction sequence decoded at guest `0x02595160`:
//!
//! ```text
//! 0x2595170: adrp/add x0, "libvulkan.so.1" ; mov w1, #2 (RTLD_NOW) ; bl dlopen
//! 0x2595180: cbnz x0, got_it               ; if that failed,
//! 0x2595184: adrp/add x0, "libvulkan.so"   ; mov w1, #2            ; bl dlopen
//! 0x2595194: cbz  x0, give_up
//! 0x2595198: adrp/add x1, "vkGetInstanceProcAddr" ; bl dlsym
//! 0x25951b8: mov x0, xzr ; adrp/add x1, "vkCreateInstance" ; blr x8
//! 0x25951c8: ...          adrp/add x1, "vkEnumerateInstanceExtensionProperties"
//! ```
//!
//! [`the_bootstrap_sequence_libroblox_executes_runs_end_to_end`] is that program, `CBNZ` and all,
//! assembled word for word. A test that called the handlers directly would be asserting facts
//! about a function call; this one asserts what an engine doing the same thing will see.
//!
//! # What it asserts about the census, and why that is a detector rather than a watch
//!
//! `VERIFICATION.md` entry 11: a counter that rises under load but does not move under the
//! injected fault is a watch. So the census is asserted three ways rather than printed:
//!
//! * **It is empty before the guest runs and exact afterwards** — the ordered list of names,
//!   compared element by element, including the ones answered NULL. Membership and order, not a
//!   length (entry 1).
//! * **A name the guest did not ask for is absent.** A census that accumulated anything else
//!   would pass a length check and fail this one.
//! * **Two counters that must agree are read as a pair** (entry 15):
//!   `Vulkan::entry_calls()` is charged on a line nothing can truncate, and
//!   `requests().len() + requests_dropped()` is the log's own account of itself.

#![cfg(target_arch = "x86_64")]

mod harness;

use std::cell::Cell;
use std::sync::Arc;

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::Bionic;
use omni_android::vulkan::{
    ProcAnswer, Vulkan, LOADER_ENTRY_POINT, LOADER_SONAMES, NULL_INSTANCE_COMMANDS,
};
use omni_android::{AbiError, Boundary};
use omni_cpu::{ExitReason, GuestAddr};

/// `RTLD_NOW`, which is the `mov w1, #2` at `0x2595178`.
const RTLD_NOW: u64 = 2;

/// Where in the data region the guest's C strings start, leaving the low bytes for results.
const STRINGS_AT: usize = 0x800;

// ============================================================================ local encodings

/// `CBNZ Xt, label` — `1 011010 1 imm19:19 Rt:5`, the offset in instructions from the `CBNZ`.
///
/// Not in `harness::a64` and added here rather than there, because the harness is shared with
/// tests this one does not own. [`the_local_encodings_are_what_they_claim`] states the word in
/// hex so it can be checked against the ARM ARM by eye, which is the rule that file follows.
const fn cbnz(rt: u32, offset_insns: i32) -> u32 {
    0xB500_0000 | (((offset_insns as u32) & 0x7FFFF) << 5) | rt
}

/// The one encoding this file adds, in hex.
#[test]
fn the_local_encodings_are_what_they_claim() {
    assert_eq!(cbnz(0, 2), 0xB500_0040, "CBNZ X0, +8");
    // `CBZ` and `CBNZ` differ in one bit, and getting it wrong produces a legal instruction that
    // takes the branch in exactly the cases it should not.
    assert_eq!(cbnz(0, 2) ^ 0x0100_0000, 0xB400_0040, "CBZ X0, +8");
}

// ================================================================================ the fixture

/// A host directory that removes itself, so the bionic instance has a filesystem root.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-vkloader-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        Scratch(at)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A guest, a bionic instance for `dlopen`/`dlsym`, and — unless the test is the one that proves
/// the old behaviour survives — a bound [`Vulkan`].
struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    vulkan: Option<Arc<Vulkan>>,
    boundary: Arc<Boundary>,
    next_string: Cell<usize>,
    _root: Scratch,
}

fn fixture(tag: &str, with_vulkan: bool) -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(1024);
    bionic.bind_into(&builder).expect("bind every bionic handler");
    bionic.set_log_to_stderr(false);
    let vulkan = with_vulkan.then(|| {
        let vulkan = Vulkan::new();
        let bound = vulkan.bind_into(&builder).expect("bind the Vulkan loader");
        assert_eq!(bound, omni_android::vulkan::BOUND_SYMBOLS);
        vulkan
    });
    let root = Scratch::new(tag);
    bionic.set_filesystem_root(&root.0).expect("a filesystem root");
    let boundary = builder.finish();
    Fixture { guest, bionic, vulkan, boundary, next_string: Cell::new(STRINGS_AT), _root: root }
}

impl Fixture {
    fn vulkan(&self) -> &Arc<Vulkan> {
        self.vulkan.as_ref().expect("this fixture was built with a Vulkan instance")
    }

    fn thunk(&self, symbol: &str) -> GuestAddr {
        self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address
    }

    /// Write a NUL-terminated string into the data region and return its guest address.
    fn cstr(&self, text: &str) -> u64 {
        let offset = self.next_string.get();
        let at = self.guest.data + offset;
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        assert!(offset + bytes.len() < harness::DATA_BYTES, "the string area is full");
        self.guest.write_bytes(at, &bytes);
        self.next_string.set(offset + bytes.len() + 8);
        at as u64
    }

    fn run(&self, entry: GuestAddr) -> Result<ExitReason, AbiError> {
        let _bionic = self.bionic.activate().expect("publish the bionic instance");
        let _vulkan = self.vulkan.as_ref().map(|vulkan| vulkan.activate());
        let mut cpu = self.guest.thread(&self.boundary);
        self.boundary.run(&mut cpu, entry, BUDGET)
    }

    /// A program that calls `symbol`'s thunk and stores `X0` at the base of the data region.
    fn program_calling(&self, symbol: &str, setup: impl FnOnce(&mut Asm)) -> GuestAddr {
        let thunk = self.thunk(symbol);
        self.program_branching(|asm| {
            setup(asm);
            asm.bl(thunk);
        })
    }

    /// A program that does whatever `body` assembles and stores `X0` at the base of the data
    /// region. `X21` holds the return address and `X22` is scratch, so `body` owns `X0`-`X9`.
    fn program_branching(&self, body: impl FnOnce(&mut Asm)) -> GuestAddr {
        let entry = self.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        body(&mut asm);
        asm.mov(22, self.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        self.guest.load(asm.words());
        entry
    }

    fn value_of(&self, symbol: &str, setup: impl FnOnce(&mut Asm)) -> u64 {
        let entry = self.program_calling(symbol, setup);
        let exit = self.run(entry).expect("the run must complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        self.guest.read_u64(self.guest.data)
    }

    fn refusal_of(&self, symbol: &str, setup: impl FnOnce(&mut Asm)) -> AbiError {
        let entry = self.program_calling(symbol, setup);
        match self.run(entry) {
            Err(error) => error,
            Ok(exit) => panic!("`{symbol}` completed with {exit:?} where a refusal was required"),
        }
    }

    /// `dlopen(name, RTLD_NOW)` through the guest.
    fn dlopen(&self, name: &str) -> u64 {
        let at = self.cstr(name);
        self.value_of("dlopen", |asm| {
            asm.mov(0, at);
            asm.mov(1, RTLD_NOW);
        })
    }

    /// `dlsym(handle, symbol)` through the guest.
    fn dlsym(&self, handle: u64, symbol: &str) -> u64 {
        let at = self.cstr(symbol);
        self.value_of("dlsym", |asm| {
            asm.mov(0, handle);
            asm.mov(1, at);
        })
    }

    /// `vkGetInstanceProcAddr(instance, name)` through the guest, called **indirectly** through
    /// the address `dlsym` returned — which is how the engine calls it (`blr x8`).
    fn proc_addr(&self, entry_point: u64, instance: u64, name: &str) -> u64 {
        let at = self.cstr(name);
        let program = self.program_branching(|asm| {
            asm.mov(9, entry_point);
            asm.mov(0, instance);
            asm.mov(1, at);
            asm.push(blr(9));
        });
        let exit = self.run(program).expect("the run must complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        self.guest.read_u64(self.guest.data)
    }

    /// An indirect call to `target` with `X0`-`X2` set that must complete: its `X0`.
    fn call_through(&self, target: u64, args: [u64; 3]) -> u64 {
        let program = self.program_branching(|asm| {
            asm.mov(9, target);
            asm.mov(0, args[0]);
            asm.mov(1, args[1]);
            asm.mov(2, args[2]);
            asm.push(blr(9));
        });
        let exit = self.run(program).expect("the call must complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        self.guest.read_u64(self.guest.data)
    }

    /// The refusal an indirect call to `target` produces, with `X0`-`X2` set.
    fn refusal_of_call(&self, target: u64, args: [u64; 3]) -> AbiError {
        let program = self.program_branching(|asm| {
            asm.mov(9, target);
            asm.mov(0, args[0]);
            asm.mov(1, args[1]);
            asm.mov(2, args[2]);
            asm.push(blr(9));
        });
        match self.run(program) {
            Err(error) => error,
            Ok(exit) => panic!("a call to {target:#x} completed with {exit:?}, where the whole \
                                point of a stage 1 thunk is that it refuses"),
        }
    }

    /// The guest's own bootstrap: open the loader and fetch `vkGetInstanceProcAddr` from it.
    fn open_and_fetch(&self) -> (u64, u64) {
        let handle = self.dlopen(LOADER_SONAMES[0]);
        assert_ne!(handle, 0, "`{}` must open", LOADER_SONAMES[0]);
        let entry_point = self.dlsym(handle, LOADER_ENTRY_POINT);
        assert_ne!(entry_point, 0, "dlsym must answer for `{LOADER_ENTRY_POINT}`");
        (handle, entry_point)
    }
}

// ================================================================================= dlopen

/// **Both `soname`s open, they are different handles, and a library this runtime does not have
/// still answers NULL.**
///
/// The third clause is what makes the first two mean something. `dlopen` answering non-NULL for
/// everything would pass any test that only opened `libvulkan.so`, and it would hand the engine a
/// handle for `libcamera2ndk.so` — which `dl.rs` records the engine asking for at `0x243883c`,
/// with a null test in front of it.
#[test]
fn both_vulkan_sonames_open_and_an_absent_library_still_answers_null() {
    let _serial = serialized();
    let f = fixture("open", true);

    let first = f.dlopen(LOADER_SONAMES[0]);
    let second = f.dlopen(LOADER_SONAMES[1]);
    assert_ne!(first, 0, "`{}` is the soname tried at 0x2595170", LOADER_SONAMES[0]);
    assert_ne!(second, 0, "`{}` is the fall-back tried at 0x2595184", LOADER_SONAMES[1]);
    assert_ne!(
        first, second,
        "the two sonames are two scopes, and one handle for both would make `dlclose` of one \
         close the other"
    );

    assert_eq!(f.dlopen("libcamera2ndk.so"), 0, "a library this runtime does not have is NULL");
    assert_eq!(f.dlopen("libvulkan"), 0, "the name is matched exactly, not by prefix");
    assert_eq!(f.dlopen("libvulkan.so.2"), 0, "and not by prefix in the other direction");
}

/// **Without a bound [`Vulkan`], `dlopen("libvulkan.so")` answers NULL exactly as it did before.**
///
/// The regression detector for the `dl.rs` change. `provided_index` issues a handle only when the
/// entry point is really bound, so an embedding that never called `Vulkan::bind_into` — which is
/// every other test binary in this crate, and `tests/gameactivity.rs` until the line is added —
/// is untouched. A handle here would be one nobody can honour, which is the thing phase 2's
/// argument was right about.
#[test]
fn a_runtime_with_no_vulkan_instance_answers_null_for_the_loader() {
    let _serial = serialized();
    let f = fixture("unbound", false);
    for soname in LOADER_SONAMES {
        assert_eq!(
            f.dlopen(soname),
            0,
            "`{soname}` must stay NULL when nothing bound `{LOADER_ENTRY_POINT}`"
        );
    }
}

/// **A loader handle closes**, because the engine's own fall-back arm may close what it opened.
#[test]
fn a_loader_handle_can_be_closed() {
    let _serial = serialized();
    let f = fixture("close", true);
    let handle = f.dlopen(LOADER_SONAMES[0]);
    let result = f.value_of("dlclose", |asm| {
        asm.mov(0, handle);
    }) as i32;
    assert_eq!(result, 0, "dlclose of a handle this layer issued is success");
}

// ================================================================================== dlsym

/// **`dlsym` answers with the real `vkGetInstanceProcAddr` thunk**, and the address is the slot
/// the boundary actually holds — not merely something non-zero.
#[test]
fn dlsym_on_a_loader_handle_returns_the_real_entry_point_thunk() {
    let _serial = serialized();
    let f = fixture("dlsym", true);
    let expected = f.thunk(LOADER_ENTRY_POINT);
    assert_eq!(
        f.vulkan().entry_point(),
        Some(expected),
        "the instance and the boundary must agree about where the entry point is"
    );
    for soname in LOADER_SONAMES {
        let handle = f.dlopen(soname);
        assert_eq!(
            f.dlsym(handle, LOADER_ENTRY_POINT) as GuestAddr,
            expected,
            "dlsym through `{soname}` must give the thunk address, which is what `blr x8` \
             branches to"
        );
    }
}

/// **Another `vk*` name refuses rather than answering NULL.**
///
/// A real `libvulkan.so` exports `vkCreateInstance` directly, so a NULL would be this layer
/// making a false statement about the platform and the caller's null test would disable something
/// on the strength of it. The measured bootstrap never does this, so reaching the refusal is
/// itself the finding — which is why it names the symbol and says where the real one comes from.
#[test]
fn dlsym_for_another_vulkan_symbol_refuses_and_names_it() {
    let _serial = serialized();
    let f = fixture("dlsym-vk", true);
    let handle = f.dlopen(LOADER_SONAMES[0]);
    let name = f.cstr("vkCreateInstance");
    let error = f.refusal_of("dlsym", |asm| {
        asm.mov(0, handle);
        asm.mov(1, name);
    });
    let text = error.to_string();
    assert!(text.contains("vkCreateInstance"), "the refusal must name the symbol: {text}");
    assert!(text.contains(LOADER_ENTRY_POINT), "and what the loader does export: {text}");
    assert!(text.contains("libvulkan.so"), "and which library was asked: {text}");
}

/// **A non-Vulkan name in the loader's scope is an ordinary miss**, as it is on a device.
///
/// This is the clause that stops the loader handle being a second global scope: `memcpy` is bound
/// in this boundary and `boundary.lookup(None, "memcpy")` would find it, so answering from the
/// global table would hand the guest a `memcpy` it asked `libvulkan.so` for.
#[test]
fn dlsym_for_a_non_vulkan_symbol_in_the_loader_scope_is_a_miss() {
    let _serial = serialized();
    let f = fixture("dlsym-miss", true);
    assert_ne!(f.thunk("memcpy"), 0, "the premise: memcpy really is bound in this boundary");
    let handle = f.dlopen(LOADER_SONAMES[0]);
    assert_eq!(
        f.dlsym(handle, "memcpy"),
        0,
        "libvulkan.so does not export memcpy, and the global scope is not this handle's scope"
    );
}

// ================================================================= vkGetInstanceProcAddr

/// **The five null-instance commands each get a thunk, they are all distinct, and everything else
/// is NULL.**
///
/// The NULL half is the specified one — the Vulkan specification's "Command Function Pointers"
/// section fixes `vkGetInstanceProcAddr(VK_NULL_HANDLE, pName)` to be non-NULL for exactly those
/// five names — so this is the one place in the module where a zero is an answer rather than a
/// refusal, and it is asserted against the specification rather than against what the code does.
///
/// Distinctness is asserted because the engine stores these pointers (`0x6d3ca8`) and branches to
/// them: two names sharing one address would make the first call the engine makes report the
/// wrong function, which is worse than no answer.
#[test]
fn the_five_null_instance_commands_get_distinct_thunks_and_the_rest_are_null() {
    let _serial = serialized();
    let f = fixture("procaddr", true);
    let (_, entry_point) = f.open_and_fetch();

    let mut seen: Vec<(&str, u64)> = Vec::new();
    for name in NULL_INSTANCE_COMMANDS {
        let address = f.proc_addr(entry_point, 0, name);
        assert_ne!(address, 0, "`{name}` is valid with a null instance and must not be NULL");
        seen.push((name, address));
    }
    for (index, (name, address)) in seen.iter().enumerate() {
        for (other, other_address) in &seen[index + 1..] {
            assert_ne!(
                address, other_address,
                "`{name}` and `{other}` were given the same address {address:#x}"
            );
        }
    }
    assert_eq!(
        f.proc_addr(entry_point, 0, LOADER_ENTRY_POINT) as GuestAddr,
        f.thunk(LOADER_ENTRY_POINT),
        "the loader returns a pointer to itself, and it is the same pointer dlsym gave"
    );

    // Not in the specification's five. These are not "unimplemented" — a conforming loader
    // answers NULL for them with a null instance, and a device does too.
    for name in ["vkCreateDevice", "vkCreateAndroidSurfaceKHR", "vkGetDeviceProcAddr", ""] {
        assert_eq!(
            f.proc_addr(entry_point, 0, name),
            0,
            "`{name}` is not valid with a null instance, so NULL is the specified answer"
        );
    }

    // Asking twice gives the same pointer back: the engine compares these.
    let first = f.proc_addr(entry_point, 0, "vkCreateInstance");
    let again = f.proc_addr(entry_point, 0, "vkCreateInstance");
    assert_eq!(first, again, "one function must have one address");
    assert_eq!(
        f.vulkan().thunk_for("vkCreateInstance"),
        Some(first as GuestAddr),
        "the host side of the map must agree with what the guest was handed"
    );
    assert_eq!(f.vulkan().thunk_for("vkCreateDevice"), None, "a NULL consumed no slot");
    // Four pool slots: the five commands minus `vkGetInstanceProcAddr`, which is its own slot.
    assert_eq!(f.vulkan().handed_out(), NULL_INSTANCE_COMMANDS.len() - 1);
}

/// **A `VkInstance` this layer never issued refuses**, and quotes it.
///
/// Stage 1 refused *every* non-null instance, because it had created none. Stage 2a creates real
/// ones, so the refusal narrowed to the case that is actually dangerous and **the danger is what
/// it is now about**: a `VkInstance` is a dispatchable handle a host driver dereferences, so a
/// number the guest computed reaching one is a host access violation from guest data — Global
/// Constraint 11's Critical case. `Vulkan::instance_handles()` is the list of handles that are
/// real, and `0xdeadbeef` is not in it.
///
/// NULL is still not the answer: it would say "that instance does not support `vkCreateDevice`",
/// which is a statement about an instance that does not exist, and the engine would read it as a
/// missing capability rather than as a handle that came from nowhere.
#[test]
fn an_instance_handle_this_layer_never_issued_is_refused_and_quoted() {
    let _serial = serialized();
    let f = fixture("instance", true);
    let (_, entry_point) = f.open_and_fetch();
    let name = f.cstr("vkCreateDevice");
    let program = f.program_branching(|asm| {
        asm.mov(9, entry_point);
        asm.mov(0, 0xDEAD_BEEF);
        asm.mov(1, name);
        asm.push(blr(9));
    });
    let error = match f.run(program) {
        Err(error) => error,
        Ok(exit) => panic!("a non-null VkInstance completed with {exit:?}"),
    };
    let text = error.to_string();
    assert!(text.contains("0xdeadbeef"), "the refusal must quote the handle: {text}");
    assert!(text.contains("vkCreateDevice"), "and the name asked for: {text}");
    assert!(text.contains("not a `VkInstance` this layer issued"), "and why: {text}");
    assert!(text.contains("dispatchable"), "and what the danger is: {text}");
    assert!(f.vulkan().instance_handles().is_empty(), "nothing has been issued in this run");

    // It is recorded as well as refused: a refusal that is not in the census is a failure nobody
    // can count afterwards.
    let requests = f.vulkan().requests();
    let last = requests.last().expect("the refused lookup is still a lookup");
    assert_eq!(last.name, "vkCreateDevice");
    assert_eq!(last.instance, 0xDEAD_BEEF);
    assert_eq!(last.answer, ProcAnswer::Refused);
}

/// **A NULL `pName` refuses.** There is no name to look up, so a NULL return would be
/// indistinguishable from "that function is absent" — the one reading a caller acts on.
#[test]
fn a_null_name_refuses_rather_than_answering_null() {
    let _serial = serialized();
    let f = fixture("nullname", true);
    let (_, entry_point) = f.open_and_fetch();
    let error = f.refusal_of_call(entry_point, [0, 0, 0]);
    let text = error.to_string();
    assert!(text.contains("pName = NULL"), "{text}");
    assert!(text.contains("null-terminated UTF-8"), "{text}");
}

// ========================================================================= the returned thunks

/// **A call through a returned thunk is recorded with what was passed, and reaches its function.**
///
/// This test used to call `vkEnumerateInstanceLayerProperties` to see an unimplemented thunk
/// refuse; M6 implemented it (the engine's bootstrap calls it -- an Android app has no layers),
/// and with it every one of the specification's five null-instance commands is served, so this
/// hostless fixture can no longer reach an unimplemented one. That refusal -- named, arguments
/// quoted, no status invented -- is now asserted in `tests/vulkan_instance.rs`, through a
/// driver-backed instance. What stays here is the recording, which this fixture still reaches.
#[test]
fn a_call_through_a_returned_thunk_is_recorded_with_its_arguments() {
    let _serial = serialized();
    let f = fixture("call", true);
    let (_, entry_point) = f.open_and_fetch();
    let layers = f.proc_addr(entry_point, 0, "vkEnumerateInstanceLayerProperties");
    assert_ne!(layers, 0);

    assert!(f.vulkan().first_call().is_none(), "nothing has been called yet");
    let count_at = f.guest.data + 0x700;
    f.guest.write_u64(count_at, u64::MAX);
    let result = f.call_through(layers, [count_at as u64, 0, 0x3333]);
    assert_eq!(result as i32, 0, "VK_SUCCESS: an Android app has no layers");
    assert_eq!(f.guest.read_u64(count_at) as u32, 0);

    let call = f.vulkan().first_call().expect("the call is recorded");
    assert_eq!(call.name.as_deref(), Some("vkEnumerateInstanceLayerProperties"));
    assert_eq!(call.thunk, layers as GuestAddr);
    assert_eq!(call.args[0], count_at as u64);
    assert_eq!(call.args[2], 0x3333);
    assert_eq!(f.vulkan().calls_dropped(), 0);
    // And every call is counted by its name -- the census a long render loop's frame count is
    // read from, after the ordered log has filled.
    f.call_through(layers, [count_at as u64, 0, 0x3333]);
    assert_eq!(
        f.vulkan().call_counts().get("vkEnumerateInstanceLayerProperties"),
        Some(&2),
        "two calls, counted under the name they were made through"
    );
}

/// **A pool address the guest computed rather than received refuses too, and says so.**
///
/// The slots are consecutive, so a guest that has one and adds sixteen has another. It has never
/// been handed out, so there is no Vulkan function to name — and the refusal says *that*, rather
/// than naming whichever function happens to occupy the slot later.
#[test]
fn a_pool_slot_that_was_never_handed_out_refuses_without_inventing_a_name() {
    let _serial = serialized();
    let f = fixture("unassigned", true);
    let (_, entry_point) = f.open_and_fetch();
    let handed_out = f.proc_addr(entry_point, 0, "vkCreateInstance");
    let never = handed_out + omni_android::SLOT_BYTES as u64;
    let error = f.refusal_of_call(never, [1, 2, 3]);
    let text = error.to_string();
    assert!(text.contains("never handed out"), "{text}");
    assert!(!text.contains("vkCreateInstance"), "it must not borrow a neighbour's name: {text}");
}

// ================================================================================ the census

/// **The whole bootstrap, as `libroblox.so` executes it, and the census is the ordered list.**
///
/// One program: the two `dlopen`s with the real `CBNZ` between them, the `dlsym`, and the two
/// `BLR`s through the returned pointer with `X0` zeroed — the instructions at `0x2595170`
/// through `0x25951c8`. What comes out is the answer this stage exists to produce: *these are the
/// Vulkan entry points the engine asked for, in this order.*
#[test]
fn the_bootstrap_sequence_libroblox_executes_runs_end_to_end() {
    let _serial = serialized();
    let f = fixture("bootstrap", true);

    // Before anything runs, the census is empty and says so — the "before" half of the detector.
    assert!(f.vulkan().names().is_empty(), "nothing has been asked for yet");
    assert_eq!(f.vulkan().entry_calls(), 0);

    let soname_1 = f.cstr(LOADER_SONAMES[0]);
    let soname_0 = f.cstr(LOADER_SONAMES[1]);
    let symbol = f.cstr(LOADER_ENTRY_POINT);
    let create_instance = f.cstr("vkCreateInstance");
    let extensions = f.cstr("vkEnumerateInstanceExtensionProperties");
    let dlopen = f.thunk("dlopen");
    let dlsym = f.thunk("dlsym");
    let results = f.guest.data + 0x100;

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(23, results as u64);

    // 0x2595170: dlopen("libvulkan.so.1", RTLD_NOW)
    asm.mov(0, soname_1);
    asm.mov(1, RTLD_NOW);
    asm.bl(dlopen);
    asm.push(str_imm(0, 23, 0));

    // 0x2595180: cbnz x0, got_it — over the fall-back, whose length is computed rather than
    // counted by hand, because a wrong displacement here would branch into the middle of a `MOV`.
    let fallback_setup: Vec<u32> =
        mov64(0, soname_0).into_iter().chain(mov64(1, RTLD_NOW)).collect();
    let fallback_words = fallback_setup.len() + 2; // the BL and the STR
    asm.push(cbnz(0, fallback_words as i32 + 1));
    // 0x2595184: dlopen("libvulkan.so", RTLD_NOW)
    asm.extend(fallback_setup);
    asm.bl(dlopen);
    asm.push(str_imm(0, 23, 8));

    // got_it. 0x2595198: dlsym(handle, "vkGetInstanceProcAddr"), the handle still in X0.
    asm.mov(1, symbol);
    asm.bl(dlsym);
    asm.push(str_imm(0, 23, 16));
    asm.push(mov_reg(24, 0));

    // 0x25951b8: mov x0, xzr ; x1 = "vkCreateInstance" ; blr x8
    asm.push(mov_reg(8, 24));
    asm.mov(0, 0);
    asm.mov(1, create_instance);
    asm.push(blr(8));
    asm.push(str_imm(0, 23, 24));

    // 0x25951c8: the same again for the extension enumeration.
    asm.push(mov_reg(8, 24));
    asm.mov(0, 0);
    asm.mov(1, extensions);
    asm.push(blr(8));
    asm.push(str_imm(0, 23, 32));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let exit = f.run(entry).expect("the bootstrap must complete");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");

    let handle = f.guest.read_u64(results);
    let fallback = f.guest.read_u64(results + 8);
    let entry_point = f.guest.read_u64(results + 16);
    let vk_create_instance = f.guest.read_u64(results + 24);
    let vk_enumerate = f.guest.read_u64(results + 32);

    assert_ne!(handle, 0, "the first soname opened, so `cbnz` was taken");
    assert_eq!(fallback, 0, "and the fall-back arm was therefore skipped, not executed");
    assert_eq!(entry_point as GuestAddr, f.thunk(LOADER_ENTRY_POINT));
    assert_ne!(vk_create_instance, 0, "vkCreateInstance is valid with a null instance");
    assert_ne!(vk_enumerate, 0);
    assert_ne!(vk_create_instance, vk_enumerate, "two functions, two addresses");

    // **The measurement.** Order and membership, not a count.
    assert_eq!(
        f.vulkan().names(),
        vec!["vkCreateInstance".to_string(), "vkEnumerateInstanceExtensionProperties".to_string()],
        "this is the ordered list of Vulkan entry points the guest asked for"
    );
    let requests = f.vulkan().requests();
    assert!(
        requests.iter().all(|request| request.instance == 0),
        "every lookup in this sequence is on a null instance: {requests:?}"
    );
    assert!(
        requests.iter().all(|request| matches!(request.answer, ProcAnswer::Thunk(_))),
        "both names are in the specification's five, so both are pointers: {requests:?}"
    );
    assert!(
        !f.vulkan().names().iter().any(|name| name == "vkCreateDevice"),
        "a name the guest never asked for must not be in the census"
    );

    // Entry 15's pair: a total charged where nothing can truncate it, and the log's own account.
    assert_eq!(f.vulkan().requests_dropped(), 0);
    assert_eq!(
        f.vulkan().entry_calls(),
        (requests.len() + f.vulkan().requests_dropped()) as u64,
        "the unconditional counter and the bounded log must agree"
    );

    // And the step after the bootstrap: the engine calls what it fetched. That is the second
    // half of the answer this stage owes — *which* entry point it actually goes to first — and it
    // is recorded before anything else can happen, which is what makes the record survive every
    // outcome.
    //
    // **This fixture attaches no `VulkanHost`**, on purpose: it is the stage 1 fixture and this
    // is the assertion that a loader with no driver behind it refuses *naming the missing piece*
    // rather than inventing a `VK_SUCCESS` or answering the engine's null test with a zero.
    // `tests/vulkan_instance.rs` is the same call with a driver attached.
    assert!(f.vulkan().first_call().is_none(), "nothing has been called through a thunk yet");
    let error = f.refusal_of_call(vk_create_instance, [0x1000, 0, 0x2000]);
    let first = f.vulkan().first_call().expect("the call is recorded before it refuses");
    assert_eq!(first.name.as_deref(), Some("vkCreateInstance"));
    assert_eq!(f.vulkan().calls().len(), 1, "one BLR, one record");
    let text = error.to_string();
    assert!(text.contains("no host driver attached"), "{text}");
    assert!(text.contains("Vulkan::set_host"), "and what an embedding must do: {text}");
    assert_eq!(
        f.vulkan().allocator_calls(),
        1,
        "the pAllocator observation is charged before anything can return early"
    );
    assert_eq!(f.vulkan().allocator_non_null(), 0, "and this call passed NULL");

    // What a gate prints, asserted rather than only printed.
    let report = f.vulkan().report();
    assert!(report.contains("vkCreateInstance"), "{report}");
    assert!(report.contains("vkEnumerateInstanceExtensionProperties"), "{report}");
    assert!(report.contains("called [0] vkCreateInstance"), "{report}");
    assert!(!report.contains("no Vulkan entry point"), "{report}");

    eprintln!("\n{report}");
    eprintln!("  and what the call answered:\n    {error}\n");
}

/// **The census records the NULL answers too**, which is the half that would otherwise be
/// invisible.
///
/// A name answered NULL leaves no pointer and no call, so the *only* record that the engine
/// wanted it is this one. If the engine ever asks for a sixth name with a null instance and then
/// gives up, this is what says so — rather than the run presenting as "Vulkan initialisation
/// failed" with nothing naming the function.
#[test]
fn a_name_answered_null_is_still_recorded_with_its_answer() {
    let _serial = serialized();
    let f = fixture("nullcensus", true);
    let (_, entry_point) = f.open_and_fetch();
    assert_eq!(f.proc_addr(entry_point, 0, "vkCreateAndroidSurfaceKHR"), 0);

    let requests = f.vulkan().requests();
    assert_eq!(requests.len(), 1, "one lookup, one record: {requests:?}");
    assert_eq!(requests[0].name, "vkCreateAndroidSurfaceKHR");
    assert_eq!(requests[0].answer, ProcAnswer::NullPerSpecification);
    assert_eq!(requests[0].order, 0);
    assert_ne!(requests[0].caller, 0, "the call site is recorded, so a report can point at it");
    assert_eq!(f.vulkan().handed_out(), 0, "a NULL consumes no pool slot");
}

/// **A thread with no published instance refuses by name** rather than reaching a process-wide
/// default.
///
/// The failure this guards is the one `Ndk::thread_instance` already cost this project once: the
/// engine's renderer bring-up runs on the game thread, and an instance published only to the
/// thread that called `initializeNativeCode` is absent exactly where it is needed. A default
/// would turn that into a census that is silently empty.
#[test]
fn an_unpublished_instance_refuses_and_names_how_to_publish_one() {
    let _serial = serialized();
    let f = fixture("unpublished", true);
    let (_, entry_point) = f.open_and_fetch();
    let name = f.cstr("vkCreateInstance");
    let program = f.program_branching(|asm| {
        asm.mov(9, entry_point);
        asm.mov(0, 0);
        asm.mov(1, name);
        asm.push(blr(9));
    });
    // Run with bionic published and Vulkan deliberately not.
    let error = {
        let _bionic = f.bionic.activate().expect("publish the bionic instance");
        let mut cpu = f.guest.thread(&f.boundary);
        match f.boundary.run(&mut cpu, program, BUDGET) {
            Err(error) => error,
            Ok(exit) => panic!("an unpublished instance completed with {exit:?}"),
        }
    };
    let text = error.to_string();
    assert!(text.contains("Vulkan::activate"), "{text}");
    assert!(text.contains("thread_instance"), "and the guest-thread half: {text}");
}
