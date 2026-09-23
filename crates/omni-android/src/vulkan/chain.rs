//! **`pNext` chains of flat structures: walked out of the guest, carried as bytes, written back.**
//!
//! # Every other module here refuses a chain, and why this one admits some
//!
//! A `pNext` chain is a list of structures, each identified only by its `sType`, and copying one
//! means knowing its layout. So every `vkCreate*` in this layer refuses a non-null `pNext` by name
//! rather than dropping it. This module admits **exactly the structures a run has measured** and
//! that are *flat*: after `sType` and `pNext`, nothing but 32-bit scalars. No pointer, no `size_t`,
//! no padding before the members -- so the aarch64 LP64 layout the guest wrote and the x86-64 LLP64
//! layout the driver reads are the same bytes, [`physical`](super::physical)'s argument one
//! structure along, and a member block travels as the bytes it is. A structure that is not in
//! [`FLAT_STRUCTURES`] still refuses, naming its `sType`, its address and its place in the chain.
//!
//! # The guest's `pNext` values never reach the driver
//!
//! [`ChainLink`] carries an `sType` and a member block and no pointer at all. The host rebuilds the
//! chain in its own memory, in the guest's order, linked by its own `pNext` values; for a query,
//! the member blocks it answers are written back into the guest's structures at their own
//! addresses, with the guest's `sType` and `pNext` untouched.
//!
//! # What was measured
//!
//! The engine's device bring-up, `0x2590580`..`0x2590818` in `libroblox.so`:
//! `vkGetPhysicalDeviceFeatures2KHR` with a `VkPhysicalDeviceSamplerYcbcrConversionFeatures`
//! chained, a second call with a `VkPhysicalDeviceExtendedDynamicStateFeaturesEXT`, and then
//! `vkCreateDevice` with those two and a `VkPhysicalDeviceMultiviewFeatures` in its chain, each
//! prepended only when the engine found its extension in the device's list.

use omni_mem::GuestAddr;

use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::ChainLink;
use super::instance::guest_pointer;
use super::Site;

/// One structure a chain may carry: flat, measured, and named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlatStructure {
    /// Its `VkStructureType`.
    pub s_type: u32,
    /// Its name in `vulkan_core.h`, for refusals and for `omni-gfx`'s size assertion.
    pub name: &'static str,
    /// How many bytes of members follow `pNext`, which sits at 8 -- **not** counting the tail
    /// padding that rounds the structure up to its alignment of 8, which is nobody's to read or
    /// write.
    pub member_bytes: usize,
}

impl FlatStructure {
    /// `sizeof` the structure: the 16-byte `sType`/`pNext` header, the members, and tail padding
    /// to the alignment of 8 that `pNext` gives it. `omni-gfx` asserts this against `ash`'s
    /// `size_of`, which is generated from `vk.xml`.
    #[must_use]
    pub const fn size(&self) -> usize {
        (CHAIN_HEADER_BYTES + self.member_bytes).next_multiple_of(8)
    }
}

/// `sType`, four bytes of padding, and `pNext`.
pub const CHAIN_HEADER_BYTES: usize = 16;

/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2`: the structure `vkGetPhysicalDeviceFeatures2`
/// is handed, which heads its chain rather than being in it.
pub const STYPE_PHYSICAL_DEVICE_FEATURES_2: u32 = 1_000_059_000;

/// `sizeof(VkPhysicalDeviceFeatures2)`: the header and a `VkPhysicalDeviceFeatures` (220 bytes of
/// `VkBool32`), 236 rounded up to 8.
pub const PHYSICAL_DEVICE_FEATURES_2_BYTES: usize = 240;

/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2`: the question
/// `vkGetPhysicalDeviceImageFormatProperties2` is handed.
pub const STYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2: u32 = 1_000_059_004;

/// `sizeof(VkPhysicalDeviceImageFormatInfo2)`: the header, then `format`, `type`, `tiling`,
/// `usage` and `flags` -- five 32-bit members, 36 rounded up to 8.
pub const PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2_BYTES: usize = 40;

/// `VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2`: the answer's head.
pub const STYPE_IMAGE_FORMAT_PROPERTIES_2: u32 = 1_000_059_003;

/// `sizeof(VkImageFormatProperties2)`: the header and a 32-byte `VkImageFormatProperties`.
pub const IMAGE_FORMAT_PROPERTIES_2_BYTES: usize = 48;

/// The structures a chain may carry, each **MEASURED** in the engine's device bring-up.
///
/// ```text
/// VkPhysicalDeviceMultiviewFeatures                  1000053001  multiview, multiviewGeometryShader,
///                                                                multiviewTessellationShader
/// VkPhysicalDeviceSamplerYcbcrConversionFeatures     1000156004  samplerYcbcrConversion
/// VkPhysicalDeviceExtendedDynamicStateFeaturesEXT    1000267000  extendedDynamicState
/// VkSamplerYcbcrConversionImageFormatProperties     1000156005  combinedImageSamplerDescriptorCount
/// ```
///
/// Every member is a `VkBool32` or, in the last, a `uint32_t` -- the engine chains that one to
/// `VkImageFormatProperties2` when it asks about a YCbCr format (`0x2590fb8`..`0x2591030`).
/// Adding a structure here is a claim that it is flat, and it is the kind of claim a layout table
/// cannot check -- so each new entry wants its `vk.xml` definition read, and `omni-gfx`'s
/// assertion extended to it.
pub const FLAT_STRUCTURES: [FlatStructure; 4] = [
    FlatStructure {
        s_type: 1_000_053_001,
        name: "VkPhysicalDeviceMultiviewFeatures",
        member_bytes: 12,
    },
    FlatStructure {
        s_type: 1_000_156_004,
        name: "VkPhysicalDeviceSamplerYcbcrConversionFeatures",
        member_bytes: 4,
    },
    FlatStructure {
        s_type: 1_000_267_000,
        name: "VkPhysicalDeviceExtendedDynamicStateFeaturesEXT",
        member_bytes: 4,
    },
    FlatStructure {
        s_type: 1_000_156_005,
        name: "VkSamplerYcbcrConversionImageFormatProperties",
        member_bytes: 4,
    },
];

/// How many structures one chain may hold.
///
/// An allocation bound and a cycle bound at once: a chain is a guest linked list, and one that
/// points back into itself never ends. The measured chains hold at most three; eight is room with
/// nothing behind it, and reaching it is a refusal naming the number rather than a truncation --
/// a chain cut short is a feature silently not enabled.
pub const MAX_CHAIN_LINKS: usize = 8;

/// The admitted structure with this `sType`, if there is one.
#[must_use]
pub fn flat_structure(s_type: u32) -> Option<&'static FlatStructure> {
    FLAT_STRUCTURES.iter().find(|known| known.s_type == s_type)
}

/// Walk a guest `pNext` chain from `first`, returning each structure's address and its link.
///
/// `field` is how the refusals name where the chain hangs (`pFeatures->pNext`), and `argument`
/// is the register that carried the structure it hangs from, for `admit`'s blame.
pub(super) fn read_chain(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    field: &str,
    first: u64,
    argument: usize,
) -> AbiResult<Vec<(GuestAddr, ChainLink)>> {
    let mut links: Vec<(GuestAddr, ChainLink)> = Vec::new();
    let mut next = first;
    while next != 0 {
        if links.len() == MAX_CHAIN_LINKS {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with a `{field}` chain longer than \
                 {MAX_CHAIN_LINKS} structures -- the next one is at {next:#x}. A chain is a guest \
                 linked list, and one this long is either a cycle or something no run has shown; \
                 cutting it short would silently leave out whatever the rest of it asks for",
                caller = at.caller
            )));
        }
        let link_at = guest_pointer(at, field, next)?;
        let header = c.mem().read_bytes(link_at, CHAIN_HEADER_BYTES, c.blame(argument))?;
        let s_type = u32::from_le_bytes(header[0..4].try_into().expect("four bytes"));
        let Some(known) = flat_structure(s_type) else {
            let admitted: Vec<String> = FLAT_STRUCTURES
                .iter()
                .map(|known| format!("{} ({})", known.name, known.s_type))
                .collect();
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with a `{field}` chain whose \
                 structure {index} (at {link_at:#x}) has `sType` {s_type}. This layer carries only \
                 flat structures a run has measured -- {} -- because copying any other means \
                 knowing a layout it does not, and dropping it would quietly leave out what it \
                 asks for",
                admitted.join(", "),
                caller = at.caller,
                index = links.len()
            )));
        };
        let body =
            c.mem().read_bytes(link_at + CHAIN_HEADER_BYTES as GuestAddr, known.member_bytes, c.blame(argument))?;
        links.push((link_at, ChainLink { s_type, body }));
        next = u64::from_le_bytes(header[8..16].try_into().expect("eight bytes"));
    }
    Ok(links)
}

/// Write the host's answer for each structure of a queried chain back into the guest, members
/// only: `sType` and `pNext` stay the guest's.
///
/// The host answers in place, so the shapes can only differ if it resized a member block -- which
/// is refused by name rather than written short or long.
pub(super) fn write_back(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    addresses: &[GuestAddr],
    answered: &[ChainLink],
    argument: usize,
) -> AbiResult<()> {
    for (index, (link_at, link)) in addresses.iter().zip(answered).enumerate() {
        let expected = flat_structure(link.s_type).map(|known| known.member_bytes);
        if expected != Some(link.body.len()) {
            return Err(at.refuse(format!(
                "the host answered `{call}` with {got} member bytes for chain structure {index} \
                 (`sType` {s_type}), and this layer carries {expected:?}. Writing them would \
                 either leave part of the guest's structure stale or write past its end",
                got = link.body.len(),
                s_type = link.s_type
            )));
        }
        c.mem().write_bytes(*link_at + CHAIN_HEADER_BYTES as GuestAddr, &link.body, c.blame(argument))?;
    }
    Ok(())
}
