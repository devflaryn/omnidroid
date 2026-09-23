//! **The two Vulkan commands the engine reaches first, forwarded to a real driver.**
//!
//! Stage 1 opened the loader and recorded what `libroblox.so` asked for. What it recorded is the
//! whole of this module's scope, and nothing wider: the decoded bootstrap at guest `0x02595160`
//! resolves `vkCreateInstance` and `vkEnumerateInstanceExtensionProperties` and nothing else
//! before it branches. Every other thunk `vkGetInstanceProcAddr` hands out still refuses naming
//! the function, because the next batch is a **measurement** away — the census records the names
//! the engine asks for — and not a guess about a header.
//!
//! # The four things a call has to survive before a driver sees it
//!
//! Both handlers are the same shape, and the order is not arbitrary.
//!
//! 1. **The allocator is observed and then refused.** `pAllocator` is measured *first*, before
//!    anything can return early, because the question "does Roblox pass allocation callbacks?" has
//!    an answer that only a run can give and a refusal that happened for some other reason would
//!    lose it. See [`Vulkan::allocator_calls`](super::Vulkan::allocator_calls).
//! 2. **Every guest pointer goes through `admit`.** D4 makes a validated guest pointer a host
//!    pointer, which is what makes forwarding cheap and is exactly what makes an *unvalidated* one
//!    lethal: a wild `const VkInstanceCreateInfo *` reaching an NVIDIA driver is a host access
//!    violation reachable from guest data, which Global Constraint 11 calls Critical. Every read
//!    here is through [`crate::GuestMem`], which is the one copy of that check.
//! 3. **The structure is decoded, not forwarded.** See [`InstanceRequest`] — the extension list
//!    has to change, so it is rebuilt host-side, and guest memory is never written to achieve it.
//! 4. **The substitution is recorded.** [`rewrite`](super::rewrite) holds the argument.
//!
//! # Why the decode is layout arithmetic and not a `#[repr(C)]` struct
//!
//! A `#[repr(C)]` mirror of `VkInstanceCreateInfo` would be laid out by **this host's** compiler
//! for **this host's** target, and the claim that needs to hold is about the *guest's* layout on
//! aarch64 LP64. The two agree here — every member is a fixed-width integer or a pointer, `long`
//! never appears in Vulkan's structures and `size_t` is 8 bytes on both aarch64 LP64 and x86-64
//! Windows LLP64 — but "they agree" is the thing to state and check rather than to rely on a
//! compiler to reproduce. So the offsets are named constants with the C declaration beside them,
//! [`the_structure_offsets_are_the_ones_the_specification_fixes`] states them in one place, and a
//! reader can check them against `vulkan_core.h` by eye.
//!
//! [`InstanceRequest`]: super::InstanceRequest
//! [`the_structure_offsets_are_the_ones_the_specification_fixes`]:
//!     #tests::the_structure_offsets_are_the_ones_the_specification_fixes

use std::sync::Arc;

use omni_mem::GuestAddr;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

use super::host::{ApplicationInfo, DriverAnswer, HostExtension, HostInstance, InstanceRequest};
use super::{Site, Vulkan};

// ----------------------------------------------------------------- the specification's numbers

/// `VK_SUCCESS`.
pub const VK_SUCCESS: i32 = 0;
/// `VK_INCOMPLETE` — the caller's array was too small, and what fitted was written.
pub const VK_INCOMPLETE: i32 = 5;

/// `VK_MAX_EXTENSION_NAME_SIZE`.
pub const MAX_EXTENSION_NAME_SIZE: usize = 256;

/// `sizeof(VkExtensionProperties)`: `char[256]` then a `uint32_t`, alignment 4.
pub const EXTENSION_PROPERTIES_BYTES: usize = MAX_EXTENSION_NAME_SIZE + 4;

/// `sizeof(VkInstanceCreateInfo)` on any LP64 or LLP64 target.
///
/// ```text
/// VkStructureType            sType;                     //  0  (u32, then 4 bytes of padding)
/// const void                *pNext;                     //  8
/// VkInstanceCreateFlags      flags;                     // 16  (u32, then 4 bytes of padding)
/// const VkApplicationInfo   *pApplicationInfo;          // 24
/// uint32_t                   enabledLayerCount;         // 32  (then 4 bytes of padding)
/// const char *const         *ppEnabledLayerNames;       // 40
/// uint32_t                   enabledExtensionCount;     // 48  (then 4 bytes of padding)
/// const char *const         *ppEnabledExtensionNames;   // 56
/// ```
pub const INSTANCE_CREATE_INFO_BYTES: usize = 64;

/// `sizeof(VkApplicationInfo)`.
///
/// ```text
/// VkStructureType   sType;                //  0  (u32, then 4 bytes of padding)
/// const void       *pNext;                //  8
/// const char       *pApplicationName;     // 16
/// uint32_t          applicationVersion;   // 24  (then 4 bytes of padding)
/// const char       *pEngineName;          // 32
/// uint32_t          engineVersion;        // 40
/// uint32_t          apiVersion;           // 44
/// ```
pub const APPLICATION_INFO_BYTES: usize = 48;

/// `VK_STRUCTURE_TYPE_APPLICATION_INFO`.
const STYPE_APPLICATION_INFO: u32 = 0;
/// `VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO`.
const STYPE_INSTANCE_CREATE_INFO: u32 = 1;

/// How many layer or extension names one `vkCreateInstance` may name.
///
/// **An allocation bound, and the reason it exists is that the count is a guest value.**
/// `enabledExtensionCount` is a `uint32_t` the guest writes; multiplying it by eight and reading
/// that many pointers is a guest-controlled allocation of up to 32 GB, which Global Constraint 11
/// is about. 64 is far above what any renderer enables — `omni-gfx` itself enables two — so
/// reaching it means the guest is asking for something nobody has seen, and that is a finding the
/// refusal names rather than an allocation the host attempts.
pub const MAX_ENABLED_NAMES: usize = 64;

/// How many live `VkInstance` handles one [`Vulkan`] will issue.
///
/// A renderer needs one. Four leaves room for a loader that creates, destroys and recreates
/// without the registry being the thing that stops it. A fifth is a refusal naming the call, not a
/// `VK_ERROR_OUT_OF_HOST_MEMORY` — that code means the *host* is out of memory, and this host is
/// not; saying so would send the engine looking in the wrong place.
pub const MAX_INSTANCES: usize = 4;

/// Bytes of the boundary's data area one issued `VkInstance` occupies.
///
/// Sixteen, and for [`ndk::handles`](crate::ndk)'s reason rather than for a reason of its own: a
/// slot holds a magic and its index, so a guest that dereferences the handle reads something
/// recognisable in a dump instead of faulting. Nothing reads either back — the **address** is the
/// identity, and `Instances::index_of` is what checks it.
pub const INSTANCE_SLOT_BYTES: usize = 16;

/// What an issued instance slot holds, for a reader of a memory dump. Nothing reads it back.
pub const INSTANCE_SLOT_MAGIC: u64 = 0x004F_4D4E_564B_4900; // "\0OMNVKI\0"

/// The symbol [`Vulkan::bind_into`](super::Vulkan::bind_into) declares the registry under.
///
/// Spelled with `::` so it cannot collide with anything in `libroblox.so`'s `.dynstr`, exactly as
/// [`proc_slot_symbol`](super::proc_slot_symbol) is. It is never shown to the guest.
pub const INSTANCE_REGISTRY_SYMBOL: &str = "vulkan::instances";

// ------------------------------------------------------------------------------- the registry

/// The `VkInstance` handles this layer has issued, addressed by arena slot.
///
/// # Why the guest never receives the driver's handle
///
/// `VkInstance` is a **dispatchable** handle: on every real implementation it is a pointer to a
/// structure whose first word is the loader's dispatch table, and the driver dereferences it. So
/// the value is a host pointer, and handing it to the guest and casting it back later would make
/// every later Vulkan call a host dereference of a number the guest last held — which is Global
/// Constraint 11's Critical case, reachable by the guest storing a handle and adding one to it.
///
/// What the guest gets instead is an address in the boundary's own data area, and what comes back
/// out of [`Instances::get`] is a [`HostInstance`] **token** rather than a pointer, because the
/// driver's pointer never leaves the [`VulkanHost`](super::VulkanHost) implementation at all. Two
/// checks, each catching what the other cannot: this one catches a handle the guest invented, and
/// the token catches a host that has been swapped underneath.
///
/// The alignment test is the whole point, and it is [`ndk::handles::Slots`](crate::ndk)'s
/// argument verbatim: without it `(at - base) / INSTANCE_SLOT_BYTES` turns *any* address in the
/// region into a valid index, so a guest that computed `instance + 4` would operate on the
/// instance.
#[derive(Debug)]
pub(super) struct Instances {
    base: GuestAddr,
    entries: Vec<Option<HostInstance>>,
}

impl Instances {
    /// A registry of [`MAX_INSTANCES`] slots starting at `base`.
    pub(super) fn new(base: GuestAddr) -> Instances {
        let mut entries = Vec::with_capacity(MAX_INSTANCES);
        entries.resize_with(MAX_INSTANCES, || None);
        Instances { base, entries }
    }

    /// One past the last address this registry covers.
    pub(super) fn end(&self) -> GuestAddr {
        self.base + self.entries.len() * INSTANCE_SLOT_BYTES
    }

    /// The guest address of slot `index`.
    pub(super) fn address_of(&self, index: usize) -> GuestAddr {
        self.base + index * INSTANCE_SLOT_BYTES
    }

    /// The slot a guest handle names, **checked**: in range, and on a slot boundary.
    pub(super) fn index_of(&self, at: GuestAddr) -> Option<usize> {
        if at < self.base || at >= self.end() {
            return None;
        }
        let offset = at - self.base;
        (offset % INSTANCE_SLOT_BYTES == 0).then_some(offset / INSTANCE_SLOT_BYTES)
    }

    /// Put `token` in the lowest free slot and return its index and guest address.
    pub(super) fn insert(&mut self, token: HostInstance) -> Option<(usize, GuestAddr)> {
        let index = self.entries.iter().position(Option::is_none)?;
        self.entries[index] = Some(token);
        Some((index, self.address_of(index)))
    }

    /// What a live guest handle names.
    pub(super) fn get(&self, at: GuestAddr) -> Option<HostInstance> {
        self.index_of(at).and_then(|index| self.entries[index])
    }

    /// Every live handle, with the address the guest holds for it.
    pub(super) fn iter(&self) -> impl Iterator<Item = (GuestAddr, HostInstance)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.map(|token| (self.address_of(index), token)))
    }

    /// How many slots hold an instance.
    pub(super) fn live(&self) -> usize {
        self.entries.iter().filter(|slot| slot.is_some()).count()
    }
}

// ------------------------------------------------------------------------------- the handlers

/// `VK_API_VERSION_1_3`: `VK_MAKE_API_VERSION(0, 1, 3, 0)`.
pub const ANDROID_13_LOADER_VERSION: u32 = (1 << 22) | (3 << 12);

/// `VkResult vkEnumerateInstanceVersion(uint32_t *pApiVersion)`
///
/// **`VK_API_VERSION_1_3`, the Android 13 loader's own answer.** `frameworks/native/vulkan/
/// libvulkan/api.cpp` at `android-13.0.0_r1` is `*pApiVersion = VK_API_VERSION_1_3; return
/// VK_SUCCESS;` -- unconditionally: this reports the *loader's* instance-level version, and the
/// driver's is applied later, when `vkCreateInstance` clamps the requested `apiVersion` to it. So
/// the host is not asked here, and what the device can really do still comes from the physical
/// device's properties, which are the host driver's. MEASURED reader: the engine's instance
/// bootstrap, straight after `vkEnumerateInstanceLayerProperties`.
pub(super) fn enumerate_instance_version(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    let version_at = guest_pointer(at, "pApiVersion", args[0])?;
    if version_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `vkEnumerateInstanceVersion` from {caller:#x} with \
             `pApiVersion = NULL`, so there is nowhere to put the version",
            caller = at.caller
        )));
    }
    c.mem().write_u32(version_at, ANDROID_13_LOADER_VERSION, c.blame(0))?;
    c.ret().i32(0);
    Ok(())
}

/// `VkResult vkEnumerateInstanceLayerProperties(uint32_t *pPropertyCount, VkLayerProperties *pProperties)`
///
/// **None: `*pPropertyCount = 0` and `VK_SUCCESS`, and that is this app's true answer.** An
/// Android loader offers an app the layers in its own native-library directory (and, for a
/// debuggable app, the ones debug settings name) and nothing else; this APK's `lib/arm64-v8a/`
/// holds no `libVkLayer_*`, and the release build is not debuggable. So the answer does not
/// depend on the host at all, and the host driver's own layers -- validation layers included --
/// must never reach the guest through here. MEASURED reader: the engine's instance bootstrap,
/// after `vkEnumerateInstanceExtensionProperties`.
///
/// `pProperties` is never written: with nothing to report, both halves of the two-call protocol
/// answer the same count, and a capacity of any size holds all zero of them (`VK_SUCCESS`, not
/// `VK_INCOMPLETE`). A NULL `pPropertyCount` refuses, as the extension enumerator's does.
pub(super) fn enumerate_instance_layer_properties(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    let count_at = guest_pointer(at, "pPropertyCount", args[0])?;
    if count_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `vkEnumerateInstanceLayerProperties` from {caller:#x} with \
             `pPropertyCount = NULL`. The specification requires it to be a valid pointer in both \
             halves of the two-call protocol, so there is nowhere to put the answer (zero layers)",
            caller = at.caller
        )));
    }
    c.mem().write_u32(count_at, 0, c.blame(0))?;
    c.ret().i32(0);
    Ok(())
}

/// `VkResult vkEnumerateInstanceExtensionProperties(const char *pLayerName,
/// uint32_t *pPropertyCount, VkExtensionProperties *pProperties)`
///
/// **The call that decides whether the engine will try Vulkan at all.** `libroblox.so` resolves it
/// second, immediately after `vkCreateInstance`, and the list it gets back is where it looks for
/// `VK_KHR_android_surface`. A list without that name ends the Vulkan path, silently, on the
/// engine's own branch — so this is the call the substitution exists for, and
/// [`rewrite`](super::rewrite) is where the argument that a rewrite must be recorded is made.
///
/// The two-call protocol is the specification's and is implemented as written: `pProperties` NULL
/// writes the count and answers `VK_SUCCESS`; `pProperties` non-NULL writes
/// `min(*pPropertyCount, available)` entries, writes back how many it wrote, and answers
/// `VK_INCOMPLETE` when that was fewer than there are. `VK_INCOMPLETE` is a *success* code the
/// caller is required to handle, and answering `VK_SUCCESS` for a truncated list would be this
/// layer telling the engine it has seen every extension when it has not.
pub(super) fn enumerate_instance_extension_properties(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    let (layer_pointer, count_pointer, properties_pointer) = (args[0], args[1], args[2]);
    let host = vulkan.require_host(at)?;

    let count_at = guest_pointer(at, "pPropertyCount", count_pointer)?;
    if count_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `vkEnumerateInstanceExtensionProperties` from {caller:#x} with \
             `pPropertyCount = NULL`. The specification requires it to be a valid pointer in both \
             halves of the two-call protocol -- it is the only output when `pProperties` is NULL, \
             and the input capacity when it is not -- so there is nowhere to put an answer. \
             Returning `VK_SUCCESS` would tell the engine a count had been written into memory \
             nothing wrote to",
            caller = at.caller
        )));
    }

    let layer = if layer_pointer == 0 {
        None
    } else {
        let layer_at = guest_pointer(at, "pLayerName", layer_pointer)?;
        Some(guest_string(c.mem(), at, "pLayerName", layer_at, c.blame(0))?)
    };

    let driver = match host.instance_extensions(layer.as_deref())? {
        DriverAnswer::Ok(list) => list,
        // The driver's own code, forwarded verbatim. `VK_ERROR_LAYER_NOT_PRESENT` for a layer the
        // host does not have is a real answer to a real question, and the engine has a branch for
        // it; replacing it with a refusal would make this layer lie about a conforming driver.
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result("vkEnumerateInstanceExtensionProperties", result);
            c.ret().i32(result);
            return Ok(());
        }
    };
    let shown = vulkan.advertise_extensions(&host, &driver, at.caller)?;

    // `pProperties == NULL`: the count alone, and `VK_SUCCESS` whatever it is.
    let available = u32::try_from(shown.len()).map_err(|_| {
        at.refuse(format!(
            "the host driver reported {} instance extensions, which does not fit the `uint32_t` \
             `pPropertyCount` is. Nothing this layer can write into that cell would be the \
             driver's count",
            shown.len()
        ))
    })?;
    if properties_pointer == 0 {
        c.mem().write_u32(count_at, available, c.blame(1))?;
        c.ret().i32(VK_SUCCESS);
        return Ok(());
    }

    let properties_at = guest_pointer(at, "pProperties", properties_pointer)?;
    let capacity = c.mem().read_u32(count_at, c.blame(1))?;
    let writing = capacity.min(available) as usize;
    // One `admit`-checked write of the whole array rather than one per entry: the check is the
    // expensive part and a partial write followed by a refusal would leave the guest's array half
    // filled with no count to say how far it got.
    let mut bytes = Vec::with_capacity(writing * EXTENSION_PROPERTIES_BYTES);
    for extension in shown.iter().take(writing) {
        bytes.extend_from_slice(&extension_properties(at, extension)?);
    }
    c.mem().write_bytes(properties_at, &bytes, c.blame(2))?;
    c.mem().write_u32(count_at, writing as u32, c.blame(1))?;
    c.ret().i32(if writing < available as usize { VK_INCOMPLETE } else { VK_SUCCESS });
    Ok(())
}

/// One `VkExtensionProperties`, as the 260 bytes the guest reads.
///
/// The name is NUL-padded to 256 bytes rather than merely NUL-terminated, because the array is a
/// fixed field of a structure the guest may memcmp or copy whole, and leaving the tail as whatever
/// was in the buffer would make two identical extensions compare unequal.
pub(super) fn extension_properties(at: &Site, extension: &HostExtension) -> AbiResult<Vec<u8>> {
    let name = extension.name.as_bytes();
    if name.len() >= MAX_EXTENSION_NAME_SIZE {
        // Reachable from the **host driver** rather than from the guest, which is why it is a
        // refusal and not a `debug_assert!` (`VERIFICATION.md` entry 12): a driver that reports a
        // 256-byte name is non-conformant, and truncating it would hand the engine a plausible
        // extension name that is not one the driver has.
        return Err(at.refuse(format!(
            "the host driver reported an instance extension whose name is {} bytes, and \
             `VkExtensionProperties::extensionName` is `char[{MAX_EXTENSION_NAME_SIZE}]` \
             including its NUL. The name was \"{}\"",
            name.len(),
            extension.name
        )));
    }
    let mut out = vec![0u8; EXTENSION_PROPERTIES_BYTES];
    out[..name.len()].copy_from_slice(name);
    out[MAX_EXTENSION_NAME_SIZE..].copy_from_slice(&extension.spec_version.to_le_bytes());
    Ok(out)
}

/// `VkResult vkCreateInstance(const VkInstanceCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkInstance *pInstance)`
///
/// **The first call in this project that makes a real host object on the guest's behalf.** What
/// comes back through `pInstance` is an address in the boundary's data area, not the driver's
/// handle; [`Instances`] is where the argument for that lives.
///
/// # `pAllocator`, measured before anything is built for it
///
/// A `VkAllocationCallbacks` holds four or five **guest** function pointers. A host driver cannot
/// branch into guest code at all, and the specification explicitly permits it to call
/// `pfnAllocation` from any thread it likes — including its own worker threads, where there is no
/// guest CPU context to enter and no `Boundary` to enter it through. A re-entry trampoline for
/// that would have to be `bind_reentrant` and would still have nowhere to run on a driver thread.
///
/// The handoff's own instruction is to measure first: most engines pass NULL. So a non-null
/// `pAllocator` **refuses by name**, and every call is counted either way —
/// [`Vulkan::allocator_calls`](super::Vulkan::allocator_calls) and
/// [`Vulkan::allocator_non_null`](super::Vulkan::allocator_non_null) are read as a pair, so
/// "Roblox never passed one" is a sentence backed by a count rather than by an absence of
/// refusals.
pub(super) fn create_instance(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    let (create_info_pointer, allocator_pointer, instance_pointer) = (args[0], args[1], args[2]);

    // **First, and before anything can return early.** The whole value of this counter is that it
    // is charged on a line no other failure can skip -- `VERIFICATION.md` entry 15's shape.
    vulkan.note_allocator("vkCreateInstance", allocator_pointer);
    if allocator_pointer != 0 {
        return Err(at.refuse(format!(
            "the guest called `vkCreateInstance` from {caller:#x} with \
             `pAllocator = {allocator_pointer:#x}`, a non-null `VkAllocationCallbacks *`. Those \
             are **guest** function pointers, and a host Vulkan driver cannot branch into \
             translated ARM64 at all -- the specification also lets it call `pfnAllocation` from \
             its own worker threads, where this runtime has no guest CPU context to enter and no \
             boundary to enter it through. Passing NULL to the driver instead would be this layer \
             silently discarding an allocator the engine asked to be used, and every later \
             `pfnFree` the engine expects would never arrive. This refusal is the measurement the \
             handoff asked for: `Vulkan::allocator_non_null()` counts it",
            caller = at.caller
        )));
    }

    let host = vulkan.require_host(at)?;
    let create_info_at = guest_pointer(at, "pCreateInfo", create_info_pointer)?;
    let instance_at = guest_pointer(at, "pInstance", instance_pointer)?;
    for (name, pointer) in [("pCreateInfo", create_info_at), ("pInstance", instance_at)] {
        if pointer == 0 {
            return Err(at.refuse(format!(
                "the guest called `vkCreateInstance` from {caller:#x} with `{name} = NULL`, which \
                 the specification requires to be a valid pointer. There is no instance to \
                 describe or nowhere to put the one that was made, and a `VK_SUCCESS` here would \
                 be the plausible answer Global Constraint 1 forbids",
                caller = at.caller
            )));
        }
    }

    let request = decode_create_info(c, at, vulkan, &host, create_info_at)?;
    match host.create_instance(&request)? {
        DriverAnswer::Failed(result) => {
            // A real driver refusing for a real reason. Forwarded, not replaced, and recorded so a
            // run that "did nothing" can be told from one the driver declined.
            vulkan.note_driver_result("vkCreateInstance", result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let (index, handle) = vulkan.register_instance(at, token)?;
            // The magic is for a reader of a dump and nothing reads it back; `Instances` says why.
            // It is written through `GuestMem` like every other write in this crate, so the data
            // area's own protection is what permits it rather than this handler's opinion.
            let mut slot = [0u8; INSTANCE_SLOT_BYTES];
            slot[..8].copy_from_slice(&INSTANCE_SLOT_MAGIC.to_le_bytes());
            slot[8..].copy_from_slice(&(index as u64).to_le_bytes());
            c.mem().write_bytes(handle, &slot, c.blame(2))?;
            c.mem().write_u64(instance_at, handle as u64, c.blame(2))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// Decode `VkInstanceCreateInfo` out of guest memory, apply the substitution, and record it.
fn decode_create_info(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    host: &Arc<dyn super::VulkanHost>,
    create_info_at: GuestAddr,
) -> AbiResult<InstanceRequest> {
    let blame = c.blame(0);
    let info = c.mem().read_bytes(create_info_at, INSTANCE_CREATE_INFO_BYTES, blame)?;
    let u32_at = |offset: usize| {
        u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four bytes"))
    };
    let u64_at = |offset: usize| {
        u64::from_le_bytes(info[offset..offset + 8].try_into().expect("eight bytes"))
    };

    let stype = u32_at(0);
    if stype != STYPE_INSTANCE_CREATE_INFO {
        return Err(at.refuse(format!(
            "the guest called `vkCreateInstance` from {caller:#x} with a `pCreateInfo` whose \
             `sType` is {stype}, and `VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO` is \
             {STYPE_INSTANCE_CREATE_INFO}. Every field after it would be read at an offset that \
             belongs to a different structure, so forwarding this to a driver would hand it a \
             pointer count taken from somewhere that is not a pointer count",
            caller = at.caller
        )));
    }
    let next = u64_at(8);
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `vkCreateInstance` from {caller:#x} with \
             `pCreateInfo->pNext = {next:#x}`. A `pNext` chain is a list of structures this layer \
             would have to know the layout of in order to validate and to rebuild host-side, and \
             it does not -- the one the engine is most likely to put there,
             `VkDebugUtilsMessengerCreateInfoEXT`, carries a **guest** callback that a host driver \
             cannot branch into, which is the same wall `pAllocator` hits. Dropping the chain and \
             creating the instance anyway would be creating a different instance from the one the \
             engine asked for and saying `VK_SUCCESS`. What is in the chain is a measurement: this \
             refusal names the address so the next run can decode it",
            caller = at.caller
        )));
    }

    let application = decode_application_info(c, at, u64_at(24))?;
    let layers =
        decode_names(c, at, "vkCreateInstance", "ppEnabledLayerNames", u32_at(32), u64_at(40))?;
    let requested = decode_names(
        c,
        at,
        "vkCreateInstance",
        "ppEnabledExtensionNames",
        u32_at(48),
        u64_at(56),
    )?;
    // **The rewrite**, and the only place it happens on this path. `rewrite::apply_enabled`
    // returns an owned list; the guest's own array is untouched, which is what "do not mutate
    // guest memory to achieve it" means in practice.
    let extensions = vulkan.enable_extensions(host, &requested, at.caller)?;

    Ok(InstanceRequest { flags: u32_at(16), application, layers, extensions })
}

/// Decode `pApplicationInfo`, which is allowed to be NULL.
fn decode_application_info(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    pointer: u64,
) -> AbiResult<Option<ApplicationInfo>> {
    let application_at = guest_pointer(at, "pApplicationInfo", pointer)?;
    if application_at == 0 {
        // Legal, and **not** the same as a zeroed structure: the specification says a NULL
        // `pApplicationInfo` behaves as though `apiVersion` were `VK_API_VERSION_1_0`, and a
        // driver is entitled to tell the two apart. `Option` is what carries that distinction to
        // the host rather than a `Default`.
        return Ok(None);
    }
    let blame = c.blame(0);
    let bytes = c.mem().read_bytes(application_at, APPLICATION_INFO_BYTES, blame)?;
    let u32_at = |offset: usize| {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
    };
    let u64_at = |offset: usize| {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("eight bytes"))
    };
    let stype = u32_at(0);
    if stype != STYPE_APPLICATION_INFO {
        return Err(at.refuse(format!(
            "the guest's `pCreateInfo->pApplicationInfo` at {application_at:#x} has `sType` \
             {stype}, and `VK_STRUCTURE_TYPE_APPLICATION_INFO` is {STYPE_APPLICATION_INFO}"
        )));
    }
    let next = u64_at(8);
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest's `pCreateInfo->pApplicationInfo->pNext` is {next:#x}. See the refusal for \
             `pCreateInfo->pNext`: the chain's layout is not something this layer knows, and \
             dropping it would create a different instance from the one that was asked for"
        )));
    }
    let name_at = |offset: usize| -> AbiResult<Option<String>> {
        let pointer = guest_pointer(at, "a VkApplicationInfo name", u64_at(offset))?;
        if pointer == 0 {
            return Ok(None);
        }
        Ok(Some(guest_string(c.mem(), at, "a VkApplicationInfo name", pointer, c.blame(0))?))
    };
    Ok(Some(ApplicationInfo {
        application_name: name_at(16)?,
        application_version: u32_at(24),
        engine_name: name_at(32)?,
        engine_version: u32_at(40),
        api_version: u32_at(44),
    }))
}

/// Decode a `const char *const *` of `count` names.
pub(super) fn decode_names(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    field: &str,
    count: u32,
    array: u64,
) -> AbiResult<Vec<String>> {
    if count == 0 {
        // Zero names is a zero-length array, and the specification permits the pointer to be
        // anything at all when the count is zero -- including garbage the engine never cleared.
        // Reading it would refuse a correct program, which is `read_bytes`' own argument for
        // accepting a zero-length `memcpy`.
        return Ok(Vec::new());
    }
    let count = count as usize;
    if count > MAX_ENABLED_NAMES {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} naming {count} entries in \
             `{field}`, and this layer reads at most {MAX_ENABLED_NAMES}. The count is a guest \
             `uint32_t` and the array it indexes is {count} pointers long, so honouring it \
             unbounded would be a guest-controlled host allocation (Global Constraint 11). This is \
             a refusal rather than a truncation because a truncated extension list is a plausible \
             extension list",
            caller = at.caller
        )));
    }
    let array_at = guest_pointer(at, field, array)?;
    if array_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `{field} = NULL` and a \
             count of {count}. There are no names to read, and enabling nothing while the engine \
             believes it enabled {count} things is the silent divergence this layer exists to \
             refuse",
            caller = at.caller
        )));
    }
    let blame = c.blame(0);
    let pointers = c.mem().read_bytes(array_at, count * 8, blame)?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let raw = u64::from_le_bytes(
            pointers[index * 8..index * 8 + 8].try_into().expect("eight bytes"),
        );
        let name_at = guest_pointer(at, field, raw)?;
        if name_at == 0 {
            return Err(at.refuse(format!(
                "the guest's `{field}[{index}]` is NULL. The specification requires every entry to \
                 be a null-terminated UTF-8 string, so there is no name to enable"
            )));
        }
        out.push(guest_string(c.mem(), at, field, name_at, c.blame(0))?);
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ small helpers

/// A guest `u64` as a [`GuestAddr`], or a refusal naming the field.
///
/// `GuestAddr` is `usize`, so on a 32-bit host this is a real conversion that can fail. It is a
/// refusal rather than a truncation for the reason every refusal in this file is: a truncated
/// pointer is a plausible pointer, and the driver would dereference it.
pub(super) fn guest_pointer(at: &Site, field: &str, value: u64) -> AbiResult<GuestAddr> {
    GuestAddr::try_from(value).map_err(|_| {
        at.refuse(format!(
            "the guest passed {value:#x} as `{field}`, which is not an address in this guest's \
             space"
        ))
    })
}

/// A guest pointer that the specification requires to be non-NULL, or a refusal naming it.
///
/// **Written once because stage 4 has eighteen of them**, and because the wrong thing to do is the
/// same every time: `VK_SUCCESS` with nothing written into the guest's output leaves the guest
/// reading whatever was already in its own variable as a `VkSwapchainKHR`, a `VkFence` or an image
/// index. The refusal names the parameter and the call, so the message says which of a call's
/// several pointers was the missing one.
pub(super) fn require_pointer(
    at: &Site,
    call: &str,
    field: &str,
    value: u64,
) -> AbiResult<GuestAddr> {
    let address = guest_pointer(at, field, value)?;
    if address == 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `{field} = NULL`, which the \
             specification requires to be a valid pointer. There is nothing to read there or \
             nowhere to put the answer, and returning as though there were would leave the guest \
             reading whatever was already in its own variable as this call's result",
            caller = at.caller
        )));
    }
    Ok(address)
}

/// Observe a `pAllocator`, and refuse a non-null one naming the call.
///
/// **Counted first, refused second**, which is the order every call that takes one uses:
/// [`Vulkan::allocator_calls`](super::Vulkan::allocator_calls) is the denominator and a call that
/// refused before being counted would be missing from it. The measurement is the point — zero
/// non-null allocators across a thousand calls is evidence that this engine passes none, and it is
/// evidence only because the denominator is real.
pub(super) fn refuse_allocator(
    vulkan: &Arc<Vulkan>,
    at: &Site,
    call: &str,
    allocator: u64,
) -> AbiResult<()> {
    vulkan.note_allocator(call, allocator);
    if allocator == 0 {
        return Ok(());
    }
    Err(at.refuse(format!(
        "the guest called `{call}` from {caller:#x} with `pAllocator = {allocator:#x}`. See the \
         same refusal on `vkCreateInstance`: a `VkAllocationCallbacks` holds **guest** function \
         pointers, a host driver cannot branch into translated ARM64, and there is no guest CPU \
         context on the driver's own worker threads where the specification explicitly permits it \
         to call them. Passing NULL to the driver instead would silently discard an allocator the \
         engine asked to be used, and on a destroy it would free with a different allocator from \
         the one that allocated. `Vulkan::allocator_non_null()` counts this",
        caller = at.caller
    )))
}

/// A guest C string as text, **refusing** rather than replacing what is not UTF-8.
///
/// [`GuestMem::cstr`] answers bytes because a guest is under no obligation to hand over UTF-8 and
/// a path in the device's encoding is a legitimate argument elsewhere. Here it is not: these
/// bytes are Vulkan extension and layer names, which are compared against the driver's own ASCII
/// spellings, and `String::from_utf8_lossy` would turn one bad byte into `U+FFFD` and then fail
/// the comparison against a name the guest may well have spelled correctly in some other
/// encoding. A refusal naming the field says which name could not be read; a replacement
/// character says `VK_ERROR_EXTENSION_NOT_PRESENT` for a reason nothing records.
pub(super) fn guest_string(
    mem: &GuestMem,
    at: &Site,
    field: &str,
    address: GuestAddr,
    blame: Blame<'_>,
) -> AbiResult<String> {
    let bytes = mem.cstr(address, blame)?;
    String::from_utf8(bytes).map_err(|err| {
        at.refuse(format!(
            "the guest's `{field}` at {address:#x} is not UTF-8: {err}. Vulkan layer and extension \
             names are ASCII and are matched against the driver's own spelling, so replacing the \
             bad byte would produce a name that silently matches nothing"
        ))
    })
}

/// The refusal a thunk produces for a Vulkan function this stage does not implement.
///
/// Stage 1's words, updated by one sentence rather than rewritten: what has changed is *which*
/// functions are implemented, and the reason a refusal is not a `VK_SUCCESS` has not changed at
/// all.
pub(super) fn unimplemented(
    at: &Site,
    name: &str,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiError {
    let registers = args
        .iter()
        .enumerate()
        .map(|(index, value)| format!("x{index}={value:#x}"))
        .collect::<Vec<_>>()
        .join(", ");
    AbiError::Refused {
        symbol: name.to_string(),
        address: at.address,
        why: format!(
            "the guest called the Vulkan function `{name}` from {caller:#x}, through the thunk \
             `vkGetInstanceProcAddr` handed out for it at {address:#x}. It was passed \
             {registers}. **This is stage 5**: `omni_android::vulkan` forwards the instance \
             bootstrap (`vkEnumerateInstanceExtensionProperties`, `vkCreateInstance`), the \
             surface substitution (`vkCreateAndroidSurfaceKHR`) and its end \
             (`vkDestroySurfaceKHR`), the ten queries a renderer \
             makes in order to choose a physical device, `vkCreateDevice`, `vkGetDeviceQueue`, \
             `vkGetDeviceProcAddr` and `vkDestroyDevice`, the whole presentation spine -- the \
             swapchain and its images, \
             image views, semaphores and fences, command pools and command buffers, \
             `vkCmdPipelineBarrier` and `vkCmdClearColorImage`, and \
             `vkAcquireNextImageKHR`/`vkQueueSubmit`/`vkQueuePresentKHR` with the two idle waits \
             -- and everything between a device and a draw: device memory with \
             `vkMapMemory`/`vkUnmapMemory` and the two bind calls, buffers, images and samplers, \
             shader modules, pipeline layouts, render passes, framebuffers, pipeline caches \
             with `vkGetPipelineCacheData`, \
             `vkCreateGraphicsPipelines` and `vkCreateComputePipelines`, the four descriptor \
             calls with `vkUpdateDescriptorSets` and descriptor update templates, query pools \
             with the GPU timer's two commands, and \
             the thirteen `vkCmd*` a textured draw records. All to a real host driver through \
             `VulkanHost`, and `vkCmdDispatch`, `vkCmdCopyImage` and `vkCmdBlitImage`. It \
             implements no other Vulkan command: **indirect \
             dispatch, events, sparse binding, buffer views, secondary command buffers and most \
             `2`-suffixed variants are not here**, and which of them gets \
             built is decided by what the engine's census actually asks for rather than by a \
             header (D17). Nothing has been created, nothing has been destroyed, and no status \
             code has been invented -- which is the whole reason this is an error and not a \
             `VK_SUCCESS`. The ordered list of what was asked for is `Vulkan::names()`, and this \
             call is `Vulkan::first_call()`; the next batch to implement is whichever names that \
             list holds",
            caller = at.caller,
            address = at.address
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure sizes are the ones the specification fixes**, stated where a reader can
    /// check them against `vulkan_core.h` by eye.
    ///
    /// This is the assertion the module header promises instead of a `#[repr(C)]` mirror: the
    /// numbers here describe the **guest's** aarch64 LP64 layout, and a host compiler reproducing
    /// them would be evidence about the host.
    #[test]
    fn the_structure_offsets_are_the_ones_the_specification_fixes() {
        assert_eq!(INSTANCE_CREATE_INFO_BYTES, 64);
        assert_eq!(APPLICATION_INFO_BYTES, 48);
        assert_eq!(MAX_EXTENSION_NAME_SIZE, 256);
        assert_eq!(EXTENSION_PROPERTIES_BYTES, 260, "char[256] then a uint32_t, aligned to 4");
        assert_eq!(VK_SUCCESS, 0);
        assert_eq!(VK_INCOMPLETE, 5);
        assert_eq!(STYPE_APPLICATION_INFO, 0);
        assert_eq!(STYPE_INSTANCE_CREATE_INFO, 1);
    }

    /// **A `VkExtensionProperties` is NUL-padded to the full array**, not merely terminated.
    ///
    /// The tail matters: the guest may copy or compare the whole 260 bytes, and leaving whatever
    /// was in the buffer behind the NUL would make two identical extensions compare unequal.
    #[test]
    fn an_extension_properties_is_nul_padded_and_carries_the_driver_s_version() {
        let at = Site { symbol: "test".to_string(), address: 0, caller: 0 };
        let bytes = extension_properties(
            &at,
            &HostExtension { name: "VK_KHR_surface".to_string(), spec_version: 25 },
        )
        .expect("an ordinary name");
        assert_eq!(bytes.len(), EXTENSION_PROPERTIES_BYTES);
        assert_eq!(&bytes[..14], b"VK_KHR_surface");
        assert!(bytes[14..MAX_EXTENSION_NAME_SIZE].iter().all(|&b| b == 0), "the tail is NUL");
        assert_eq!(
            u32::from_le_bytes(bytes[MAX_EXTENSION_NAME_SIZE..].try_into().expect("four bytes")),
            25
        );
    }

    /// **A name that does not fit is refused rather than truncated**, and the refusal quotes it.
    #[test]
    fn a_name_that_does_not_fit_the_array_is_refused() {
        let at = Site { symbol: "test".to_string(), address: 0, caller: 0 };
        let name = "V".repeat(MAX_EXTENSION_NAME_SIZE);
        let error = extension_properties(&at, &HostExtension { name, spec_version: 1 })
            .expect_err("a 256-byte name cannot be NUL-terminated in char[256]");
        let text = error.to_string();
        assert!(text.contains("256 bytes"), "{text}");
        assert!(text.contains("extensionName"), "{text}");
    }

    /// **A handle off a slot boundary names nothing**, which is the check the whole registry is
    /// for — `ndk::handles`' argument, and the reason `index_of` is a function and not a division.
    #[test]
    fn an_instance_handle_off_a_slot_boundary_names_nothing() {
        let mut instances = Instances::new(0x1_0000);
        let (index, first) = instances.insert(HostInstance::from_token(7)).expect("a free slot");
        assert_eq!(index, 0);
        assert_eq!(first, 0x1_0000);
        assert_eq!(instances.get(first), Some(HostInstance::from_token(7)));
        for offset in 1..INSTANCE_SLOT_BYTES {
            assert_eq!(
                instances.index_of(first + offset),
                None,
                "{offset} bytes past a handle must not name it"
            );
        }
        assert_eq!(instances.index_of(0x1_0000 - INSTANCE_SLOT_BYTES), None);
        assert_eq!(instances.index_of(instances.end()), None);
        assert_eq!(instances.get(instances.end() - INSTANCE_SLOT_BYTES), None, "in range, unused");
    }

    /// **The registry fills and then refuses**, rather than overwriting a live instance.
    #[test]
    fn a_full_registry_refuses_rather_than_reusing_a_live_slot() {
        let mut instances = Instances::new(0x2_0000);
        for token in 0..MAX_INSTANCES as u64 {
            instances.insert(HostInstance::from_token(token)).expect("a free slot");
        }
        assert_eq!(instances.live(), MAX_INSTANCES);
        assert_eq!(instances.insert(HostInstance::from_token(99)), None);
        let live: Vec<(GuestAddr, HostInstance)> = instances.iter().collect();
        assert_eq!(live.len(), MAX_INSTANCES);
        assert_eq!(live[0].1, HostInstance::from_token(0));
        assert_eq!(live[MAX_INSTANCES - 1].0, instances.end() - INSTANCE_SLOT_BYTES);
    }
}
