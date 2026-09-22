//! **The two-call enumeration protocol, written once.**
//!
//! # The idiom, and the two ways to get it wrong silently
//!
//! Almost every Vulkan query that answers a list is called **twice**: once with a null array
//! pointer, which writes the count, and once with a buffer, which fills it. The specification
//! states it as one rule for all of them, and the two failures it admits are both silent.
//!
//! 1. **Writing the host's count into `pPropertyCount` while filling the guest's shorter array.**
//!    The guest sized its buffer from the first call, the driver has since gained a device, and
//!    the layer writes `available` entries into room for `capacity`. That is a **host write past
//!    the end of a guest buffer** — Global Constraint 11, Critical — and it is reachable from
//!    nothing more hostile than a second GPU being plugged in. So the array write here is bounded
//!    by `capacity` and `capacity` alone, and `pPropertyCount` is written back with **what was
//!    written**, never with what was available.
//! 2. **Answering `VK_SUCCESS` for a truncated list.** `VK_INCOMPLETE` is a *success* code the
//!    caller is required to handle, and an engine that deliberately asks for fewer than there are
//!    — a renderer that only ever wants the first format, say — **expects exactly it**. Answering
//!    `VK_SUCCESS` tells that engine it has seen everything there is, and the consequence is a
//!    device chosen from a list that was cut off.
//!
//! Both are invisible in a run that happens to size its buffer correctly, which is every run until
//! one is not. So the rule is in one function, every caller goes through it, and
//! [`Filled`] is what a caller turns into a `VkResult` — or, for the two queries that return
//! `void`, deliberately does not.
//!
//! # `vkGetPhysicalDeviceQueueFamilyProperties` returns `void`, and that is not an oversight
//!
//! Two of the calls that use this idiom have **no return value at all**:
//! `vkGetPhysicalDeviceQueueFamilyProperties` and, in later versions, its `2` variants. There is
//! therefore no `VK_INCOMPLETE` for them, and the specification says what happens instead — the
//! count written back is what fitted, and the caller is expected to notice it is smaller than the
//! one it was given. [`Filled::Incomplete`] is still produced for them, because the *fact* is the
//! same and a diagnostic can print it; what the caller must not do is invent a `VkResult` to carry
//! it, and the type makes that a choice at the call site rather than something that happens by
//! default.
//!
//! # Where the array pointer is validated
//!
//! Through [`GuestMem`](crate::GuestMem), like every other write in this crate, and **once** for
//! the whole array rather than once per element: the `admit` check is the expensive part, and a
//! per-element loop that refused halfway would leave the guest's array partly filled with a count
//! that never got written. The buffer is built host-side first and written in one call, so either
//! the whole array and the count land or neither does.
//!
//! A `capacity` of zero writes **nothing at all**, not a zero-length write: `admit` treats a
//! zero-length access as one byte, so validating an array pointer the guest is entitled to have
//! sized zero would refuse a conforming caller whose empty buffer sits at the end of a mapping.

use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::Site;

/// What one two-call enumeration did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Filled {
    /// `pProperties` was NULL: the count was written and nothing else. Always `VK_SUCCESS` for the
    /// calls that have a result, whatever the count is — including zero.
    CountOnly {
        /// How many there are.
        available: u32,
    },
    /// The array was filled and it was big enough. `VK_SUCCESS`.
    Complete {
        /// How many were written, which is also how many there are.
        written: u32,
    },
    /// The array was filled as far as it went and there are more. `VK_INCOMPLETE` — **not** an
    /// error, and not `VK_SUCCESS` either.
    Incomplete {
        /// How many were written, which is what `pPropertyCount` now says.
        written: u32,
        /// How many there are.
        available: u32,
    },
}

impl Filled {
    /// The `VkResult` this is, for the calls that have one.
    ///
    /// Deliberately **not** used by `vkGetPhysicalDeviceQueueFamilyProperties`, which returns
    /// `void`; this module's documentation says why that is a call-site choice.
    pub(super) fn result(self) -> i32 {
        match self {
            Filled::CountOnly { .. } | Filled::Complete { .. } => super::VK_SUCCESS,
            Filled::Incomplete { .. } => super::VK_INCOMPLETE,
        }
    }
}

/// One two-call query, as the seven facts it takes to run it.
///
/// **A struct rather than seven parameters**, and not only because clippy counts: `count_pointer`
/// and `array_pointer` are both `u64`, and `count_argument` and `array_argument` are both
/// `usize`, so a positional call that transposed either pair would compile and would then read
/// the capacity out of the array pointer. Named fields make that transposition a thing a reader
/// sees.
#[derive(Clone, Copy)]
pub(super) struct Array<'a> {
    /// The Vulkan function, for every refusal this produces.
    pub(super) call: &'a str,
    /// What one entry is called, for the same refusals.
    pub(super) element: &'a str,
    /// `sizeof` one entry.
    pub(super) element_bytes: usize,
    /// The guest's `pPropertyCount`, as it arrived.
    pub(super) count_pointer: u64,
    /// The guest's `pProperties`, as it arrived. Zero is the count-only half of the protocol.
    pub(super) array_pointer: u64,
    /// Which AAPCS64 argument `count_pointer` was, so a refusal from `admit` names the register.
    pub(super) count_argument: usize,
    /// Which AAPCS64 argument `array_pointer` was.
    pub(super) array_argument: usize,
}

/// Run the two-call protocol for one query.
///
/// `elements` is the **flat** byte image of the whole list, [`Array::element_bytes`] long per
/// entry, in the driver's own order — which is kept, because a guest that indexes the array it was
/// given must see the same thing in the same place in both halves of the protocol.
///
/// # Errors
///
/// [`AbiError::Refused`](crate::AbiError::Refused) for a NULL count pointer, for a list whose
/// length does not divide by the element size (a host bug, not a guest one), or for a count that
/// does not fit a `uint32_t`; whatever [`GuestMem`](crate::GuestMem) refuses for a pointer that
/// is not admissible.
pub(super) fn enumerate(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    array: &Array<'_>,
    elements: &[u8],
) -> AbiResult<Filled> {
    let Array { call, element, element_bytes, .. } = *array;
    debug_assert!(element_bytes > 0, "a zero-sized element would make the count meaningless");
    if elements.len() % element_bytes != 0 {
        // **Reachable from the host, not from the guest**, which is why it is a refusal and not a
        // `debug_assert!` (`VERIFICATION.md` entry 12): a `VulkanHost` that answered a blob of the
        // wrong length would otherwise have the remainder silently dropped, and the guest would
        // read a count that does not describe the bytes beside it.
        return Err(at.refuse(format!(
            "the host answered `{call}` with {} bytes of `{element}`, and one `{element}` is              {element_bytes} bytes -- so the list does not divide into entries. Writing the              {whole} whole entries and dropping the remainder would hand the guest an array whose              last element is half of one structure and half of nothing",
            elements.len(),
            whole = elements.len() / element_bytes
        )));
    }
    let count_at = super::instance::guest_pointer(at, "pPropertyCount", array.count_pointer)?;
    if count_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with a NULL count pointer. The              specification requires it to be valid in **both** halves of the two-call protocol --              it is the only output when the array pointer is NULL, and it is the input capacity              when it is not -- so there is nowhere to put an answer and nothing that says how big              the guest's buffer is. `VK_SUCCESS` here would claim a count had been written into              memory nothing wrote to",
            caller = at.caller
        )));
    }

    let available = u32::try_from(elements.len() / element_bytes).map_err(|_| {
        at.refuse(format!(
            "the host answered `{call}` with {} `{element}` entries, which does not fit the              `uint32_t` the count pointer points at. Nothing this layer could write into that cell              would be the driver's count",
            elements.len() / element_bytes
        ))
    })?;

    if array.array_pointer == 0 {
        // The count alone. `VK_SUCCESS` whatever it is -- a count of zero is an answer, not a
        // failure, and it is how a caller discovers there are no present modes it can use.
        c.mem().write_u32(count_at, available, c.blame(array.count_argument))?;
        return Ok(Filled::CountOnly { available });
    }

    let array_at = super::instance::guest_pointer(at, "the array pointer", array.array_pointer)?;
    // **The capacity is the guest's, and it is the only bound on the write.** Reading it here,
    // from the cell the guest owns, is what makes the write below fit the buffer the guest
    // actually has rather than the one it had when it asked for the count.
    let capacity = c.mem().read_u32(count_at, c.blame(array.count_argument))?;
    let writing = capacity.min(available);
    if writing > 0 {
        let bytes = &elements[..writing as usize * element_bytes];
        c.mem().write_bytes(array_at, bytes, c.blame(array.array_argument))?;
    }
    // Written **after** the array, and with what was written rather than with what was available.
    c.mem().write_u32(count_at, writing, c.blame(array.count_argument))?;
    Ok(if writing < available {
        Filled::Incomplete { written: writing, available }
    } else {
        Filled::Complete { written: writing }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three outcomes map to the two `VkResult`s the specification allows, and
    /// `VK_INCOMPLETE` is the one that is a **success**.
    #[test]
    fn a_truncated_enumeration_is_vk_incomplete_and_a_full_one_is_vk_success() {
        assert_eq!(Filled::CountOnly { available: 0 }.result(), super::super::VK_SUCCESS);
        assert_eq!(Filled::CountOnly { available: 9 }.result(), super::super::VK_SUCCESS);
        assert_eq!(Filled::Complete { written: 3 }.result(), super::super::VK_SUCCESS);
        assert_eq!(
            Filled::Incomplete { written: 1, available: 3 }.result(),
            super::super::VK_INCOMPLETE
        );
        assert_ne!(super::super::VK_SUCCESS, super::super::VK_INCOMPLETE);
    }
}
