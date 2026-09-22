//! **Stage 2a: a real `VkInstance`, created by translated ARM64 code through a guest thunk.**
//!
//! ```text
//! cargo test -p omni-android --release --test vulkan_instance
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-android --release --test vulkan_instance -- --ignored --test-threads=1
//! ```
//!
//! # What this file asserts, and where the evidence comes from
//!
//! `tests/vulkan_loader.rs` proves the loader opens and records. This one proves the two commands
//! the engine reaches first are **forwarded**, and it does it twice over, because the two halves
//! answer different questions and neither is enough on its own.
//!
//! The **ordinary** tests run everywhere, on every machine, with no GPU and no display. They drive
//! the guest through a [`RecordingHost`] — a test double for the *host* side of
//! [`VulkanHost`](omni_android::vulkan::VulkanHost), not for anything the guest can see. Its job
//! is to be a driver whose answers the test *chose*, so that the things worth asserting can be
//! asserted at all: that the name substitution happened and was recorded, that the guest's
//! `VkInstanceCreateInfo` was decoded field by field, that a driver's `VkResult` reaches the guest
//! unchanged, that a non-null `pAllocator` refuses, and that a wild `VkInstance` refuses instead
//! of reaching a driver. None of those can be established against a real driver, because a real
//! driver's answers are not ours to choose — a real driver cannot be made to return
//! `VK_ERROR_INCOMPATIBLE_DRIVER` on demand.
//!
//! The **live** tests are the other half and they are the ones that answer "does it work". They
//! are `#[ignore]`d and gated on `OMNI_GFX_WINDOW_TESTS`, the same gate `omni-gfx`'s
//! `renderer_live.rs` and `tests/ndk_host_window.rs` use — one decision by one person covers the
//! whole graphics bring-up. Under `--ignored` without the gate they **panic** naming the variable.
//! Nothing here is allowed to quietly pass on a machine that cannot run it (`VERIFICATION.md`
//! entry 4).
//!
//! # Why a test double is not the thing rule 1 forbids
//!
//! "No plausible stubs" is about what the **guest** is told. A `vkCreateInstance` that answers
//! `VK_SUCCESS` without creating anything is forbidden because the engine will act on it and the
//! consequence arrives three frames later with nothing naming the cause. [`RecordingHost`] is on
//! the other side of the seam: it is the *embedding*, it exists only inside this test binary, and
//! the guest-facing code under test is the real code either way. The assertion that a real
//! instance really gets created belongs to the live tests, and they are the ones that print what
//! the driver said.

#![cfg(target_arch = "x86_64")]

mod harness;

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::Bionic;
use omni_android::vulkan::{
    DriverAnswer, HostExtension, HostInstance, InstanceRequest, ProcAnswer, RewriteSite, Vulkan,
    VulkanHost, GUEST_SURFACE_EXTENSION, LOADER_ENTRY_POINT, LOADER_SONAMES, MAX_ENABLED_NAMES,
    VK_INCOMPLETE, VK_SUCCESS,
};
use omni_android::{AbiError, AbiResult, Boundary};
use omni_cpu::{ExitReason, GuestAddr};

/// `RTLD_NOW`, the `mov w1, #2` at `0x2595178`.
const RTLD_NOW: u64 = 2;

/// The gate, shared with `omni-gfx`'s `renderer_live.rs` and `tests/ndk_host_window.rs`.
const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

/// `sizeof(VkExtensionProperties)`.
const EXTENSION_PROPERTIES_BYTES: usize = omni_android::vulkan::EXTENSION_PROPERTIES_BYTES;

/// `VK_ERROR_INCOMPATIBLE_DRIVER`, which [`RecordingHost`] can be told to answer with.
const VK_ERROR_INCOMPATIBLE_DRIVER: i32 = -9;

/// Fail, naming the variable, if a live test was run without the opt-in.
fn require_gate() {
    let set = std::env::var(GATE).is_ok_and(|v| v == "1");
    assert!(
        set,
        "this test was run with --ignored but {GATE} is not set to 1. It loads the host's Vulkan \
         driver and creates a real VkInstance on it; it will not pretend to have passed on a \
         machine that cannot do that. Set {GATE}=1 to run it, or drop --ignored to skip it \
         visibly."
    );
}

// =================================================================== the host test double

/// What a [`RecordingHost`] was asked, and what it will answer.
#[derive(Debug, Default)]
struct HostLog {
    /// Every `vkCreateInstance` request, exactly as the shim built it.
    ///
    /// **The assertion surface that matters most in this file.** It is the only way to see that
    /// the extension list reaching a driver is in *host* spelling, because the guest cannot see
    /// its own request after it has been rewritten.
    requests: Vec<InstanceRequest>,
    /// Every `pLayerName` the shim passed through, `None` for the implicit set.
    layers_asked: Vec<Option<String>>,
    /// Every `(instance, name)` `vkGetInstanceProcAddr` asked about.
    procs_asked: Vec<(HostInstance, String)>,
}

/// A [`VulkanHost`] whose answers the test chooses. **Not a driver, and not guest-facing.**
///
/// See this module's header for why a double on this side of the seam is not the thing rule 1
/// forbids. What it gives up is the only thing it could: it cannot show that a real driver accepts
/// what the shim builds. That is what the live tests are for, and nothing here claims otherwise.
#[derive(Debug)]
struct RecordingHost {
    extensions: Vec<HostExtension>,
    /// The name [`VulkanHost::platform_surface_extension`] answers with.
    surface: String,
    /// What `vkCreateInstance` answers. `None` creates; `Some` is a driver failure code.
    fail_create_with: Option<i32>,
    /// The names `has_instance_proc` says the driver has. Everything else is `false`.
    driver_has: Vec<String>,
    log: Mutex<HostLog>,
}

impl RecordingHost {
    /// A host that looks like this machine's: a surface extension, a platform one, and one more.
    fn windows_like() -> Arc<RecordingHost> {
        RecordingHost::new(None)
    }

    /// The same host, except that `vkCreateInstance` answers with a driver failure code.
    fn failing(result: i32) -> Arc<RecordingHost> {
        RecordingHost::new(Some(result))
    }

    fn new(fail_create_with: Option<i32>) -> Arc<RecordingHost> {
        Arc::new(RecordingHost {
            extensions: vec![
                extension("VK_KHR_surface", 25),
                extension("VK_KHR_win32_surface", 6),
                extension("VK_EXT_debug_utils", 2),
            ],
            surface: "VK_KHR_win32_surface".to_string(),
            fail_create_with,
            driver_has: vec![
                "vkDestroyInstance".to_string(),
                "vkEnumeratePhysicalDevices".to_string(),
                "vkCreateWin32SurfaceKHR".to_string(),
            ],
            log: Mutex::new(HostLog::default()),
        })
    }

    fn log(&self) -> std::sync::MutexGuard<'_, HostLog> {
        self.log.lock().expect("the log is never held across a panic")
    }
}

fn extension(name: &str, spec_version: u32) -> HostExtension {
    HostExtension { name: name.to_string(), spec_version }
}

impl VulkanHost for RecordingHost {
    fn platform_surface_extension(&self) -> AbiResult<String> {
        Ok(self.surface.clone())
    }

    fn instance_extensions(
        &self,
        layer: Option<&str>,
    ) -> AbiResult<DriverAnswer<Vec<HostExtension>>> {
        self.log().layers_asked.push(layer.map(str::to_string));
        if layer.is_some() {
            // A layer this "driver" does not have, answered the way a driver answers it.
            return Ok(DriverAnswer::Failed(-6)); // VK_ERROR_LAYER_NOT_PRESENT
        }
        Ok(DriverAnswer::Ok(self.extensions.clone()))
    }

    fn create_instance(&self, request: &InstanceRequest) -> AbiResult<DriverAnswer<HostInstance>> {
        let mut log = self.log();
        log.requests.push(request.clone());
        if let Some(result) = self.fail_create_with {
            return Ok(DriverAnswer::Failed(result));
        }
        Ok(DriverAnswer::Ok(HostInstance::from_token(log.requests.len() as u64 - 1)))
    }

    fn has_instance_proc(&self, instance: HostInstance, name: &str) -> AbiResult<bool> {
        self.log().procs_asked.push((instance, name.to_string()));
        Ok(self.driver_has.iter().any(|have| have == name))
    }
}

// ================================================================================ the fixture

/// A host directory that removes itself, so the bionic instance has a filesystem root.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-vkinst-{tag}-{}", std::process::id()));
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

struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    vulkan: Arc<Vulkan>,
    boundary: Arc<Boundary>,
    /// A bump pointer through the data region, for guest strings and guest structures.
    next: Cell<usize>,
    _root: Scratch,
}

/// Where the bump allocator starts, leaving the low bytes for results.
const ARENA_AT: usize = 0x800;

/// A fixture with a driver behind the loader.
fn fixture_with(tag: &str, host: Arc<dyn VulkanHost>) -> Fixture {
    fixture(tag, Some(host))
}

/// A fixture with none, for the tests that are about a loader nobody gave a driver to.
fn fixture_without_host(tag: &str) -> Fixture {
    fixture(tag, None)
}

fn fixture(tag: &str, host: Option<Arc<dyn VulkanHost>>) -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(1024);
    bionic.bind_into(&builder).expect("bind every bionic handler");
    bionic.set_log_to_stderr(false);
    let vulkan = Vulkan::new();
    let bound = vulkan.bind_into(&builder).expect("bind the Vulkan loader");
    assert_eq!(bound, omni_android::vulkan::BOUND_SYMBOLS);
    if let Some(host) = host {
        vulkan.set_host(host);
    }
    let root = Scratch::new(tag);
    bionic.set_filesystem_root(&root.0).expect("a filesystem root");
    let boundary = builder.finish();
    Fixture { guest, bionic, vulkan, boundary, next: Cell::new(ARENA_AT), _root: root }
}

impl Fixture {
    fn vulkan(&self) -> &Arc<Vulkan> {
        &self.vulkan
    }

    fn thunk(&self, symbol: &str) -> GuestAddr {
        self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address
    }

    /// `len` bytes of the data region, eight-aligned.
    fn alloc(&self, len: usize) -> u64 {
        let offset = (self.next.get() + 7) & !7;
        assert!(offset + len < harness::DATA_BYTES, "the guest arena is full");
        self.next.set(offset + len);
        (self.guest.data + offset) as u64
    }

    /// Write a NUL-terminated string into the data region and return its guest address.
    fn cstr(&self, text: &str) -> u64 {
        let bytes: Vec<u8> = text.bytes().chain(std::iter::once(0)).collect();
        let at = self.alloc(bytes.len());
        self.guest.write_bytes(at as GuestAddr, &bytes);
        at
    }

    /// Write an array of `u64` and return its guest address.
    fn u64_array(&self, values: &[u64]) -> u64 {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let at = self.alloc(bytes.len().max(8));
        self.guest.write_bytes(at as GuestAddr, &bytes);
        at
    }

    /// A `VkApplicationInfo` in guest memory.
    fn application_info(&self, application: &str, engine: &str, api_version: u32) -> u64 {
        let application_name = self.cstr(application);
        let engine_name = self.cstr(engine);
        let mut bytes = vec![0u8; 48];
        bytes[0..4].copy_from_slice(&0u32.to_le_bytes()); // VK_STRUCTURE_TYPE_APPLICATION_INFO
        bytes[16..24].copy_from_slice(&application_name.to_le_bytes());
        bytes[24..28].copy_from_slice(&7u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&engine_name.to_le_bytes());
        bytes[40..44].copy_from_slice(&11u32.to_le_bytes());
        bytes[44..48].copy_from_slice(&api_version.to_le_bytes());
        let at = self.alloc(48);
        self.guest.write_bytes(at as GuestAddr, &bytes);
        at
    }

    /// A `VkInstanceCreateInfo` in guest memory, exactly as a C compiler would lay it out.
    fn create_info(
        &self,
        application_info: u64,
        layers: &[&str],
        extensions: &[&str],
    ) -> u64 {
        self.create_info_raw(1, 0, application_info, layers, extensions)
    }

    fn create_info_raw(
        &self,
        stype: u32,
        next: u64,
        application_info: u64,
        layers: &[&str],
        extensions: &[&str],
    ) -> u64 {
        let layer_pointers: Vec<u64> = layers.iter().map(|name| self.cstr(name)).collect();
        let extension_pointers: Vec<u64> =
            extensions.iter().map(|name| self.cstr(name)).collect();
        let layer_array = self.u64_array(&layer_pointers);
        let extension_array = self.u64_array(&extension_pointers);
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(&stype.to_le_bytes());
        bytes[8..16].copy_from_slice(&next.to_le_bytes());
        bytes[16..20].copy_from_slice(&0u32.to_le_bytes()); // flags
        bytes[24..32].copy_from_slice(&application_info.to_le_bytes());
        bytes[32..36].copy_from_slice(&(layers.len() as u32).to_le_bytes());
        bytes[40..48].copy_from_slice(&layer_array.to_le_bytes());
        bytes[48..52].copy_from_slice(&(extensions.len() as u32).to_le_bytes());
        bytes[56..64].copy_from_slice(&extension_array.to_le_bytes());
        let at = self.alloc(64);
        self.guest.write_bytes(at as GuestAddr, &bytes);
        at
    }

    fn run(&self, entry: GuestAddr) -> Result<ExitReason, AbiError> {
        let _bionic = self.bionic.activate().expect("publish the bionic instance");
        let _vulkan = self.vulkan.activate();
        let mut cpu = self.guest.thread(&self.boundary);
        self.boundary.run(&mut cpu, entry, BUDGET)
    }

    /// A program that does whatever `body` assembles and stores `X0` at the base of the data
    /// region. `X21` holds the return address and `X22` is scratch.
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

    /// Call `target` indirectly with `x0`-`x3` set — which is how the engine calls every Vulkan
    /// entry point (`blr x8` through the pointer `vkGetInstanceProcAddr` returned).
    fn call(&self, target: u64, args: [u64; 4]) -> Result<u64, AbiError> {
        let program = self.program_branching(|asm| {
            asm.mov(9, target);
            asm.mov(0, args[0]);
            asm.mov(1, args[1]);
            asm.mov(2, args[2]);
            asm.mov(3, args[3]);
            asm.push(blr(9));
        });
        let exit = self.run(program)?;
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        Ok(self.guest.read_u64(self.guest.data))
    }

    /// Call `target` and require that it refuses.
    fn refusal(&self, target: u64, args: [u64; 4]) -> AbiError {
        match self.call(target, args) {
            Err(error) => error,
            Ok(value) => panic!("a call to {target:#x} returned {value:#x} where a refusal was required"),
        }
    }

    /// `dlopen` + `dlsym` + `vkGetInstanceProcAddr`, as the engine does it.
    fn entry_point(&self) -> u64 {
        let soname = self.cstr(LOADER_SONAMES[0]);
        let symbol = self.cstr(LOADER_ENTRY_POINT);
        let dlopen = self.thunk("dlopen");
        let dlsym = self.thunk("dlsym");
        let program = self.program_branching(|asm| {
            asm.mov(0, soname);
            asm.mov(1, RTLD_NOW);
            asm.bl(dlopen);
            asm.mov(1, symbol);
            asm.bl(dlsym);
        });
        let exit = self.run(program).expect("the bootstrap must complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        let at = self.guest.read_u64(self.guest.data);
        assert_eq!(at as GuestAddr, self.thunk(LOADER_ENTRY_POINT));
        at
    }

    /// `vkGetInstanceProcAddr(instance, name)` through the guest.
    fn proc_addr(&self, entry_point: u64, instance: u64, name: &str) -> Result<u64, AbiError> {
        let at = self.cstr(name);
        self.call(entry_point, [instance, at, 0, 0])
    }

    /// The thunk for `name`, resolved the way the engine resolves it.
    fn resolve(&self, entry_point: u64, name: &str) -> u64 {
        let at = self.proc_addr(entry_point, 0, name).expect("the lookup must complete");
        assert_ne!(at, 0, "`{name}` must resolve to a thunk");
        at
    }

    /// Read a `uint32_t` back out of guest memory.
    fn read_u32(&self, at: u64) -> u32 {
        self.guest.read_u64(at as GuestAddr) as u32
    }

    /// Read `len` bytes back out of guest memory. `Guest::read_u64` is unaligned, and a
    /// `VkExtensionProperties` is 260 bytes, so entry *n* is not eight-aligned for odd *n*.
    fn read_bytes(&self, at: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        let mut offset = 0;
        while out.len() < len {
            out.extend_from_slice(&self.guest.read_u64(at as GuestAddr + offset).to_le_bytes());
            offset += 8;
        }
        out.truncate(len);
        out
    }

    /// Read back an array of `VkExtensionProperties` the guest was written.
    fn read_extension_properties(&self, at: u64, count: usize) -> Vec<(String, u32)> {
        let bytes = self.read_bytes(at, count * EXTENSION_PROPERTIES_BYTES);
        (0..count)
            .map(|index| {
                let entry = &bytes[index * EXTENSION_PROPERTIES_BYTES..][..EXTENSION_PROPERTIES_BYTES];
                let name = &entry[..256];
                let end = name.iter().position(|&b| b == 0).expect("a NUL-terminated name");
                let version = u32::from_le_bytes(entry[256..].try_into().expect("four bytes"));
                (String::from_utf8(name[..end].to_vec()).expect("an ASCII name"), version)
            })
            .collect()
    }
}

// ================================================= vkEnumerateInstanceExtensionProperties

/// **The guest is shown `VK_KHR_android_surface` where the driver said the platform one, and the
/// substitution is in the log.**
///
/// The assertion the whole rewrite machinery exists for, and it is made **three** ways on purpose,
/// because any one of them alone can be satisfied by the wrong implementation:
///
/// * the guest reads the Android name out of its own array — which a passthrough would fail;
/// * the guest does **not** read the host name — which "append instead of replace" would fail;
/// * `Vulkan::rewrites()` names both spellings and the direction — which a *silent* rename would
///   fail, and a silent rename is the thing Global Constraint 1 is about.
#[test]
fn the_android_surface_extension_is_advertised_and_the_substitution_is_recorded() {
    let _serial = serialized();
    let host = RecordingHost::windows_like();
    let f = fixture_with("advertise", host.clone());
    let entry_point = f.entry_point();
    let enumerate = f.resolve(entry_point, "vkEnumerateInstanceExtensionProperties");

    assert!(f.vulkan().rewrites().is_empty(), "nothing has been rewritten yet");

    // First call: the count alone, which is the protocol the specification fixes.
    let count_at = f.alloc(4);
    let result = f.call(enumerate, [0, count_at, 0, 0]).expect("the count call must complete");
    assert_eq!(result as i32, VK_SUCCESS);
    let count = f.read_u32(count_at);
    assert_eq!(count, 3, "the driver's own count, unchanged by a rename");

    // Second call: the array.
    let properties_at = f.alloc(count as usize * EXTENSION_PROPERTIES_BYTES);
    let result = f
        .call(enumerate, [0, count_at, properties_at, 0])
        .expect("the enumeration must complete");
    assert_eq!(result as i32, VK_SUCCESS, "the array was big enough, so this is not VK_INCOMPLETE");
    assert_eq!(f.read_u32(count_at), 3, "and the count written back is what fitted");

    let shown = f.read_extension_properties(properties_at, 3);
    let names: Vec<&str> = shown.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        ["VK_KHR_surface", GUEST_SURFACE_EXTENSION, "VK_EXT_debug_utils"],
        "the driver's order, with the platform surface extension under the guest's name"
    );
    assert!(
        !names.contains(&"VK_KHR_win32_surface"),
        "the host name must not also be advertised: the guest could enable it by its real name \
         and take a different path through its own platform layer, by accident"
    );
    assert_eq!(shown[1].1, 6, "the driver's specVersion travelled with the name, not invented");

    // **The log.** A list that came out right by coincidence passes the assertions above and
    // fails this one.
    let rewrites = f.vulkan().rewrites();
    assert_eq!(rewrites.len(), 2, "one per enumeration that produced an array: {rewrites:?}");
    for (index, rewrite) in rewrites.iter().enumerate() {
        assert_eq!(rewrite.site, RewriteSite::Advertised);
        assert_eq!(rewrite.from, "VK_KHR_win32_surface");
        assert_eq!(rewrite.to, GUEST_SURFACE_EXTENSION);
        assert_eq!(rewrite.spec_version, Some(6));
        assert_eq!(rewrite.order, index, "the ordinals are the ordered sequence");
        assert_ne!(rewrite.caller, 0, "and each points at the instruction that caused it");
    }
    assert_eq!(f.vulkan().rewrites_dropped(), 0);

    // And a host reads it the way it reads the census: from the report, which states it whether
    // or not anything was rewritten.
    let report = f.vulkan().report();
    assert!(report.contains("extension-name substitutions: 2 recorded"), "{report}");
    assert!(report.contains("advertised to the guest as"), "{report}");
    assert!(report.contains(GUEST_SURFACE_EXTENSION), "{report}");
    assert_eq!(host.log().layers_asked, vec![None, None], "the implicit set, both times");
}

/// **A `pProperties` array that is too small gets `VK_INCOMPLETE` and the count of what fitted.**
///
/// `VK_INCOMPLETE` is a *success* code the caller is required to handle; answering `VK_SUCCESS`
/// would tell the engine it had seen every extension when it had seen one.
#[test]
fn a_short_array_is_vk_incomplete_and_says_how_much_fitted() {
    let _serial = serialized();
    let f = fixture_with("incomplete", RecordingHost::windows_like());
    let entry_point = f.entry_point();
    let enumerate = f.resolve(entry_point, "vkEnumerateInstanceExtensionProperties");

    let count_at = f.alloc(4);
    let properties_at = f.alloc(3 * EXTENSION_PROPERTIES_BYTES);
    f.guest.write_u32(count_at as GuestAddr, 1);
    let result = f.call(enumerate, [0, count_at, properties_at, 0]).expect("it must complete");
    assert_eq!(result as i32, VK_INCOMPLETE, "one of three fitted");
    assert_eq!(f.read_u32(count_at), 1, "and the count written back is what fitted");
    assert_eq!(f.read_extension_properties(properties_at, 1)[0].0, "VK_KHR_surface");

    // A capacity of exactly the available count is VK_SUCCESS, not VK_INCOMPLETE — the boundary
    // case a `<` written as `<=` would get wrong in the direction nobody notices.
    f.guest.write_u32(count_at as GuestAddr, 3);
    let result = f.call(enumerate, [0, count_at, properties_at, 0]).expect("it must complete");
    assert_eq!(result as i32, VK_SUCCESS);
    assert_eq!(f.read_u32(count_at), 3);
}

/// **A NULL `pPropertyCount` refuses**, because there is nowhere to put an answer and
/// `VK_SUCCESS` would claim a count had been written into memory nothing wrote to.
#[test]
fn a_null_property_count_refuses_rather_than_answering_success() {
    let _serial = serialized();
    let f = fixture_with("nullcount", RecordingHost::windows_like());
    let entry_point = f.entry_point();
    let enumerate = f.resolve(entry_point, "vkEnumerateInstanceExtensionProperties");
    let text = f.refusal(enumerate, [0, 0, 0, 0]).to_string();
    assert!(text.contains("pPropertyCount = NULL"), "{text}");
    assert!(text.contains("two-call protocol"), "{text}");
}

/// **A named layer the driver does not have answers with the driver's own code**, forwarded
/// verbatim — and the failure is recorded, because a forwarded failure leaves no other trace.
#[test]
fn a_layer_the_driver_lacks_answers_with_the_drivers_own_result() {
    let _serial = serialized();
    let host = RecordingHost::windows_like();
    let f = fixture_with("layer", host.clone());
    let entry_point = f.entry_point();
    let enumerate = f.resolve(entry_point, "vkEnumerateInstanceExtensionProperties");

    let count_at = f.alloc(4);
    let layer = f.cstr("VK_LAYER_KHRONOS_validation");
    let result = f.call(enumerate, [layer, count_at, 0, 0]).expect("it must complete");
    assert_eq!(result as i32, -6, "VK_ERROR_LAYER_NOT_PRESENT, exactly as the driver said it");
    assert_eq!(
        host.log().layers_asked,
        vec![Some("VK_LAYER_KHRONOS_validation".to_string())],
        "the name reached the host unchanged"
    );
    assert_eq!(
        f.vulkan().driver_failures(),
        vec![("vkEnumerateInstanceExtensionProperties".to_string(), -6)],
        "a forwarded failure produces no refusal and no thunk, so this log is its only trace"
    );
    assert!(f.vulkan().rewrites().is_empty(), "a failed enumeration rewrote nothing");
}

// ================================================================================ vkCreateInstance

/// **A real instance: the request reaches the host in host spelling, the guest gets a registry
/// handle, and both directions of the rewrite are logged.**
#[test]
fn vk_create_instance_forwards_rewrites_and_registers_the_handle() {
    let _serial = serialized();
    let host = RecordingHost::windows_like();
    let f = fixture_with("create", host.clone());
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");

    let application = f.application_info("RobloxPlayer", "Roblox", 0x0040_0000);
    let info = f.create_info(
        application,
        &[],
        &["VK_KHR_surface", GUEST_SURFACE_EXTENSION],
    );
    let out = f.alloc(8);
    let result = f.call(create, [info, 0, out, 0]).expect("vkCreateInstance must complete");
    assert_eq!(result as i32, VK_SUCCESS);

    // What the **driver** was asked for: the Android name is gone and the host's is in its place.
    let requests = host.log().requests.clone();
    assert_eq!(requests.len(), 1, "one call, one request");
    assert_eq!(
        requests[0].extensions,
        vec!["VK_KHR_surface".to_string(), "VK_KHR_win32_surface".to_string()],
        "the guest's order, in the driver's spelling"
    );
    assert!(requests[0].layers.is_empty());
    let application = requests[0].application.as_ref().expect("pApplicationInfo was not NULL");
    assert_eq!(application.application_name.as_deref(), Some("RobloxPlayer"));
    assert_eq!(application.engine_name.as_deref(), Some("Roblox"));
    assert_eq!(application.application_version, 7);
    assert_eq!(application.engine_version, 11);
    assert_eq!(application.api_version, 0x0040_0000, "Vulkan 1.0, as the guest packed it");

    // What the **guest** got: a registry address, not the driver's handle.
    let handle = f.guest.read_u64(out as GuestAddr);
    assert_ne!(handle, 0, "a VkInstance was written");
    let issued = f.vulkan().instance_handles();
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].0 as u64, handle, "the handle the guest holds is the registry's address");
    assert_eq!(issued[0].1, HostInstance::from_token(0));

    // The rewrite log has the enabled direction now, distinct from the advertised one.
    let rewrites = f.vulkan().rewrites();
    assert_eq!(rewrites.len(), 1, "{rewrites:?}");
    assert_eq!(rewrites[0].site, RewriteSite::Enabled);
    assert_eq!(rewrites[0].from, GUEST_SURFACE_EXTENSION);
    assert_eq!(rewrites[0].to, "VK_KHR_win32_surface");
    assert_eq!(rewrites[0].spec_version, None, "a bare name carries no version");

    // And the allocator measurement, as a pair.
    assert_eq!(f.vulkan().allocator_calls(), 1);
    assert_eq!(f.vulkan().allocator_non_null(), 0);
    let report = f.vulkan().report();
    assert!(report.contains("pAllocator: NULL in all 1"), "{report}");
    assert!(report.contains("VkInstance handles issued: 1"), "{report}");
}

/// **A non-null `pAllocator` refuses by name and is counted**, which is the measurement the
/// handoff asked for before anything is built for it.
#[test]
fn a_non_null_allocator_refuses_by_name_and_is_counted() {
    let _serial = serialized();
    let host = RecordingHost::windows_like();
    let f = fixture_with("allocator", host.clone());
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");

    let info = f.create_info(0, &[], &[]);
    let out = f.alloc(8);
    let text = f.refusal(create, [info, 0xCAFE_0000, out, 0]).to_string();
    assert!(text.contains("0xcafe0000"), "the refusal quotes the pointer: {text}");
    assert!(text.contains("VkAllocationCallbacks"), "{text}");
    assert!(text.contains("worker thread"), "and why a trampoline is not enough: {text}");

    assert_eq!(f.vulkan().allocator_calls(), 1, "charged before anything can return early");
    assert_eq!(f.vulkan().allocator_non_null(), 1);
    assert_eq!(f.vulkan().first_allocator(), Some(0xCAFE_0000));
    assert!(host.log().requests.is_empty(), "the driver was never asked");
    assert!(f.vulkan().instance_handles().is_empty(), "and nothing was issued");
    assert!(
        f.vulkan().report().contains("NON-NULL in 1 of 1"),
        "{}",
        f.vulkan().report()
    );
}

/// **A `pNext` chain refuses by name and quotes its address**, rather than being dropped.
///
/// Dropping it would create a different instance from the one the engine asked for and then say
/// `VK_SUCCESS` — which is the exact shape rule 1 names. What is in the chain is a measurement the
/// next session can take, and the refusal gives it the address to decode.
#[test]
fn a_pnext_chain_refuses_and_names_the_address() {
    let _serial = serialized();
    let f = fixture_with("pnext", RecordingHost::windows_like());
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");

    let chain = f.alloc(64);
    let info = f.create_info_raw(1, chain, 0, &[], &[]);
    let out = f.alloc(8);
    let text = f.refusal(create, [info, 0, out, 0]).to_string();
    assert!(text.contains(&format!("{chain:#x}")), "the address to decode next: {text}");
    assert!(text.contains("VkDebugUtilsMessengerCreateInfoEXT"), "{text}");
}

/// **A `pCreateInfo` whose `sType` is wrong refuses**, because every field after it would be read
/// at an offset belonging to a different structure.
#[test]
fn a_wrong_structure_type_refuses_before_anything_is_read_from_it() {
    let _serial = serialized();
    let host = RecordingHost::windows_like();
    let f = fixture_with("stype", host.clone());
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");

    let info = f.create_info_raw(2, 0, 0, &[], &[]); // VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO
    let out = f.alloc(8);
    let text = f.refusal(create, [info, 0, out, 0]).to_string();
    assert!(text.contains("VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO"), "{text}");
    assert!(text.contains("sType"), "{text}");
    assert!(host.log().requests.is_empty(), "the driver was never asked");
}

/// **A wild `pCreateInfo` refuses at `admit` and never reaches the driver.**
///
/// Global Constraint 11's Critical case, and the whole reason the decode goes through `GuestMem`:
/// a `const VkInstanceCreateInfo *` the guest invented, dereferenced by an NVIDIA driver, is a
/// host access violation reachable from guest data. The refusal here is `AbiError::BadPointer`,
/// which names the symbol, the argument and the reason the range was not admitted.
#[test]
fn a_wild_create_info_pointer_is_refused_by_admit_and_never_reaches_the_driver() {
    let _serial = serialized();
    let host = RecordingHost::windows_like();
    let f = fixture_with("wild", host.clone());
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");

    let out = f.alloc(8);
    let error = f.refusal(create, [0xDEAD_0000_0000, 0, out, 0]);
    let text = error.to_string();
    assert!(text.contains("0xdead00000000"), "the refusal quotes the pointer: {text}");
    assert!(host.log().requests.is_empty(), "the driver was never asked");
    assert_eq!(f.guest.read_u64(out as GuestAddr), 0, "and nothing was written back");
}

/// **A count past the bound refuses rather than allocating what the guest asked for.**
#[test]
fn an_extension_count_past_the_bound_refuses() {
    let _serial = serialized();
    let f = fixture_with("bound", RecordingHost::windows_like());
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");

    // A create-info that claims far more names than it has, which is what a hostile or corrupt
    // guest produces and what an unbounded read would turn into a host allocation.
    let info = f.create_info(0, &[], &["VK_KHR_surface"]);
    f.guest.write_u32(info as GuestAddr + 48, 1_000_000);
    let out = f.alloc(8);
    let text = f.refusal(create, [info, 0, out, 0]).to_string();
    assert!(text.contains("1000000"), "{text}");
    assert!(text.contains(&MAX_ENABLED_NAMES.to_string()), "{text}");
    assert!(text.contains("guest-controlled host allocation"), "{text}");
}

/// **A driver that refuses hands the guest its own `VkResult`, unchanged.**
///
/// Not a refusal, and not a different negative number: `VK_ERROR_INCOMPATIBLE_DRIVER` is something
/// the engine has a branch for, and substituting anything else sends it down the wrong one.
#[test]
fn a_driver_failure_reaches_the_guest_verbatim() {
    let _serial = serialized();
    let host = RecordingHost::failing(VK_ERROR_INCOMPATIBLE_DRIVER);
    let f = fixture_with("driverfail", host.clone());
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");

    let info = f.create_info(0, &[], &[]);
    let out = f.alloc(8);
    let result = f.call(create, [info, 0, out, 0]).expect("it must complete, with a failure code");
    assert_eq!(result as i32, VK_ERROR_INCOMPATIBLE_DRIVER);
    assert_eq!(f.guest.read_u64(out as GuestAddr), 0, "no handle was written");
    assert!(f.vulkan().instance_handles().is_empty(), "and none was registered");
    assert_eq!(
        f.vulkan().driver_failures(),
        vec![("vkCreateInstance".to_string(), VK_ERROR_INCOMPATIBLE_DRIVER)]
    );
}

/// **A loader with no host refuses by name, and names what an embedding must do.**
#[test]
fn a_loader_with_no_host_refuses_and_names_set_host() {
    let _serial = serialized();
    let f = fixture_without_host("nohost");
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");
    let info = f.create_info(0, &[], &[]);
    let out = f.alloc(8);
    let text = f.refusal(create, [info, 0, out, 0]).to_string();
    assert!(text.contains("Vulkan::set_host"), "{text}");
    assert!(text.contains("GfxVulkanHost"), "and the one this workspace ships: {text}");
    assert!(
        !text.contains("VK_SUCCESS\""),
        "it must not have answered anything: {text}"
    );
}

// ======================================================= vkGetInstanceProcAddr on a real instance

/// **With a real instance, the driver decides: a thunk for what it has, NULL for what it has
/// not — and never a host function pointer.**
///
/// The two NULLs stay distinguishable in the census ([`ProcAnswer::NullFromDriver`] against
/// [`ProcAnswer::NullPerSpecification`]), which is what lets a reader say whether a missing
/// `vkCreateAndroidSurfaceKHR` was this layer's table being short or the driver genuinely not
/// having it — the question stage 3 opens with.
#[test]
fn a_real_instance_resolves_through_the_driver_and_still_returns_guest_thunks() {
    let _serial = serialized();
    let host = RecordingHost::windows_like();
    let f = fixture_with("procaddr", host.clone());
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");

    let info = f.create_info(0, &[], &[GUEST_SURFACE_EXTENSION]);
    let out = f.alloc(8);
    assert_eq!(f.call(create, [info, 0, out, 0]).expect("create") as i32, VK_SUCCESS);
    let instance = f.guest.read_u64(out as GuestAddr);

    // A command the driver has: a **guest** thunk, inside this boundary's thunk region.
    let destroy = f.proc_addr(entry_point, instance, "vkDestroyInstance").expect("a lookup");
    assert_ne!(destroy, 0);
    assert!(
        f.boundary.symbol_at(destroy as GuestAddr).is_some(),
        "{destroy:#x} must be a slot in this boundary's own thunk region and not a host address"
    );
    assert_eq!(
        f.vulkan().thunk_for("vkDestroyInstance"),
        Some(destroy as GuestAddr),
        "and the host side of the map agrees with what the guest was handed"
    );

    // A command the driver does not have: NULL, on the driver's authority.
    assert_eq!(f.proc_addr(entry_point, instance, "vkCreateRayTracingPipelinesKHR").expect("a lookup"), 0);

    let requests = f.vulkan().requests();
    let answers: Vec<(&str, ProcAnswer)> =
        requests.iter().map(|r| (r.name.as_str(), r.answer)).collect();
    assert!(
        answers.contains(&("vkCreateRayTracingPipelinesKHR", ProcAnswer::NullFromDriver)),
        "the driver's NULL is recorded as the driver's: {answers:?}"
    );
    assert!(
        answers
            .iter()
            .any(|(name, answer)| *name == "vkDestroyInstance"
                && matches!(answer, ProcAnswer::Thunk(_))),
        "{answers:?}"
    );
    // The driver was asked about exactly those two, with the token it issued.
    assert_eq!(
        host.log().procs_asked,
        vec![
            (HostInstance::from_token(0), "vkDestroyInstance".to_string()),
            (HostInstance::from_token(0), "vkCreateRayTracingPipelinesKHR".to_string()),
        ]
    );

    // Calling the thunk it handed out still refuses by name: `vkDestroyInstance` is not one of
    // the commands this layer implements, and the refusal says which stage it is and what that
    // stage covers.
    let text = f.refusal(destroy, [instance, 0, 0, 0]).to_string();
    assert!(text.contains("vkDestroyInstance"), "{text}");
    assert!(text.contains("stage 4"), "{text}");
    assert!(text.contains("Nothing has been"), "{text}");
}

/// **A handle one byte off a registry slot names nothing**, which is the check the whole registry
/// exists for.
///
/// Without the alignment test, `(at - base) / 16` turns any address in the region into a valid
/// index, so a guest that computed `instance + 4` would operate on the instance.
#[test]
fn a_handle_off_a_registry_slot_boundary_is_refused() {
    let _serial = serialized();
    let f = fixture_with("offslot", RecordingHost::windows_like());
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");
    let info = f.create_info(0, &[], &[]);
    let out = f.alloc(8);
    assert_eq!(f.call(create, [info, 0, out, 0]).expect("create") as i32, VK_SUCCESS);
    let instance = f.guest.read_u64(out as GuestAddr);

    for offset in [1u64, 4, 8, 15] {
        let text = f.refusal(entry_point, [instance + offset, f.cstr("vkDestroyInstance"), 0, 0])
            .to_string();
        assert!(
            text.contains("not a `VkInstance` this layer issued"),
            "{offset} bytes past a handle must not name it: {text}"
        );
    }
    // And the real handle still works, so the test is not passing because everything refuses.
    assert_ne!(f.proc_addr(entry_point, instance, "vkDestroyInstance").expect("a lookup"), 0);
}

// ============================================================================ the live tests

/// **A real `VkInstance`, on this machine's real Vulkan driver, created by translated ARM64 code
/// branching through a guest thunk.**
///
/// This is the evidence stage 2a owes. Everything above it can be satisfied by a shim that
/// marshals correctly into a host that is not a driver; this one cannot, and the proof is that the
/// instance is asked afterwards for a **physical device name** — a string only a driver can
/// produce, through the same `HostInstance` token the guest's handle resolves to.
///
/// It prints what came back, because the task that produced it asked for exactly that.
#[test]
#[ignore = "opens the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_real_vulkan_instance_is_created_through_the_guest_path() {
    require_gate();
    let _serial = serialized();
    let host = omni_gfx::GfxVulkanHost::load().expect(
        "this machine must have a Vulkan loader: the gate was set, so a missing driver is a \
         failure and not a skip",
    );
    let f = fixture_with("live-create", host.clone());
    let entry_point = f.entry_point();

    // The bootstrap the engine executes, and then the two calls it makes.
    let enumerate = f.resolve(entry_point, "vkEnumerateInstanceExtensionProperties");
    let create = f.resolve(entry_point, "vkCreateInstance");

    // 1. The engine looks for its surface extension in the real driver's list.
    let count_at = f.alloc(4);
    assert_eq!(
        f.call(enumerate, [0, count_at, 0, 0]).expect("the count call") as i32,
        VK_SUCCESS
    );
    let count = f.read_u32(count_at) as usize;
    assert!(count > 0, "a real loader reports at least VK_KHR_surface");
    let properties_at = f.alloc(count * EXTENSION_PROPERTIES_BYTES);
    assert_eq!(
        f.call(enumerate, [0, count_at, properties_at, 0]).expect("the array call") as i32,
        VK_SUCCESS
    );
    let shown = f.read_extension_properties(properties_at, count);
    let names: Vec<&str> = shown.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&GUEST_SURFACE_EXTENSION),
        "the engine will not proceed without {GUEST_SURFACE_EXTENSION}; the driver reported: \
         {names:?}"
    );

    // 2. It creates the instance, enabling what it just found.
    let application = f.application_info("RobloxPlayer", "Roblox", 0x0040_0000);
    let info =
        f.create_info(application, &[], &["VK_KHR_surface", GUEST_SURFACE_EXTENSION]);
    let out = f.alloc(8);
    let result = f.call(create, [info, 0, out, 0]).expect("vkCreateInstance must complete");
    assert_eq!(
        result as i32,
        VK_SUCCESS,
        "the driver answered VkResult {result} where VK_SUCCESS was needed",
        result = result as i32
    );
    let handle = f.guest.read_u64(out as GuestAddr);
    assert_ne!(handle, 0);

    // 3. **The proof that something was created.** A device name is a string only a driver has.
    let issued = f.vulkan().instance_handles();
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].0 as u64, handle);
    let device = host
        .first_device_name(issued[0].1)
        .expect("the instance the guest holds must be one this host can use")
        .expect("this machine has a physical device");
    assert!(!device.is_empty());

    // 4. And `vkGetInstanceProcAddr` on the real instance goes to the real driver.
    let destroy = f.proc_addr(entry_point, handle, "vkDestroyInstance").expect("a lookup");
    assert_ne!(destroy, 0, "a real driver has vkDestroyInstance");
    assert!(
        f.boundary.symbol_at(destroy as GuestAddr).is_some(),
        "and what the guest got is a thunk in this boundary, not the driver's function pointer"
    );
    let absent = f
        .proc_addr(entry_point, handle, "vkThisIsNotAVulkanCommand")
        .expect("a lookup of a name no driver has");
    assert_eq!(absent, 0, "the driver's NULL, forwarded");

    let rewrites = f.vulkan().rewrites();
    assert!(
        rewrites.iter().any(|r| r.site == RewriteSite::Advertised && r.to == GUEST_SURFACE_EXTENSION),
        "the advertised substitution must be recorded: {rewrites:?}"
    );
    assert!(
        rewrites.iter().any(|r| r.site == RewriteSite::Enabled && r.from == GUEST_SURFACE_EXTENSION),
        "and the enabled one: {rewrites:?}"
    );

    eprintln!("\n=== stage 2a live evidence ===");
    eprintln!("host: {host:?}");
    eprintln!("the driver reported {count} instance extension(s); the guest was shown:");
    for (name, version) in &shown {
        eprintln!("    {name} (specVersion {version})");
    }
    eprintln!("vkCreateInstance returned VkResult {}", result as i32);
    eprintln!("the guest's VkInstance handle is {handle:#x} -> {:?}", issued[0].1);
    eprintln!("that instance's first physical device is: {device}");
    eprintln!("\n{}", f.vulkan().report());
}

/// **With a real driver and no window, `pAllocator` is still NULL in every call this path makes.**
///
/// Kept separate from the headline test so that the `pAllocator` counters are read on a run where
/// nothing else could have charged them.
#[test]
#[ignore = "opens the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_real_driver_run_records_its_allocator_observations() {
    require_gate();
    let _serial = serialized();
    let host = omni_gfx::GfxVulkanHost::load().expect("this machine must have a Vulkan loader");
    let f = fixture_with("live-alloc", host);
    let entry_point = f.entry_point();
    let create = f.resolve(entry_point, "vkCreateInstance");

    assert_eq!(f.vulkan().allocator_calls(), 0, "nothing has been observed yet");
    let info = f.create_info(0, &[], &["VK_KHR_surface"]);
    let out = f.alloc(8);
    assert_eq!(f.call(create, [info, 0, out, 0]).expect("create") as i32, VK_SUCCESS);
    assert_eq!(f.vulkan().allocator_calls(), 1);
    assert_eq!(f.vulkan().allocator_non_null(), 0);

    let report = f.vulkan().report();
    assert!(report.contains("pAllocator: NULL in all 1"), "{report}");
    eprintln!("\n{report}");
}
