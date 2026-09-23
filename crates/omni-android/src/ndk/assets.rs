//! `AAssetManager` and `AAsset`: the seven symbols step 13 hands a Java `AssetManager` to.
//!
//! `AAssetManager_fromJava`, `AAssetManager_open`, `AAsset_read`, `AAsset_getLength`,
//! `AAsset_getBuffer`, `AAsset_close`, `AAsset_openFileDescriptor`.
//!
//! `jni-surface.md` §5.2 step 10: `initializeNativeCode` takes a global reference to the
//! `AssetManager` argument and calls `AAssetManager_fromJava` on it, storing the result at
//! `activity->assetManager` (**+0x40**). Everything after that is the engine reading its own
//! content.
//!
//! # Where the bytes come from, and why it is a trait
//!
//! **This crate cannot read an APK and must not learn to.** `omni-apk` is a dev-dependency here,
//! for the real-APK fixtures, and making it an ordinary one would put zip and inflate into the
//! compatibility layer. So [`AssetSource`] is what the *embedding* implements — over
//! `omni_apk::Apk`, whose `assets` are already keyed by exactly the name `AAssetManager_open`
//! takes, or over a directory, or over anything else.
//!
//! An instance with no source refuses `AAssetManager_fromJava` **by name**, which is the same
//! shape as the filesystem root (D23) and the thread host (D24): no default is possible, so there
//! is none, and the refusal says which call supplies one.
//!
//! # `AAsset_getBuffer` is why two of these are on the exit path
//!
//! It returns `const void *` — a pointer the **guest** dereferences — so the asset's bytes have to
//! be in guest memory. That is a mapping change, and task 2's review finding F9 says a handler
//! that reaches `GuestSpace` is `bind_reentrant`; `ImportCall` has no way to reach it at all.
//! `AAsset_close` unmaps, so it is on the exit path too.
//!
//! The bytes are mapped **once per open asset**, on the first `getBuffer`, and the same pointer is
//! returned for every later call — which is `AAsset_getBuffer`'s documented contract and is what
//! makes the pointer safe to store. A second mapping per call would leak one per call and hand
//! the guest a different answer each time.
//!
//! # `AAsset_openFileDescriptor` answers what a device answers: -1 for a compressed asset
//!
//! `AAsset_openFileDescriptor(asset, &start, &length)` gives back a descriptor into the **APK
//! file itself** with the asset's offset and length, and the caller then `mmap`s or `pread`s it.
//! That only works for an asset stored **uncompressed**, and M0 measured this APK: *every* entry
//! is DEFLATED, and exactly one entry in the whole archive is directly mappable — a 1,447-byte
//! icon. There is no offset into any file at which a deflated asset's bytes appear.
//!
//! A device answers **-1** for such an asset: `AAsset_openFileDescriptor` asks the asset, and a
//! compressed asset is the base `Asset::openFileDescriptor`, which is `return -1`
//! (`frameworks/base/libs/androidfw/Asset.cpp`). The engine runs on devices with this very APK,
//! so -1 is what it meets there, and its fallback is `AAsset_read` -- which is what the NDK tells
//! a caller to do. So that is the answer here, for every asset the source says is in no file
//! ([`AssetPlacement::NotInAnyFile`]).
//!
//! An asset **stored** in its package gets what a device gives it (`_FileAsset::
//! openFileDescriptor`): a new read-only descriptor on the package, positioned at 0, with the
//! asset's offset and length in `outStart` and `outLength`. MEASURED: the engine's shader pack,
//! `shaders/shaders_vulkan_mobile.pack`, is stored -- so M0's "every entry is DEFLATED" was true
//! of what it counted and not of this entry. The package is opened at the **guest** path the
//! source names, through the guest's own descriptor table, so the guest reads it, maps it and
//! closes it as it would on a device. The believable wrong answer is still refused: a descriptor
//! on a temporary copy with `start = 0` would work and quietly turn every asset read into a host
//! file write.

use std::sync::Arc;

use omni_mem::{CommitPolicy, GuestAddr, Placement, Protection};

use crate::abi::Args;
use crate::boundary::{ImportCall, ImportFn, ReentrantCall, ReentrantFn};
use crate::error::{AbiError, AbiResult};
use crate::mem::Blame;

use super::{active, Ndk};

/// Where a guest instance's assets come from.
///
/// Implemented by the embedding. The name is the `AAssetManager`-relative one — `shaders/foo.bin`
/// rather than `assets/shaders/foo.bin` — which is exactly the key `omni_apk::Apk` already uses.
///
/// `Send + Sync` because the game thread reads assets and the calling thread may too.
pub trait AssetSource: Send + Sync + core::fmt::Debug {
    /// Read an asset whole, or `None` if there is no such asset.
    ///
    /// Whole rather than streamed: every entry in this APK is DEFLATED (M0), so producing any of
    /// an asset means inflating all of it, and a streaming interface here would be a decompressor
    /// pretending to be a file.
    fn read(&self, name: &[u8]) -> Option<Vec<u8>>;

    /// Where an asset's bytes lie, which is what `AAsset_openFileDescriptor` asks -- or `None` if
    /// there is no such asset.
    ///
    /// **No default**: whether an asset is stored or compressed in its package is a fact about the
    /// package, and only the embedding holding it can say.
    fn placement(&self, name: &[u8]) -> Option<AssetPlacement>;
}

/// Whether a file holds an asset's bytes as they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetPlacement {
    /// No file does: the asset is compressed in its package, or its source has no package at
    /// all. A device answers `AAsset_openFileDescriptor` with -1.
    NotInAnyFile,
    /// Stored uncompressed in its package: a device hands out a descriptor on the package with the
    /// asset's offset and length in it.
    StoredInPackage {
        /// The package's path **as the guest sees it** -- a device's `base.apk` -- which the
        /// embedding must have placed in the guest's filesystem.
        package: String,
        /// Where the asset's bytes start in the package.
        offset: u64,
        /// How many bytes they are.
        length: u64,
    },
}

/// One `AAssetManager`: which Java `AssetManager` it came from.
///
/// It holds no source of its own — the source is the instance's. What it carries is the identity
/// of the `jobject` it was made from, so that `AAssetManager_fromJava` called twice with the same
/// object returns **the same manager**, which is what a device does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AssetManager {
    /// The `jobject` handle `AAssetManager_fromJava` was given.
    pub(super) from_java: u64,
}

/// One open `AAsset`: its bytes, how far the guest has read, and where they are mapped.
#[derive(Debug)]
pub struct OpenAsset {
    /// The name it was opened by.
    name: Vec<u8>,
    /// The whole asset. Held for as long as it is open, because `AAsset_getBuffer` may ask for a
    /// pointer to all of it at any time.
    bytes: Vec<u8>,
    /// `AAsset_read`'s position.
    position: usize,
    /// Where the bytes are in guest memory, once `AAsset_getBuffer` has been called, and how many
    /// bytes were mapped there.
    mapped: Option<(GuestAddr, usize)>,
}

impl OpenAsset {
    /// The name this asset was opened by.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// How large it is.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether it is empty, which a real asset may legitimately be.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// How far `AAsset_read` has got.
    #[must_use]
    pub fn position(&self) -> usize {
        self.position
    }

    /// Where `AAsset_getBuffer` put the bytes, if it has been called.
    #[must_use]
    pub fn mapped_at(&self) -> Option<GuestAddr> {
        self.mapped.map(|(at, _)| at)
    }
}

/// A refusal naming this symbol and its guest address.
fn refuse_inline(c: &ImportCall<'_, '_>, why: String) -> AbiError {
    AbiError::Refused { symbol: c.symbol().to_string(), address: c.address(), why }
}

/// The same, on the exit path.
fn refuse_reentrant(c: &ReentrantCall<'_>, why: String) -> AbiError {
    AbiError::Refused { symbol: c.symbol().to_string(), address: c.address(), why }
}

fn count(ndk: &Ndk, symbol: &'static str) {
    *ndk.census.lock().entry(symbol).or_insert(0) += 1;
}

/// `AAssetManager *AAssetManager_fromJava(JNIEnv *env, jobject assetManager)`
///
/// **The `jobject` is checked against the JNI registry**, not merely stored. §5.2 step 10 passes
/// the `AssetManager` the Java side handed `initializeNativeCode`; a layer that accepted any
/// non-null value would make a wrong argument — a `Surface`, a `Configuration`, a stale handle —
/// into an asset manager that answers `null` for every asset, thousands of instructions from the
/// mistake.
///
/// Called twice with the same object it returns the **same** manager, which is what a device does.
fn asset_manager_from_java(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (_env, object) = {
        let mut a: Args<'_> = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AAssetManager_fromJava");

    if ndk.asset_source.get().is_none() {
        return Err(refuse_inline(
            c,
            "this guest instance has no asset source, so there is nothing for an AAssetManager to \
             read. `Ndk::set_asset_source` is what supplies one; there is deliberately no default, \
             because this crate cannot read an APK and must not learn to -- the source is the \
             embedding's, over `omni_apk::Apk` or whatever else holds the assets"
                .to_string(),
        ));
    }
    if object == 0 {
        return Err(refuse_inline(
            c,
            "`AAssetManager_fromJava` was given a null `jobject`. jni-surface.md §5.2 step 10 \
             passes the AssetManager the Java side handed initializeNativeCode, so a null here is \
             a host that did not build one"
                .to_string(),
        ));
    }
    // The handle has to be one this runtime's JNI layer issued, and it has to be an
    // `android.content.res.AssetManager`. Both checks are the JNI instance's to make.
    let (jni, _thread) = crate::jni::active(c.symbol(), c.address())?;
    let class = jni.instance_class_name(object).ok_or_else(|| {
        refuse_inline(
            c,
            format!(
                "`AAssetManager_fromJava` was given {object:#x}, which is not a live jobject of \
                 this instance"
            ),
        )
    })?;
    if class != ASSET_MANAGER_CLASS {
        return Err(refuse_inline(
            c,
            format!(
                "`AAssetManager_fromJava` was given an instance of `{class}`. It takes an \
                 `{ASSET_MANAGER_CLASS}`, and accepting another class would make a wrong argument \
                 into an asset manager that answers null for every asset -- thousands of \
                 instructions from the mistake"
            ),
        ));
    }

    let mut state = ndk.state.lock();
    let existing =
        state.managers.iter().find(|(_, manager)| manager.from_java == object).map(|(at, _)| at);
    if let Some(at) = existing {
        drop(state);
        c.ret().u64(at as u64);
        return Ok(());
    }
    let Some(at) = state.managers.insert(AssetManager { from_java: object }) else {
        return Err(AbiError::Refused {
            symbol: c.symbol().to_string(),
            address: c.address(),
            why: format!(
                "this guest instance already holds {} AAssetManagers, which is the cap",
                super::MAX_ASSET_MANAGERS
            ),
        });
    };
    drop(state);
    c.ret().u64(at as u64);
    Ok(())
}

/// The Java class `AAssetManager_fromJava` accepts.
pub const ASSET_MANAGER_CLASS: &str = "android/content/res/AssetManager";

/// `AASSET_MODE_*`, from `android/asset_manager.h`. All four are accepted and none changes what
/// this layer does: every entry in this APK is DEFLATED (M0), so an asset is inflated whole
/// whatever mode was asked for. The mode is a *hint* on a device too — `AASSET_MODE_STREAMING`
/// and `AASSET_MODE_BUFFER` differ in what the platform caches, not in what the caller may then
/// do — so honouring the hint by ignoring it is the contract rather than a shortcut.
const AASSET_MODE_UNKNOWN: i32 = 0;
const AASSET_MODE_RANDOM: i32 = 1;
const AASSET_MODE_STREAMING: i32 = 2;
const AASSET_MODE_BUFFER: i32 = 3;

/// `AAsset *AAssetManager_open(AAssetManager *mgr, const char *filename, int mode)`
///
/// Returns **null** when there is no such asset, which is what a device does and what every caller
/// branches on. A refusal would be wrong here: an asset that is not there is an ordinary answer,
/// and the engine probes for optional content.
fn asset_manager_open(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (manager, filename, mode) = {
        let mut a: Args<'_> = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_i32()?)
    };
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AAssetManager_open");

    if !matches!(
        mode,
        AASSET_MODE_UNKNOWN | AASSET_MODE_RANDOM | AASSET_MODE_STREAMING | AASSET_MODE_BUFFER
    ) {
        return Err(refuse_inline(
            c,
            format!(
                "`AAssetManager_open` was given mode {mode}, and `android/asset_manager.h` \
                 defines only 0..=3. A mode nobody has defined is refused rather than treated as \
                 one of the four"
            ),
        ));
    }
    {
        let state = ndk.state.lock();
        let at = GuestAddr::try_from(manager).ok().unwrap_or(0);
        if state.managers.get(at).is_none() {
            return Err(refuse_inline(
                c,
                format!(
                    "`AAssetManager_open` was given {manager:#x} as an `AAssetManager *`, and this \
                     instance did not hand that out"
                ),
            ));
        }
    }
    if filename == 0 {
        return Err(refuse_inline(c, "`AAssetManager_open` was given a null filename".to_string()));
    }
    let at = GuestAddr::try_from(filename)
        .map_err(|_| refuse_inline(c, "a guest pointer wider than the host's usize".to_string()))?;
    let name = c.mem().cstr(at, Blame::new(c.symbol(), c.address(), 1))?;

    let source = ndk.asset_source.get().expect("a manager exists, so a source was supplied");
    let Some(bytes) = source.read(&name) else {
        // No such asset. Null, and recorded, because "which assets did the engine ask for" is
        // one of the measurements M5 exists to take.
        let mut state = ndk.state.lock();
        let thread = Ndk::thread_index(&mut state);
        state.record(0, thread, "openAsset", format!("{}: no such asset", show(&name)));
        drop(state);
        c.ret().u64(0);
        return Ok(());
    };
    let len = bytes.len();
    let mut state = ndk.state.lock();
    let thread = Ndk::thread_index(&mut state);
    let Some(asset) =
        state.assets.insert(OpenAsset { name: name.clone(), bytes, position: 0, mapped: None })
    else {
        // The cap, reported the way a device reports failure to open: null. The count is what a
        // reader needs, so it is recorded rather than only refused.
        state.record(0, thread, "openAsset", format!("{}: no free AAsset slot", show(&name)));
        drop(state);
        c.ret().u64(0);
        return Ok(());
    };
    state.record(asset, thread, "openAsset", format!("{}: {len} bytes", show(&name)));
    drop(state);
    c.ret().u64(asset as u64);
    Ok(())
}

/// Render an asset name for a log line, without pretending it is UTF-8.
fn show(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

/// `off64_t AAsset_getLength64(AAsset *asset)` — and `AAsset_getLength`, which is the 32-bit
/// spelling the engine imports.
///
/// `AAsset_getLength` returns `off_t`, which is 32-bit even on LP64 in the NDK's own header, so an
/// asset larger than 2 GiB cannot be reported through it. **Refused rather than truncated**: a
/// truncated length makes a caller read the wrong number of bytes and every call report success.
fn asset_get_length(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let asset = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AAsset_getLength");
    let len = {
        let state = ndk.state.lock();
        let at = GuestAddr::try_from(asset).ok().unwrap_or(0);
        state
            .assets
            .get(at)
            .ok_or_else(|| {
                refuse_inline(
                    c,
                    format!("`AAsset_getLength` was given {asset:#x}, which is not an open AAsset"),
                )
            })?
            .bytes
            .len()
    };
    if len > i32::MAX as usize {
        return Err(refuse_inline(
            c,
            format!(
                "asset is {len} bytes and `AAsset_getLength` returns a 32-bit `off_t`. \
                 Truncating it would make the caller read the wrong number of bytes with every \
                 call reporting success; `AAsset_getLength64` is the call that can carry this, \
                 and `libroblox.so` does not import it"
            ),
        ));
    }
    c.ret().i32(len as i32);
    Ok(())
}

/// `int AAsset_read(AAsset *asset, void *buf, size_t count)`
///
/// Returns the bytes read, `0` at end of file, and a negative value on error — the NDK's own
/// contract, and the same one `read(2)` has.
fn asset_read(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (asset, buf, wanted) = {
        let mut a: Args<'_> = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AAsset_read");

    let at = GuestAddr::try_from(asset).ok().unwrap_or(0);
    // **The bytes are copied out under the lock and written to the guest after it is dropped.**
    // A guest write can fault, and faulting with the instance lock held would leave every other
    // thread's asset call blocked behind a failure that has already been reported.
    let taken = {
        let mut state = ndk.state.lock();
        let open = state.assets.get_mut(at).ok_or_else(|| {
            AbiError::Refused {
                symbol: c.symbol().to_string(),
                address: c.address(),
                why: format!("`AAsset_read` was given {asset:#x}, which is not an open AAsset"),
            }
        })?;
        let want = usize::try_from(wanted).unwrap_or(usize::MAX);
        let left = open.bytes.len() - open.position;
        let take = want.min(left);
        let from = open.position;
        open.position += take;
        open.bytes[from..from + take].to_vec()
    };
    if taken.is_empty() {
        // End of file, or a zero-length request. Neither touches `buf`, so a zero-length read at
        // a null pointer -- which is legal C -- does not fault.
        c.ret().i32(0);
        return Ok(());
    }
    if buf == 0 {
        return Err(refuse_inline(c, "`AAsset_read` was given a null buffer".to_string()));
    }
    let destination = GuestAddr::try_from(buf)
        .map_err(|_| refuse_inline(c, "a guest pointer wider than the host's usize".to_string()))?;
    c.mem().write_bytes(destination, &taken, Blame::new(c.symbol(), c.address(), 1))?;
    c.ret().i32(taken.len() as i32);
    Ok(())
}

/// `void AAsset_close(AAsset *asset)`
///
/// **On the exit path**, because it unmaps whatever `AAsset_getBuffer` mapped.
fn asset_close(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let asset = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AAsset_close");
    let at = GuestAddr::try_from(asset).ok().unwrap_or(0);
    let closed = {
        let mut state = ndk.state.lock();
        state.assets.remove(at)
    };
    let Some(open) = closed else {
        return Err(refuse_reentrant(
            c,
            format!("`AAsset_close` was given {asset:#x}, which is not an open AAsset"),
        ));
    };
    if let Some((mapped, bytes)) = open.mapped {
        // The guest may hold translations of code it never ran out of this range; there is no
        // reason for it to, but invalidating is what every other unmapping handler does and the
        // cost of not doing it is a stale translation nobody can see.
        c.invalidate_code(mapped, bytes)?;
        ndk.space.unmap(mapped, bytes).map_err(|error| {
            refuse_reentrant(
                c,
                format!("the asset's buffer at {mapped:#x} could not be released: {error}"),
            )
        })?;
    }
    let mut state = ndk.state.lock();
    let thread = Ndk::thread_index(&mut state);
    state.record(at, thread, "closeAsset", show(open.name()));
    Ok(())
}

/// `const void *AAsset_getBuffer(AAsset *asset)`
///
/// **On the exit path**, because it maps the asset's bytes where the guest can read them (F9).
///
/// Mapped **once per open asset**: the same pointer comes back from every later call, which is
/// the documented contract and is what makes the pointer safe for the guest to store. A mapping
/// per call would leak one per call and give a different answer each time.
///
/// Read-only, because the contract is `const void *` and because an asset the guest could write
/// through would be a copy of the APK it could corrupt.
fn asset_get_buffer(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let asset = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AAsset_getBuffer");
    let at = GuestAddr::try_from(asset).ok().unwrap_or(0);

    // Already mapped, or empty: answer without touching the address space.
    let (bytes, already) = {
        let state = ndk.state.lock();
        let open = state.assets.get(at).ok_or_else(|| {
            AbiError::Refused {
                symbol: c.symbol().to_string(),
                address: c.address(),
                why: format!("`AAsset_getBuffer` was given {asset:#x}, which is not an open AAsset"),
            }
        })?;
        (open.bytes.clone(), open.mapped)
    };
    if let Some((mapped, _)) = already {
        c.ret(|mut r| r.u64(mapped as u64));
        return Ok(());
    }
    if bytes.is_empty() {
        // A zero-length asset. NULL is what the NDK returns, and it is not an error: the caller
        // has already learned the length is zero and has nothing to read.
        c.ret(|mut r| r.u64(0));
        return Ok(());
    }

    let page = ndk.space.page_size();
    let span = (bytes.len() + page - 1) & !(page - 1);
    let mapped = ndk
        .space
        .map_anonymous(
            Placement::Anywhere { align: page },
            span,
            // Written here, then sealed. A `const void *` the guest could write through would be
            // a copy of the APK it could corrupt, and the engine caches these pointers.
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .map_err(|error| {
            refuse_reentrant(c, format!("the asset's buffer could not be mapped: {error}"))
        })?;
    c.mem().write_bytes(mapped, &bytes, c.blame(0))?;
    ndk.space.protect(mapped, span, Protection::Read).map_err(|error| {
        refuse_reentrant(c, format!("the asset's buffer could not be sealed read-only: {error}"))
    })?;

    let mut state = ndk.state.lock();
    let thread = Ndk::thread_index(&mut state);
    if let Some(open) = state.assets.get_mut(at) {
        open.mapped = Some((mapped, span));
    }
    state.record(at, thread, "getBuffer", format!("{} bytes at {mapped:#x}", bytes.len()));
    drop(state);
    c.ret(|mut r| r.u64(mapped as u64));
    Ok(())
}

/// `int AAsset_openFileDescriptor(AAsset *asset, off_t *outStart, off_t *outLength)`
///
/// **-1 for an asset no file holds as it is**, which is a device's answer for a compressed one,
/// and **a read-only descriptor on the package** for one stored in it -- see this module's
/// documentation. MEASURED reader: the engine's renderer, once its device and swapchain existed,
/// for its shader pack. After -1, `outStart` and `outLength` are not written: a device writes the
/// two uninitialised locals `Asset::openFileDescriptor` left alone, which is to say nothing a
/// caller may read.
fn asset_open_file_descriptor(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (asset, out_start, out_length) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AAsset_openFileDescriptor");
    let name = {
        let state = ndk.state.lock();
        let at = GuestAddr::try_from(asset).ok().unwrap_or(0);
        state
            .assets
            .get(at)
            .ok_or_else(|| {
                refuse_inline(
                    c,
                    format!(
                        "`AAsset_openFileDescriptor` was given {asset:#x}, which is not an open \
                         AAsset"
                    ),
                )
            })?
            .name
            .clone()
    };
    let source = ndk.asset_source.get().expect("an asset is open, so a source was supplied");
    match source.placement(&name) {
        Some(AssetPlacement::NotInAnyFile) => {
            let mut state = ndk.state.lock();
            let thread = Ndk::thread_index(&mut state);
            let at = GuestAddr::try_from(asset).ok().unwrap_or(0);
            state.record(at, thread, "openFileDescriptor", format!("{}: -1", show(&name)));
            drop(state);
            c.ret().i32(-1);
            Ok(())
        }
        Some(AssetPlacement::StoredInPackage { package, offset, length }) => {
            // The two outputs are admitted before a descriptor exists, so a bad pointer cannot
            // leave one open that nothing will close.
            let blame = Blame::new(c.symbol(), c.address(), 1);
            let mut outputs = Vec::with_capacity(2);
            for (field, pointer) in [("outStart", out_start), ("outLength", out_length)] {
                let at = GuestAddr::try_from(pointer).ok().filter(|at| *at != 0).ok_or_else(|| {
                    refuse_inline(
                        c,
                        format!(
                            "`AAsset_openFileDescriptor` on {} with `{field} = {pointer:#x}`: a \
                             device writes the asset's place there and would fault",
                            show(&name)
                        ),
                    )
                })?;
                c.mem().checked_ptr(at, 8, true, blame)?;
                outputs.push(at);
            }
            let bionic = crate::bionic::active(c.symbol(), c.address())?;
            let fs = bionic.bionic.filesystem().ok_or_else(|| {
                refuse_inline(
                    c,
                    "this guest instance has no filesystem root, so the package a stored asset \
                     lives in cannot be opened for it"
                        .to_string(),
                )
            })?;
            let read_only = omni_platform::fs::OpenFlags { read: true, ..Default::default() };
            let fd = fs.open(package.as_bytes(), read_only).map_err(|error| {
                refuse_inline(
                    c,
                    format!(
                        "`AAsset_openFileDescriptor` on {}, stored in the package the source names \
                         as {package}, which the guest's filesystem cannot open: {error}. The \
                         embedding places the package there -- a device's `base.apk`",
                        show(&name)
                    ),
                )
            })?;
            c.mem().write_u64(outputs[0], offset, blame)?;
            c.mem().write_u64(outputs[1], length, blame)?;
            let mut state = ndk.state.lock();
            let thread = Ndk::thread_index(&mut state);
            let at = GuestAddr::try_from(asset).ok().unwrap_or(0);
            state.record(
                at,
                thread,
                "openFileDescriptor",
                format!("{}: fd {fd} on {package} at {offset}, {length} bytes", show(&name)),
            );
            drop(state);
            c.ret().i32(fd);
            Ok(())
        }
        None => Err(refuse_inline(
            c,
            format!(
                "`AAsset_openFileDescriptor` on {}, which is open but which its source no longer \
                 knows -- a source that changed under an open asset",
                show(&name)
            ),
        )),
    }
}

/// Every asset symbol serviced **inside** the run loop.
pub(super) static INLINE: &[(&str, ImportFn)] = &[
    ("AAssetManager_fromJava", asset_manager_from_java),
    ("AAssetManager_open", asset_manager_open),
    ("AAsset_getLength", asset_get_length),
    ("AAsset_read", asset_read),
    ("AAsset_openFileDescriptor", asset_open_file_descriptor),
];

/// Every asset symbol serviced on the **exit** path, because it changes the address space (F9).
pub(super) static REENTRANT: &[(&str, ReentrantFn)] =
    &[("AAsset_getBuffer", asset_get_buffer), ("AAsset_close", asset_close)];

/// An [`AssetSource`] that has nothing in it.
///
/// For a host that must supply a source — the type requires one — and genuinely has no assets to
/// give. **Not a default**: a default empty source would make "the embedding forgot" and "there
/// really are no assets" the same state, and the first of those is the one worth failing on.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoAssets;

impl AssetSource for NoAssets {
    fn read(&self, _name: &[u8]) -> Option<Vec<u8>> {
        None
    }

    fn placement(&self, _name: &[u8]) -> Option<AssetPlacement> {
        None
    }
}

/// An [`AssetSource`] over a table the host builds, for a test or a small embedding.
#[derive(Debug, Default)]
pub struct AssetTable {
    entries: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
}

impl AssetTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> AssetTable {
        AssetTable::default()
    }

    /// Add one asset under its `AAssetManager`-relative name.
    #[must_use]
    pub fn with(mut self, name: &str, bytes: impl Into<Vec<u8>>) -> AssetTable {
        self.entries.insert(name.as_bytes().to_vec(), bytes.into());
        self
    }

    /// How many assets it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl AssetSource for AssetTable {
    fn read(&self, name: &[u8]) -> Option<Vec<u8>> {
        self.entries.get(name).cloned()
    }

    /// A table has no package, so no file holds its bytes.
    fn placement(&self, name: &[u8]) -> Option<AssetPlacement> {
        self.entries.contains_key(name).then_some(AssetPlacement::NotInAnyFile)
    }
}

/// Wrap an [`AssetSource`] so it can be handed to [`Ndk::set_asset_source`].
#[must_use]
pub fn source(from: impl AssetSource + 'static) -> Arc<dyn AssetSource> {
    Arc::new(from)
}
