//! The GL calls whose values do not simply pass through: host strings, and host buffer mappings.
//! See the module table in [`super`].
//!
//! # Why a mapping gets a shadow
//!
//! `glMapBufferRange` returns the driver's pointer, which is host memory outside the guest's space.
//! Guest loads and stores through it would work -- the JIT's memory path is the host's -- but this
//! layer's own validated imports refuse it: the engine fills a mapping with `memcpy`, and bionic's
//! `memcpy` checks both ranges are guest memory through `admit`, which is the one copy of "may guest
//! code touch this range" (Global Constraint 11). Admitting host driver memory there would make
//! that check mean nothing. So the guest gets a **guest** range of the mapping's length, and:
//!
//! * it is filled from the driver's mapping when the mapping's contents are defined -- when
//!   `GL_MAP_READ_BIT` is set, or when neither invalidate bit is (the untouched bytes must survive
//!   the copy back);
//! * it is copied back when the specification says the guest's writes take effect: on
//!   `glFlushMappedBufferRange` for the flushed range under `GL_MAP_FLUSH_EXPLICIT_BIT`, and on
//!   `glUnmapBuffer` for the whole range otherwise -- only when `GL_MAP_WRITE_BIT` is set;
//! * `glGetBufferPointerv(GL_BUFFER_MAP_POINTER)` answers the shadow, so the two ways of asking
//!   for the pointer agree.
//!
//! `GL_MAP_PERSISTENT_BIT`/`GL_MAP_COHERENT_BIT` (`EXT_buffer_storage`) are refused by name: a
//! coherent mapping's writes must reach the GPU with no call at all, which a shadow cannot do.
//!
//! # What each access bit makes this layer copy (ES 3.2 section 6.3)
//!
//! | access | into the shadow at map | out of it |
//! |---|---|---|
//! | `READ` (any other bits) | the whole range: the guest may read any of it | -- unless `WRITE` too, as below |
//! | `WRITE` with `INVALIDATE_RANGE` or `INVALIDATE_BUFFER`, no `READ` | **nothing**: "the previous contents of the specified range may be discarded", so the shadow's are as good | at unmap, the whole range |
//! | `WRITE`, no invalidate, no `READ` | the whole range: every byte is uploaded back, and the ones the guest did not write must be the buffer's own | at unmap, the whole range |
//! | `WRITE` + `FLUSH_EXPLICIT` | as the two rows above | **only** each `glFlushMappedBufferRange`'s range, when it is made; nothing at unmap |
//!
//! [`fills_shadow`], [`uploads_at_unmap`] and [`uploads_at_flush`] are those three columns, and the
//! only place they are decided.
//!
//! # Shadows are reused: why, measured
//!
//! MEASURED (Linux, NVC0, the Pet Simulator 99 world, 2026-09-25): the engine's render thread spent
//! up to 82% of its time inside `glMapBufferRange` and 9-10% in `glUnmapBuffer`, 83% of those
//! samples in this executable, at ~150 map/unmap pairs per frame. Each map used to make a fresh guest
//! mapping and each unmap to unmap it: `GuestSpace::map_anonymous` takes the region map's lock,
//! bumps the generation that every thread's `admit` cache answers from, and **walks every entry of
//! the map** to find a free run (`find_free`) -- tens of thousands of entries in an engine-sized
//! space -- and the lazily-committed shadow was then committed a granule at a time by faults, and
//! decommitted again at unmap. None of it is the data's cost.
//!
//! So a shadow, once made, is kept: [`ShadowPool`] holds the idle ones (eagerly committed, so a
//! guest store or an `admit` never faults or takes the lock for one), a map takes the smallest idle
//! one that fits, and an unmap gives it back. Idle shadows are bounded by [`SHADOW_POOL_BYTES`];
//! past it the least recently returned are unmapped. What a reused shadow holds before the guest
//! writes it is a previous mapping's bytes -- which is exactly what an invalidated range is allowed
//! to hold, and which every other row of the table overwrites from the buffer.
//!
//! **Why a shadow at all rather than the driver's pointer**, again: that pointer is host memory,
//! and handing it to the guest would mean admitting driver memory in `admit`. The copy this layer
//! makes is the one a device's driver also makes when the mapping is a staging buffer: one pass
//! over the bytes the guest wrote, into the driver's mapping, at unmap or flush. Mapping the host
//! later (at unmap) would save nothing -- the copy is the same -- and would move the driver's
//! synchronisation point away from where the guest asked for it.

use std::collections::HashMap;
use std::ffi::CStr;

use omni_mem::{CommitPolicy, GuestAddr, GuestSpace, MappingId, Placement, Protection};

use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::{write_return, Call, Gles};

/// How much guest address space the string pool reserves (committed only as strings are written).
///
/// MEASURED: Mesa 26.0.8's `GL_EXTENSIONS` for an ES 3.2 context is 4,200 bytes and its display's
/// `EGL_EXTENSIONS` about 1 KB; every string is copied once (interned by content), so 1 MiB is
/// far past every string a host has, and running out is a refusal naming this constant.
pub const STRING_POOL_BYTES: usize = 1 << 20;

/// `GL_MAP_READ_BIT` .. `GL_MAP_COHERENT_BIT`.
pub const MAP_READ: u32 = 0x1;
/// `GL_MAP_WRITE_BIT`.
pub const MAP_WRITE: u32 = 0x2;
/// `GL_MAP_INVALIDATE_RANGE_BIT`.
pub const MAP_INVALIDATE_RANGE: u32 = 0x4;
/// `GL_MAP_INVALIDATE_BUFFER_BIT`.
pub const MAP_INVALIDATE_BUFFER: u32 = 0x8;
/// `GL_MAP_FLUSH_EXPLICIT_BIT`.
pub const MAP_FLUSH_EXPLICIT: u32 = 0x10;
/// `GL_MAP_PERSISTENT_BIT_EXT`.
pub const MAP_PERSISTENT: u32 = 0x40;
/// `GL_MAP_COHERENT_BIT_EXT`.
pub const MAP_COHERENT: u32 = 0x80;
/// `GL_BUFFER_MAP_POINTER`.
pub const BUFFER_MAP_POINTER: u32 = 0x88BD;
/// `GL_BUFFER_SIZE`.
pub const BUFFER_SIZE: u32 = 0x8764;

/// The binding query for each ES 3.2 buffer target: `glGetIntegerv(binding)` names the buffer
/// `glMapBufferRange(target)` maps, which is what a mapping is keyed by.
#[must_use]
pub fn binding_of(target: u32) -> Option<u32> {
    Some(match target {
        0x8892 => 0x8894, // GL_ARRAY_BUFFER -> _BINDING
        0x8893 => 0x8895, // GL_ELEMENT_ARRAY_BUFFER
        0x88EB => 0x88ED, // GL_PIXEL_PACK_BUFFER
        0x88EC => 0x88EF, // GL_PIXEL_UNPACK_BUFFER
        0x8A11 => 0x8A28, // GL_UNIFORM_BUFFER
        0x8C8E => 0x8C8F, // GL_TRANSFORM_FEEDBACK_BUFFER
        0x8F36 => 0x8F36, // GL_COPY_READ_BUFFER (its binding query has the same value)
        0x8F37 => 0x8F37, // GL_COPY_WRITE_BUFFER
        0x90D2 => 0x90D3, // GL_SHADER_STORAGE_BUFFER
        0x8F3F => 0x8F43, // GL_DRAW_INDIRECT_BUFFER
        0x90EE => 0x90EF, // GL_DISPATCH_INDIRECT_BUFFER
        0x92C0 => 0x92C1, // GL_ATOMIC_COUNTER_BUFFER
        0x8C2A => 0x8C2A, // GL_TEXTURE_BUFFER
        _ => return None,
    })
}

/// Host strings, copied once into guest memory.
#[derive(Debug, Default)]
pub(super) struct StringPool {
    region: Option<GuestAddr>,
    used: usize,
    interned: HashMap<Vec<u8>, GuestAddr>,
}

impl StringPool {
    pub(super) fn used(&self) -> usize {
        self.used
    }
}

/// One live buffer mapping.
#[derive(Debug, Clone, Copy)]
pub(super) struct Mapping {
    shadow: Shadow,
    host: usize,
    length: usize,
    access: u32,
}

/// How many bytes of **idle** shadows [`ShadowPool`] keeps for reuse. Live shadows are not counted:
/// they are as large as what the guest has mapped, which is the guest's to bound.
///
/// MEASURED need: the engine keeps 2-6 mappings live at a time (the census's "buffer mapping(s)
/// live") and maps ~150 times a frame; 64 MiB of committed memory keeps every such working set of
/// ring-buffer ranges up to several MiB each resident, and is 1.7% of the 3.8 GiB private memory
/// the same session had.
pub const SHADOW_POOL_BYTES: usize = 64 << 20;

/// The smallest shadow made: a commit granule (`omni_mem`'s default), so a 4-byte map and a 60 KiB
/// one share one shadow class. Larger shadows are a power of two, so a range that grows by a few
/// bytes a frame still fits the one it had.
pub const SHADOW_MIN_BYTES: usize = 64 << 10;

/// A guest range that shadows a mapping: `at`, `capacity` bytes, eagerly committed, read-write,
/// made as the guest mapping `id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Shadow {
    pub(super) at: GuestAddr,
    pub(super) capacity: usize,
    pub(super) id: MappingId,
}

/// Idle shadows, oldest-returned first. See the module documentation for why they are kept.
///
/// OS-free: it decides which shadow to take and which to let go; [`take_shadow`] and
/// [`release_shadows`] make, check and unmap them.
#[derive(Debug, Default)]
pub(super) struct ShadowPool {
    idle: Vec<Shadow>,
    idle_bytes: usize,
    /// Census: shadows made, maps served from the pool, shadows unmapped past the bound, and idle
    /// shadows found no longer this layer's (the guest unmapped or changed them) and dropped.
    pub(super) made: u64,
    pub(super) reused: u64,
    pub(super) released: u64,
    pub(super) lost: u64,
}

impl ShadowPool {
    /// The smallest idle shadow of at least `length` bytes that `still_ours` accepts, taken out of
    /// the pool. Every candidate it rejects is dropped from the pool for good.
    ///
    /// **Why the check.** A shadow is guest memory, and the guest can `munmap` or `mprotect` it --
    /// nothing a real driver's pointer would survive either, but a pooled address outlives the
    /// mapping it was handed out for, and the range at it may since have become some other
    /// allocation of the guest's. Filling that from a buffer would be writing over it.
    pub(super) fn take(&mut self, length: usize, still_ours: impl Fn(&Shadow) -> bool) -> Option<Shadow> {
        loop {
            let (index, _) = self
                .idle
                .iter()
                .enumerate()
                .filter(|(_, s)| s.capacity >= length)
                .min_by_key(|(_, s)| s.capacity)?;
            let shadow = self.idle.remove(index);
            self.idle_bytes -= shadow.capacity;
            if still_ours(&shadow) {
                self.reused += 1;
                return Some(shadow);
            }
            self.lost += 1;
        }
    }

    /// Put `shadow` back; answers the shadows that must now be unmapped so the idle total is at
    /// most `limit` -- the least recently returned first, which may be `shadow` itself when it
    /// alone is over the limit.
    #[must_use]
    pub(super) fn give_back(&mut self, shadow: Shadow, limit: usize) -> Vec<Shadow> {
        self.idle.push(shadow);
        self.idle_bytes += shadow.capacity;
        let mut evicted = Vec::new();
        while self.idle_bytes > limit {
            let oldest = self.idle.remove(0);
            self.idle_bytes -= oldest.capacity;
            evicted.push(oldest);
        }
        self.released += evicted.len() as u64;
        evicted
    }

    /// Idle shadows and their bytes.
    pub(super) fn idle(&self) -> (usize, usize) {
        (self.idle.len(), self.idle_bytes)
    }
}

/// The capacity a new shadow for a `length`-byte mapping gets: at least [`SHADOW_MIN_BYTES`], a
/// power of two, a whole number of pages. `None` past the largest power of two.
#[must_use]
pub fn shadow_capacity(length: usize, page: usize) -> Option<usize> {
    let capacity = length.max(SHADOW_MIN_BYTES).checked_next_power_of_two()?;
    Some(capacity.max(page))
}

/// Whether a mapping with `access` gets the buffer's bytes copied into its shadow at map: under
/// `GL_MAP_READ_BIT`, or when neither invalidate bit discards them (then every byte goes back at
/// unmap and the untouched ones must be the buffer's). See the module table.
#[must_use]
pub fn fills_shadow(access: u32) -> bool {
    access & MAP_READ != 0 || access & (MAP_INVALIDATE_RANGE | MAP_INVALIDATE_BUFFER) == 0
}

/// Whether `glUnmapBuffer` uploads the whole mapped range: a write mapping without
/// `GL_MAP_FLUSH_EXPLICIT_BIT`.
#[must_use]
pub fn uploads_at_unmap(access: u32) -> bool {
    access & MAP_WRITE != 0 && access & MAP_FLUSH_EXPLICIT == 0
}

/// Whether `glFlushMappedBufferRange` uploads its range: a write mapping with
/// `GL_MAP_FLUSH_EXPLICIT_BIT` (on any other the driver raises the error and nothing is flushed).
#[must_use]
pub fn uploads_at_flush(access: u32) -> bool {
    access & MAP_WRITE != 0 && access & MAP_FLUSH_EXPLICIT != 0
}

/// Copy the host string at `text` into guest memory and answer the guest address (0 for NULL).
///
/// Interned by content: `glGetString(GL_RENDERER)` asked twice answers the same address, as a
/// device's static string would.
pub(super) fn copy_host_string(
    gles: &Gles,
    c: &ImportCall<'_, '_>,
    call: &Call,
    text: u64,
    name: u32,
) -> AbiResult<u64> {
    if text == 0 {
        return Ok(0);
    }
    copy_bytes(gles, c, call, host_bytes(text), name)
}

/// The bytes of a non-NULL host string.
fn host_bytes(text: u64) -> Vec<u8> {
    // SAFETY: a non-NULL `glGetString`/`glGetStringi`/`eglQueryString` result is a static,
    // NUL-terminated string the host owns (ES 3.2 section 20.2, EGL 1.5 section 3.3), read here at
    // once on the thread that asked.
    unsafe { CStr::from_ptr(text as usize as *const core::ffi::c_char) }.to_bytes().to_vec()
}

/// Copy `bytes` into the guest string pool (interned) and answer the guest address.
fn copy_bytes(
    gles: &Gles,
    c: &ImportCall<'_, '_>,
    call: &Call,
    bytes: Vec<u8>,
    name: u32,
) -> AbiResult<u64> {
    let mut state = gles.state();
    let pool = &mut state.strings;
    if let Some(&at) = pool.interned.get(&bytes) {
        drop(state);
        note_string(gles, call, name, &bytes);
        return Ok(at as u64);
    }
    let region = match pool.region {
        Some(region) => region,
        None => {
            let space = gles.space();
            let region = space
                .map_anonymous(
                    Placement::Anywhere { align: space.page_size() },
                    STRING_POOL_BYTES,
                    Protection::ReadWrite,
                    CommitPolicy::Lazy,
                )
                .map_err(|error| call.refuse(format!("the guest string pool could not be mapped: {error}")))?;
            pool.region = Some(region);
            region
        }
    };
    let need = bytes.len() + 1;
    if pool.used + need > STRING_POOL_BYTES {
        return Err(call.refuse(format!(
            "the host's answer to `{}` is {} bytes and the guest string pool (STRING_POOL_BYTES = \
             {STRING_POOL_BYTES}) has {} left",
            call.name,
            bytes.len(),
            STRING_POOL_BYTES - pool.used
        )));
    }
    let at = region + pool.used;
    let mut with_nul = bytes.clone();
    with_nul.push(0);
    c.mem().write_bytes(at, &with_nul, c.blame(0))?;
    pool.used += need;
    pool.interned.insert(bytes.clone(), at);
    drop(state);
    note_string(gles, call, name, &bytes);
    Ok(at as u64)
}

fn note_string(gles: &Gles, call: &Call, name: u32, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let shown = if text.len() > 160 {
        format!("\"{}...\" ({} bytes)", &text[..text.char_indices().nth(160).map_or(text.len(), |(i, _)| i)], text.len())
    } else {
        format!("\"{text}\"")
    };
    gles.note(call.name, format!("{name:#x}: the host's {shown}, copied into guest memory"));
}

/// `GL_EXTENSIONS`.
pub const EXTENSIONS: u32 = 0x1F03;
/// `GL_NUM_EXTENSIONS`.
pub const NUM_EXTENSIONS: u32 = 0x821D;

fn note_withheld(gles: &Gles, call: &Call, extension: &str) {
    if let Some((_, _, why)) = super::WITHHELD.iter().find(|(name, _, _)| *name == extension) {
        gles.note(call.name, format!("{extension} withheld from the guest's extension list: {why}"));
    }
}

/// `const GLubyte *glGetString(GLenum name)`: the host's text, in guest memory -- with the
/// [`WITHHELD`](super::WITHHELD) extensions removed from `GL_EXTENSIONS`.
pub(super) fn get_string(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let name = call.lanes[0] as u32;
    let text = gles.forward_value(call)?;
    let at = if name == EXTENSIONS && text != 0 {
        let host = String::from_utf8_lossy(&host_bytes(text)).into_owned();
        let kept: Vec<&str> = host
            .split_ascii_whitespace()
            .filter(|extension| {
                let withheld = super::is_withheld_extension(extension);
                if withheld {
                    note_withheld(gles, call, extension);
                }
                !withheld
            })
            .collect();
        copy_bytes(gles, c, call, kept.join(" ").into_bytes(), name)?
    } else {
        copy_host_string(gles, c, call, text, name)?
    };
    c.ret().u64(at);
    Ok(())
}

/// The host's extension indices the guest sees, in order: every `glGetStringi(GL_EXTENSIONS, i)`
/// that is not [`WITHHELD`](super::WITHHELD). `None` when the host answers no count (no context).
fn visible_extensions(gles: &Gles, call: &Call) -> AbiResult<Option<Vec<u64>>> {
    let mut count: i32 = -1;
    gles.host_call(call, "glGetIntegerv", &[u64::from(NUM_EXTENSIONS), &mut count as *mut i32 as u64])?;
    if count < 0 {
        return Ok(None);
    }
    let mut visible = Vec::with_capacity(count as usize);
    for index in 0..count as u64 {
        let text = gles.host_call(call, "glGetStringi", &[u64::from(EXTENSIONS), index])?;
        if text == 0 {
            return Ok(None);
        }
        let extension = String::from_utf8_lossy(&host_bytes(text)).into_owned();
        if super::is_withheld_extension(&extension) {
            note_withheld(gles, call, &extension);
        } else {
            visible.push(index);
        }
    }
    Ok(Some(visible))
}

/// `const GLubyte *glGetStringi(GLenum name, GLuint index)`: for `GL_EXTENSIONS`, the guest's
/// index counts only the extensions it is shown.
pub(super) fn get_string_i(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let name = call.lanes[0] as u32;
    let mut lanes = call.lanes;
    if name == EXTENSIONS {
        if let Some(visible) = visible_extensions(gles, call)? {
            // Past the guest's end: an index past the host's end too, so the host raises
            // GL_INVALID_VALUE and answers NULL, as it would have.
            lanes[1] = visible.get(call.lanes[1] as u32 as usize).copied().unwrap_or(u64::from(u32::MAX));
        }
    }
    let text = gles.host_call(call, call.name, &lanes[..2])?;
    let at = copy_host_string(gles, c, call, text, name)?;
    c.ret().u64(at);
    Ok(())
}

/// `void glGetIntegerv(GLenum pname, GLint *data)`: forwarded; `GL_NUM_EXTENSIONS` counts only
/// the extensions the guest is shown.
pub(super) fn get_integerv(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    if call.lanes[0] as u32 != NUM_EXTENSIONS || call.lanes[1] == 0 {
        return gles.forward(c, call);
    }
    gles.forward(c, call)?;
    if let Some(visible) = visible_extensions(gles, call)? {
        let at = GuestAddr::try_from(call.lanes[1])
            .map_err(|_| call.refuse(format!("{:#x} is not an address", call.lanes[1])))?;
        c.mem().write_u32(at, visible.len() as u32, c.blame(1))?;
    }
    Ok(())
}

fn current_context(gles: &Gles, call: &Call) -> AbiResult<u64> {
    gles.host_call(call, "eglGetCurrentContext", &[])
}

fn bound_buffer(gles: &Gles, call: &Call, target: u32) -> AbiResult<Option<u32>> {
    let Some(binding) = binding_of(target) else { return Ok(None) };
    let mut value: i32 = 0;
    gles.host_call(call, "glGetIntegerv", &[u64::from(binding), &mut value as *mut i32 as u64])?;
    Ok(Some(value as u32))
}

/// A shadow of at least `length` bytes: an idle one from the pool, or a new one, eagerly committed.
fn take_shadow(gles: &Gles, call: &Call, length: usize) -> AbiResult<Shadow> {
    let space = gles.space();
    if let Some(shadow) = gles.state().shadows.take(length, |shadow| is_ours(space, shadow)) {
        return Ok(shadow);
    }
    let refuse = |why: String| {
        call.refuse(format!(
            "`{}` mapped {length} bytes and no guest shadow of that size could be mapped: {why}",
            call.name
        ))
    };
    let capacity = shadow_capacity(length, space.page_size())
        .ok_or_else(|| refuse("no power of two holds that length".to_string()))?;
    // Eager: a shadow is written end to end by the guest or by this layer as soon as it is handed
    // out, and it is kept, so a lazy one would only move the commit into a fault per granule.
    let _label = omni_mem::label_scope(omni_mem::MapLabel::new(crate::memreport::GLES_SHADOWS));
    let at = space
        .map_anonymous(
            Placement::Anywhere { align: space.page_size() },
            capacity,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .map_err(|error| refuse(error.to_string()))?;
    let id = space.region_at(at).and_then(|region| region.mapping).ok_or_else(|| {
        refuse(format!("the region map has no mapping at {at:#x}, which was mapped just now"))
    })?;
    gles.state().shadows.made += 1;
    Ok(Shadow { at, capacity, id })
}

/// Whether `shadow` is still exactly the mapping this layer made: one read-write entry of that
/// mapping from `at` for `capacity` bytes. A guest `munmap`, `mprotect` or `mmap(MAP_FIXED)` over
/// any of it changes one of those. Answered from the thread's region cache while the map is
/// unchanged, so it costs no lock on the common path.
fn is_ours(space: &GuestSpace, shadow: &Shadow) -> bool {
    space.region_at(shadow.at).is_some_and(|region| {
        region.mapping == Some(shadow.id)
            && region.start == shadow.at
            && region.len == shadow.capacity
            && region.protection == Protection::ReadWrite
    })
}

/// Give shadows no mapping uses any more back to the pool, and unmap what the pool lets go of.
fn release_shadows(gles: &Gles, shadows: impl IntoIterator<Item = Shadow>) {
    let evicted: Vec<Shadow> = {
        let mut state = gles.state();
        shadows
            .into_iter()
            .flat_map(|shadow| state.shadows.give_back(shadow, SHADOW_POOL_BYTES))
            .collect()
    };
    let space = gles.space();
    for shadow in evicted {
        // Only what is still this layer's: a range the guest has unmapped or remapped is not ours
        // to unmap. A failure to unmap leaves the pages mapped and unused; it is on a path already
        // carrying the host's answer, so it is not turned into an error the guest would see.
        if is_ours(space, &shadow) {
            let _ = space.unmap(shadow.at, shadow.capacity);
        }
    }
}

/// Give a host mapping the driver just made a shadow, fill it as `access` says, record it.
fn shadow_mapping(
    gles: &Gles,
    c: &ImportCall<'_, '_>,
    call: &Call,
    key: (u64, u32),
    host: u64,
    length: usize,
    access: u32,
) -> AbiResult<u64> {
    let shadow = take_shadow(gles, call, length)?;
    if fills_shadow(access) && length > 0 {
        let to = match c.mem().checked_ptr(shadow.at, length, true, c.blame(0)) {
            Ok(to) => to,
            Err(error) => {
                release_shadows(gles, [shadow]);
                return Err(error);
            }
        };
        // SAFETY: `host` is the driver's mapping of exactly `length` bytes, live until unmap; `to`
        // is a checked, committed, writable guest range of at least that length, owned by this
        // mapping alone from the moment it left the pool and not yet handed to the guest.
        unsafe { core::ptr::copy_nonoverlapping(host as usize as *const u8, to, length) };
    }
    let mapping = Mapping { shadow, host: host as usize, length, access };
    let replaced = {
        let mut state = gles.state();
        let entry = state.map_census.entry(access).or_default();
        entry.0 += 1;
        entry.1 += length as u64;
        state.maps.insert(key, mapping)
    };
    // The driver answered a new mapping for a buffer this layer still had one recorded for, so the
    // old one ended without a `glUnmapBuffer` -- the buffer was deleted, which unmaps it (ES 3.2
    // section 6.3.1), and its name reused. Its shadow is nobody's any more.
    if let Some(old) = replaced {
        release_shadows(gles, [old.shadow]);
    }
    Ok(shadow.at as u64)
}

/// Copy `[offset, offset + len)` of a mapping's shadow back to the driver's mapping.
fn copy_back(c: &ImportCall<'_, '_>, mapping: &Mapping, offset: usize, len: usize) -> AbiResult<()> {
    if len == 0 {
        return Ok(());
    }
    let from = c.mem().checked_ptr(mapping.shadow.at + offset, len, false, c.blame(0))?;
    // SAFETY: `offset + len <= mapping.length <= shadow.capacity` (checked by every caller), so both
    // ranges are inside the shadow and the driver's live mapping respectively; they cannot overlap,
    // one being guest memory and the other the driver's.
    unsafe { core::ptr::copy_nonoverlapping(from, (mapping.host + offset) as *mut u8, len) };
    Ok(())
}

fn refuse_persistent(call: &Call, access: u32) -> crate::error::AbiError {
    call.refuse(format!(
        "the guest called `{}` with access {access:#x}, which asks for a persistent or coherent \
         mapping (EXT_buffer_storage). This layer gives the guest a guest-memory shadow of a host \
         mapping, copied back on flush and unmap; a coherent mapping's writes must reach the driver \
         with no call at all, which a shadow cannot do, and silently dropping the bit would make \
         the guest's writes arrive late",
        call.name
    ))
}

/// `void *glMapBufferRange(GLenum target, GLintptr offset, GLsizeiptr length, GLbitfield access)`
pub(super) fn map_buffer_range(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let (target, length, access) = (call.lanes[0] as u32, call.lanes[2] as usize, call.lanes[3] as u32);
    if access & (MAP_PERSISTENT | MAP_COHERENT) != 0 {
        return Err(refuse_persistent(call, access));
    }
    map_with(gles, c, call, target, access, |_| Ok(length))
}

/// `void *glMapBufferOES(GLenum target, GLenum access)`: the whole buffer, write-only
/// (`GL_WRITE_ONLY_OES` is the only access `OES_mapbuffer` defines).
pub(super) fn map_buffer_oes(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let target = call.lanes[0] as u32;
    map_with(gles, c, call, target, MAP_WRITE, |gles| {
        let mut size: i32 = 0;
        gles.host_call(
            call,
            "glGetBufferParameteriv",
            &[u64::from(target), u64::from(BUFFER_SIZE), &mut size as *mut i32 as u64],
        )?;
        Ok(usize::try_from(size).unwrap_or(0))
    })
}

fn map_with(
    gles: &Gles,
    c: &mut ImportCall<'_, '_>,
    call: &Call,
    target: u32,
    access: u32,
    length: impl FnOnce(&Gles) -> AbiResult<usize>,
) -> AbiResult<()> {
    let context = current_context(gles, call)?;
    let buffer = bound_buffer(gles, call, target)?;
    let host = gles.forward_value(call)?;
    if host == 0 {
        // The driver refused (a GL error is set for the guest's glGetError): its NULL is the answer.
        c.ret().u64(0);
        return Ok(());
    }
    let Some(buffer) = buffer else {
        return Err(call.refuse(format!(
            "`{}` mapped target {target:#x}, which is not an ES 3.2 buffer target this layer can \
             name the bound buffer of, so the mapping could not be tracked to its unmap",
            call.name
        )));
    };
    let length = length(gles)?;
    let shadow = shadow_mapping(gles, c, call, (context, buffer), host, length, access)?;
    c.ret().u64(shadow);
    Ok(())
}

/// `GLboolean glUnmapBuffer(GLenum target)` (and `glUnmapBufferOES`).
pub(super) fn unmap_buffer(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let target = call.lanes[0] as u32;
    let context = current_context(gles, call)?;
    let mapping = match bound_buffer(gles, call, target)? {
        Some(buffer) => gles.state().maps.remove(&(context, buffer)),
        None => None,
    };
    let Some(mapping) = mapping else {
        return gles.forward(c, call);
    };
    let copied = if uploads_at_unmap(mapping.access) {
        copy_back(c, &mapping, 0, mapping.length)
    } else {
        Ok(())
    };
    // The host unmaps whatever the copy did, or its mapping would outlive the record of it.
    let r = gles.forward_value(call);
    release_shadows(gles, [mapping.shadow]);
    copied?;
    write_return(c, call.signature, r?);
    Ok(())
}

/// `void glFlushMappedBufferRange(GLenum target, GLintptr offset, GLsizeiptr length)`
pub(super) fn flush_mapped_range(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let (target, offset, length) = (call.lanes[0] as u32, call.lanes[1] as usize, call.lanes[2] as usize);
    let context = current_context(gles, call)?;
    let mapping = match bound_buffer(gles, call, target)? {
        Some(buffer) => gles.state().maps.get(&(context, buffer)).copied(),
        None => None,
    };
    if let Some(mapping) = mapping {
        let inside = offset.checked_add(length).is_some_and(|end| end <= mapping.length);
        // Outside the range, or on a mapping without FLUSH_EXPLICIT, the driver raises the GL
        // error and nothing is copied -- which is what the specification says happens to the data.
        if inside && uploads_at_flush(mapping.access) {
            copy_back(c, &mapping, offset, length)?;
        }
    }
    gles.forward(c, call)
}

/// `void glGetBufferPointerv(GLenum target, GLenum pname, void **params)`: the shadow, for
/// `GL_BUFFER_MAP_POINTER` of a buffer this layer shadowed.
pub(super) fn get_buffer_pointer(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let (target, pname, out) = (call.lanes[0] as u32, call.lanes[1] as u32, call.lanes[2]);
    if pname != BUFFER_MAP_POINTER {
        return gles.forward(c, call);
    }
    let context = current_context(gles, call)?;
    let mapping = match bound_buffer(gles, call, target)? {
        Some(buffer) => gles.state().maps.get(&(context, buffer)).copied(),
        None => None,
    };
    let Some(mapping) = mapping else {
        return gles.forward(c, call);
    };
    let at = GuestAddr::try_from(out).map_err(|_| call.refuse(format!("{out:#x} is not an address")))?;
    c.mem().write_u64(at, mapping.shadow.at as u64, c.blame(2))?;
    c.ret().void();
    Ok(())
}
