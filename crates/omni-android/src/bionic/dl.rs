//! `libdl`: `dl_iterate_phdr` faithfully, and four refusals that name what they would have to
//! invent.
//!
//! # `dl_iterate_phdr` is the one that cannot be a stub
//!
//! The C++ runtime in `libroblox.so` is **statically linked**, so the unwinder that services
//! `throw` lives in the guest and finds its `.eh_frame` — 11.5 MB of it — by calling
//! `dl_iterate_phdr` and walking the program headers it is handed. A version that reported no
//! objects would return zero, which is a *success*, and every C++ exception in the engine would
//! then fail to find a landing pad. That is Global Constraint 1's failure shape exactly: a
//! believable answer whose consequence arrives somewhere else entirely.
//!
//! So this enumerates the real loaded image, out of the state
//! [`Bionic::register_image`](super::Bionic::register_image) was given, and **refuses when nothing
//! has been registered** rather than reporting an empty process. A host that has loaded an object
//! and not registered it gets a named error at the first `throw` instead of a broken unwinder.
//!
//! It is on the exit path ([`ReentrantFn`](crate::ReentrantFn)) because it calls a guest callback
//! once per object, and D18 makes that a property of the type rather than a rule.
//!
//! # The other four, and why a handle is the worst thing to hand back
//!
//! `dlopen`, `dlsym` and `dlclose` need surface `omni-platform` does not have: opening a file,
//! mapping it, running a second loader over it. The brief's instruction is blunt and it is the
//! right one — *returning a plausible handle you cannot honour is worse than refusing*. A guest
//! that gets a non-null `dlopen` result will call `dlsym` on it, store what comes back, and call
//! it thousands of initializers later; a guest that gets a *refusal naming the library it asked
//! for* tells whoever reads it which missing piece to build.
//!
//! They are bound rather than left [`Unbound`](crate::Binding::Unbound) for the reason D20 gives
//! for `fprintf`: `Unbound` says "nothing implements this", and a refusal says *which missing
//! piece*, with the guest's own argument in it.
//!
//! `dlerror` is the exception and it is not a stub either. It returns null, which means "no error
//! since the last call" — and that is **true**, because the three calls that could set one refuse
//! instead of returning. The idiom `dlerror(); p = dlsym(...); if (dlerror())` reaches the first
//! call legitimately and never reaches the second.

use omni_mem::GuestAddr;

use crate::boundary::{GuestArg, ImportCall, ReentrantCall};
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

use super::active;
use super::view::{GuestView, DL_PHDR_INFO_BYTES};

/// Read a guest C string for an error message, without ever failing.
///
/// A refusal that is *about* an argument must be able to quote it, and the argument is a guest
/// pointer that may be null, wild or unterminated. Every one of those is a description rather
/// than a second error: the call is being refused either way, and replacing "the library
/// `libfoo.so`" with a bad-pointer error would lose the only useful thing in the message.
fn describe(mem: &GuestMem, pointer: u64, blame: Blame<'_>) -> String {
    if pointer == 0 {
        return "NULL".to_string();
    }
    let Ok(at) = usize::try_from(pointer) else {
        return format!("a pointer at {pointer:#x}, wider than the host's usize");
    };
    match mem.cstr(at, blame) {
        Ok(bytes) => format!("`{}`", String::from_utf8_lossy(&bytes)),
        Err(error) => format!("an unreadable string at {at:#x} ({error})"),
    }
}

/// `void *dlopen(const char *filename, int flags)`
pub(super) fn dlopen(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (filename, flags) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?)
    };
    let what = describe(c.mem(), filename, c.blame(0));
    Err(AbiError::Refused {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: format!(
            "the guest asked to load {what} with flags {flags:#x}. Loading a library at run time \
             needs a file opened by path and a second run of the ELF loader, and `omni-platform` \
             is virtual memory and faults only — it has no files. A handle is refused rather than \
             invented, because a non-null handle would be used for `dlsym` and the value that \
             came back would be carried thousands of initializers past the point it was wrong"
        ),
    })
}

/// `void *dlsym(void *handle, const char *symbol)`
pub(super) fn dlsym(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (handle, name) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let what = describe(c.mem(), name, c.blame(1));
    Err(AbiError::Refused {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: format!(
            "the guest asked for {what} in the handle {handle:#x}. No handle can exist: `dlopen` \
             refuses, and the loaded objects keep no runtime symbol index to search. Returning \
             null would be worse than this error — null is `dlsym`'s ordinary \"not found\", so \
             the guest would treat a missing capability as an absent optional one"
        ),
    })
}

/// `int dlclose(void *handle)`
pub(super) fn dlclose(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let handle = c.args().next_u64()?;
    Err(AbiError::Refused {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: format!(
            "the guest asked to close the handle {handle:#x}, and no handle was ever issued — \
             `dlopen` refuses. Unmapping an object this layer did not open would be a guess about \
             what the guest thinks it holds"
        ),
    })
}

/// `char *dlerror(void)`
///
/// Null, meaning "no error since the last call to `dlerror`", which is true: see the module note.
pub(super) fn dlerror(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    // Nothing is read, but the state has to exist: a handler that ran with no instance published
    // is a host mistake and is reported as one everywhere else in this layer.
    let _ = active(c.symbol(), c.address())?;
    c.ret().u64(0);
    Ok(())
}

/// Where each field of a guest `struct dl_phdr_info` sits. See
/// [`DL_PHDR_INFO_BYTES`](super::DL_PHDR_INFO_BYTES) for the derivation.
mod field {
    pub(super) const ADDR: usize = 0;
    pub(super) const NAME: usize = 8;
    pub(super) const PHDR: usize = 16;
    pub(super) const PHNUM: usize = 24;
    pub(super) const ADDS: usize = 32;
    pub(super) const SUBS: usize = 40;
    pub(super) const TLS_MODID: usize = 48;
    pub(super) const TLS_DATA: usize = 56;
}

/// `int dl_iterate_phdr(int (*callback)(struct dl_phdr_info *, size_t, void *), void *data)`
///
/// Stops at the first callback that returns non-zero and returns that value, which is the
/// documented contract and the one the unwinder relies on — it returns non-zero as soon as it
/// finds the object containing the address it is looking for, and an implementation that carried
/// on would call it once per remaining library for nothing.
pub(super) fn dl_iterate_phdr(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (callback, data) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let symbol = c.symbol().to_string();
    let address = c.address();
    let state = active(&symbol, address)?;

    let target = usize::try_from(callback).ok().filter(|&t| t != 0).ok_or_else(|| {
        AbiError::Refused {
            symbol: symbol.clone(),
            address,
            why: format!(
                "the callback pointer is {callback:#x}, which is not a guest address this \
                 boundary can call"
            ),
        }
    })?;

    let images = state.bionic.images();
    if images.is_empty() {
        return Err(AbiError::Refused {
            symbol: symbol.clone(),
            address,
            why: "no loaded image has been registered with the adapter, and reporting a process \
                  with no objects would be a successful-looking answer: the statically-linked C++ \
                  runtime finds its 11.5 MB of `.eh_frame` through this call, so every guest \
                  `throw` would then fail to find a landing pad. Call `Bionic::register_image` \
                  for each `LoadedObject` before running guest code"
                .to_string(),
        });
    }

    // `adds` is how many objects have ever been added and `subs` how many removed. The unwinder
    // caches on the pair and re-scans when it changes; nothing here can `dlopen` or `dlclose`, so
    // the count is the registry's length and `subs` is zero — which is a fact rather than a
    // placeholder, and it is what makes the guest's cache valid for the life of the process.
    let adds = images.len() as u64;

    // Cloned rather than borrowed: `ReentrantCall::mem` borrows shared and `call_guest` borrows
    // unique. `GuestMem` is an `Arc` and nothing else.
    let mem = c.mem().clone();
    let depth = c.depth();
    let info_at = {
        let view = GuestView::new(&mem, &symbol, address, &state);
        view.dl_info_address(depth)?
    };

    let mut result = 0i32;
    for image in &images {
        write_info(&mem, &symbol, address, info_at, image, adds)?;
        let args = [
            GuestArg::Pointer(info_at),
            GuestArg::Int(DL_PHDR_INFO_BYTES as u64),
            GuestArg::Pointer(data as GuestAddr),
        ];
        let returned = c.call_guest(target, &args, omni_cpu::RunLimit::Unlimited)?.as_i32();
        if returned != 0 {
            result = returned;
            break;
        }
    }
    c.ret(|mut r| r.i32(result));
    Ok(())
}

/// Fill one `dl_phdr_info` in guest memory.
fn write_info(
    mem: &GuestMem,
    symbol: &str,
    address: GuestAddr,
    at: GuestAddr,
    image: &super::GuestImage,
    adds: u64,
) -> AbiResult<()> {
    let blame = Blame::new(symbol, address, 0);
    let mut record = [0u8; DL_PHDR_INFO_BYTES];
    record[field::ADDR..field::ADDR + 8].copy_from_slice(&(image.addr as u64).to_le_bytes());
    record[field::NAME..field::NAME + 8].copy_from_slice(&(image.name as u64).to_le_bytes());
    record[field::PHDR..field::PHDR + 8].copy_from_slice(&(image.phdr as u64).to_le_bytes());
    record[field::PHNUM..field::PHNUM + 2].copy_from_slice(&image.phnum.to_le_bytes());
    record[field::ADDS..field::ADDS + 8].copy_from_slice(&adds.to_le_bytes());
    record[field::SUBS..field::SUBS + 8].copy_from_slice(&0u64.to_le_bytes());
    // D9: there is no `PT_TLS` anywhere in this APK — `ElfImage::parse` refuses one — so a module
    // id of zero, which means "this object has no TLS block", is the measured truth and not a
    // default.
    record[field::TLS_MODID..field::TLS_MODID + 8].copy_from_slice(&0u64.to_le_bytes());
    record[field::TLS_DATA..field::TLS_DATA + 8].copy_from_slice(&0u64.to_le_bytes());
    mem.write_bytes(at, &record, blame)
}
