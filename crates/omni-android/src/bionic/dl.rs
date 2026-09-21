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
//! # `dlopen` and `dlsym` answer for the libraries **this layer is**, and refuse to load a file
//!
//! Phase 2 refused all three, on the argument that "returning a plausible handle you cannot
//! honour is worse than refusing". That argument is right about a handle to a **file**, and it is
//! wrong about the case the engine actually exercises, which M3's gate found at
//! `init_array[3096]`:
//!
//! ```text
//! h = dlopen("libc.so", RTLD_NOW);      // 0x2112eac
//! if (h) {                              // CBZ x0
//!     f = dlsym(h, "getauxval");        // 0x2112ec0
//!     if (f) f(AT_HWCAP);               // MOV x0, #0x10 ; BLR x8
//! }
//! ```
//!
//! That is not loading anything. `libc.so` is already loaded on a device and `dlopen` of it is a
//! lookup, not a load — and here, *this layer is `libc.so`*: it supplies `getauxval`, at an
//! address the guest can branch to, because every bound import has a thunk slot and the backend
//! dispatches a `BLR` to one exactly as it dispatches a `BL`. Refusing turned the engine's
//! **atomics feature detection** (D26, and the `AT_HWCAP` argument in `procenv`) into a stopped
//! run, and returning null would have made it silently take the no-LSE arm without anything
//! recording that a choice had been made — the same failure D22 refused to let a `Default` make.
//!
//! So:
//!
//! | call | answer |
//! |---|---|
//! | `dlopen(NULL, ..)` | a handle for the **global** scope, which is what `dlopen(NULL)` means |
//! | `dlopen("libc.so", ..)` and the other libraries the guest's own `DT_VERNEED` names | a handle for that library |
//! | `dlopen` of anything else | **NULL**, with `dlerror` set — this runtime does not have that library, which is a fact, and the guest's own null test is what the compiler emitted for it |
//! | `dlsym(handle, name)` | the thunk address of `name`, if this layer supplies it *in that handle's scope*; NULL otherwise |
//! | `dlclose(handle)` | 0. Nothing was loaded, so nothing is unloaded, and the libraries this layer is cannot be unloaded |
//!
//! **The scope is the guest's own statement, not a list here.** `.gnu.version_r` attributes 345
//! of `libroblox.so`'s 565 imports to `libc.so`, 56 to `libm.so`, 6 to `libdl.so` and leaves 158
//! unversioned; that attribution arrives with each [`SymbolRequest`] while the loader relocates
//! and is kept on the slot ([`Slot::library`](crate::Slot::library)). So
//! `dlsym(libc_handle, "getauxval")` succeeds because *this binary says* `getauxval` comes from
//! `libc.so`, and `dlsym(libc_handle, "eglGetProcAddress")` fails for the same reason.
//!
//! **An [`Unbound`](crate::Binding::Unbound) slot is not a `dlsym` hit**, and that is where phase
//! 2's argument survives intact: its address exists so a *direct call* names the symbol, and
//! handing it back through `dlsym` would convert a lookup the guest is prepared to see fail into a
//! pointer it will call thousands of initializers later.
//!
//! **What is still refused is loading a file.** A `dlopen` of a path this layer does not supply
//! answers NULL rather than a refusal, because NULL is the *true* answer — the library is not
//! here — and because every caller of `dlopen` has a null test. The engine's own
//! `dlopen("libcamera2ndk.so")` at `0x243883c` is exactly that shape.
//!
//! `dlerror` is per **thread**, as bionic's is, and is cleared by reading — the idiom
//! `dlerror(); p = dlsym(..); if (dlerror())` depends on both halves.
//!
//! These three are on the **exit path** rather than the fast one, because each needs the
//! boundary's symbol table and [`ImportCall`] deliberately cannot reach it (D18 makes that a type
//! property). None of them calls guest code. They are not hot: the engine makes fourteen direct
//! `dlopen` calls in the whole image.
//!
//! [`SymbolRequest`]: omni_elf::loader::SymbolRequest

use omni_mem::GuestAddr;

use crate::boundary::{GuestArg, ImportCall, ReentrantCall};
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

use super::active;
use super::view::{GuestView, DL_PHDR_INFO_BYTES};

/// The tag every handle this layer issues carries in its top bits.
///
/// **Not a plausible pointer, on purpose.** A guest that passes a `dlopen` handle somewhere a
/// pointer was expected must fault rather than read something, and a guest that hands this layer
/// a handle it did not issue must be told so rather than having an index read out of it. The
/// low bits are an index into [`SCOPES`]-shaped space: 0 is the global scope and *n* is the
/// *n*-th library in the boundary's sorted library list.
const HANDLE_TAG: u64 = 0xD10D_0000_0000_0000;
/// The mask that separates the tag from the scope index.
const HANDLE_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

thread_local! {
    // A `thread_local!` invocation carries no rustdoc, so the documentation is here: bionic keeps
    // the `dlerror` string in thread-local storage, and the idiom `dlerror(); p = dlsym(..); if
    // (dlerror())` is only correct if two threads cannot see each other's message.
    static DL_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn set_dlerror(message: String) {
    DL_ERROR.with(|slot| *slot.borrow_mut() = Some(message));
}

fn take_dlerror() -> Option<String> {
    DL_ERROR.with(|slot| slot.borrow_mut().take())
}

/// The scope a handle names, or `None` if it names nothing this layer issued.
fn scope_of(boundary: &crate::Boundary, handle: u64) -> Option<Option<String>> {
    if handle & !HANDLE_MASK != HANDLE_TAG {
        return None;
    }
    let index = handle & HANDLE_MASK;
    if index == 0 {
        return Some(None);
    }
    let libraries: Vec<&str> = boundary.libraries().into_iter().collect();
    libraries.get(index as usize - 1).map(|name| Some((*name).to_string()))
}

/// Read a guest C string, or `None` when it is null, wild or unterminated.
fn read_name(mem: &GuestMem, pointer: u64, blame: Blame<'_>) -> Option<String> {
    let at = usize::try_from(pointer).ok().filter(|&p| p != 0)?;
    let bytes = mem.cstr(at, blame).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `void *dlopen(const char *filename, int flags)`
///
/// See the module documentation. On the exit path because it needs the boundary's symbol table.
pub(super) fn dlopen(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (filename, flags) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?)
    };
    let _ = flags; // `RTLD_NOW`/`RTLD_LAZY` describe when to bind; everything here is already bound
    let symbol = c.symbol().to_string();
    let address = c.address();
    let _state = active(&symbol, address)?;
    let blame = Blame::new(&symbol, address, 0);
    let mem = c.mem().clone();
    let boundary = std::sync::Arc::clone(c.boundary());

    let handle = if filename == 0 {
        // `dlopen(NULL)` is the global scope, and it is what `RTLD_DEFAULT` means too.
        HANDLE_TAG
    } else {
        match read_name(&mem, filename, blame) {
            None => {
                set_dlerror(format!(
                    "dlopen: the library name at {filename:#x} is not a readable string"
                ));
                0
            }
            Some(name) => {
                let libraries: Vec<&str> = boundary.libraries().into_iter().collect();
                match libraries.iter().position(|candidate| *candidate == name) {
                    Some(index) => HANDLE_TAG | (index as u64 + 1),
                    None => {
                        // **NULL, not a refusal.** This runtime does not have that library, which
                        // is a fact about it, and `dlopen` returning NULL for a library that is
                        // not present is the answer every caller has a branch for.
                        set_dlerror(format!(
                            "dlopen failed: library \"{name}\" not found. This runtime supplies \
                             {}, which are the libraries libroblox.so's own DT_VERNEED names",
                            libraries.join(", ")
                        ));
                        0
                    }
                }
            }
        }
    };
    c.ret(|mut r| r.u64(handle));
    Ok(())
}

/// `void *dlsym(void *handle, const char *symbol)`
///
/// Returns the symbol's **thunk address**, which is an address the guest can branch to: the
/// boundary registers every slot with the CPU context, and a `BLR` to one is dispatched exactly
/// as a `BL` to one is. That is what makes this an implementation rather than a handle nobody can
/// honour.
pub(super) fn dlsym(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (handle, name) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let symbol = c.symbol().to_string();
    let address = c.address();
    let _state = active(&symbol, address)?;
    let blame = Blame::new(&symbol, address, 1);
    let mem = c.mem().clone();
    let boundary = std::sync::Arc::clone(c.boundary());

    let Some(scope) = scope_of(&boundary, handle) else {
        // **A refusal, not NULL.** A handle this layer never issued is not "symbol not found":
        // it is the guest passing something that is not a handle, or a handle from a `dlopen`
        // this layer refused, and answering NULL would let that be read as an absent optional
        // capability.
        return Err(AbiError::Refused {
            symbol,
            address,
            why: format!(
                "the guest passed the handle {handle:#x} to `dlsym`, and this layer never issued \
                 it. Every handle it issues carries the tag {HANDLE_TAG:#x}; a value without it \
                 came from somewhere else, and NULL would be indistinguishable from `dlsym`'s \
                 ordinary \"no such symbol\""
            ),
        });
    };
    let Some(wanted) = read_name(&mem, name, blame) else {
        set_dlerror(format!("dlsym: the symbol name at {name:#x} is not a readable string"));
        c.ret(|mut r| r.u64(0));
        return Ok(());
    };
    let found = boundary.lookup(scope.as_deref(), &wanted).map(|slot| slot.address as u64);
    match found {
        Some(at) => {
            c.ret(|mut r| r.u64(at));
        }
        None => {
            set_dlerror(format!(
                "dlsym failed: undefined symbol \"{wanted}\"{}",
                scope.map_or(String::new(), |lib| format!(" in \"{lib}\""))
            ));
            c.ret(|mut r| r.u64(0));
        }
    }
    Ok(())
}

/// `int dlclose(void *handle)`
///
/// Zero, meaning success. Nothing was opened, so nothing is closed — and the libraries this layer
/// *is* cannot be unloaded, which is also true of `libc.so` on a device, where `dlclose` on it
/// decrements a reference count that never reaches zero.
pub(super) fn dlclose(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let handle = c.args().next_u64()?;
    let symbol = c.symbol().to_string();
    let address = c.address();
    let _state = active(&symbol, address)?;
    let boundary = std::sync::Arc::clone(c.boundary());
    if scope_of(&boundary, handle).is_none() {
        return Err(AbiError::Refused {
            symbol,
            address,
            why: format!(
                "the guest asked to close the handle {handle:#x}, which this layer never issued. \
                 Answering 0 would tell the guest a handle it holds has been released"
            ),
        });
    }
    c.ret(|mut r| r.i32(0));
    Ok(())
}

/// `char *dlerror(void)`
///
/// The last failure on **this thread**, cleared by reading, as bionic's is. NULL when there has
/// not been one. The message is interned in this thread's scratch buffer, so it stays valid until
/// the next call that uses the scratch — which is the same lifetime bionic gives it.
pub(super) fn dlerror(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let state = active(c.symbol(), c.address())?;
    let Some(message) = take_dlerror() else {
        c.ret().u64(0);
        return Ok(());
    };
    let at = {
        let view = super::enter(c, &state);
        // A message longer than the scratch is refused rather than truncated -- a truncated
        // diagnostic is a believable wrong answer, and `put_scratch` already says so.
        view.put_scratch(message.as_bytes())?
    };
    c.ret().u64(at as u64);
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
