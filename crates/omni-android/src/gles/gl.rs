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

use std::collections::HashMap;
use std::ffi::CStr;

use omni_mem::{CommitPolicy, GuestAddr, Placement, Protection};

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
    shadow: GuestAddr,
    host: usize,
    length: usize,
    access: u32,
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
    // SAFETY: a non-NULL `glGetString`/`glGetStringi`/`eglQueryString` result is a static,
    // NUL-terminated string the host owns (ES 3.2 section 20.2, EGL 1.5 section 3.3), read here at
    // once on the thread that asked.
    let bytes = unsafe { CStr::from_ptr(text as usize as *const core::ffi::c_char) }.to_bytes().to_vec();
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

/// `const GLubyte *glGetString(GLenum name)`, `glGetStringi(GLenum name, GLuint index)`
pub(super) fn get_string(gles: &Gles, c: &mut ImportCall<'_, '_>, call: &Call) -> AbiResult<()> {
    let text = gles.forward_value(call)?;
    let at = copy_host_string(gles, c, call, text, call.lanes[0] as u32)?;
    c.ret().u64(at);
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

/// Map a shadow for a host mapping the driver just made, fill it as `access` says, record it.
fn shadow_mapping(
    gles: &Gles,
    c: &ImportCall<'_, '_>,
    call: &Call,
    key: (u64, u32),
    host: u64,
    length: usize,
    access: u32,
) -> AbiResult<u64> {
    let space = gles.space();
    let shadow = space
        .map_anonymous(
            Placement::Anywhere { align: space.page_size() },
            length.max(1),
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .map_err(|error| {
            call.refuse(format!(
                "`{}` mapped {length} bytes and no guest shadow of that size could be mapped: {error}",
                call.name
            ))
        })?;
    let defined = access & MAP_READ != 0 || access & (MAP_INVALIDATE_RANGE | MAP_INVALIDATE_BUFFER) == 0;
    if defined && length > 0 {
        let to = c.mem().checked_ptr(shadow, length, true, c.blame(0))?;
        // SAFETY: `host` is the driver's mapping of exactly `length` bytes, live until unmap; `to`
        // is a checked, committed, writable guest range of the same length, mapped just now and
        // not yet handed to the guest, so nothing else writes it.
        unsafe { core::ptr::copy_nonoverlapping(host as usize as *const u8, to, length) };
    }
    gles.state().maps.insert(key, Mapping { shadow, host: host as usize, length, access });
    Ok(shadow as u64)
}

/// Copy `[offset, offset + len)` of a mapping's shadow back to the driver's mapping.
fn copy_back(c: &ImportCall<'_, '_>, mapping: &Mapping, offset: usize, len: usize) -> AbiResult<()> {
    if len == 0 {
        return Ok(());
    }
    let from = c.mem().checked_ptr(mapping.shadow + offset, len, false, c.blame(0))?;
    // SAFETY: `offset + len <= mapping.length` (checked by every caller), so both ranges are inside
    // the shadow and the driver's live mapping respectively; they cannot overlap, one being guest
    // memory and the other the driver's.
    unsafe { core::ptr::copy_nonoverlapping(from, (mapping.host + offset) as *mut u8, len) };
    Ok(())
}

fn release_shadow(gles: &Gles, mapping: &Mapping) {
    // A failure to unmap leaves the pages mapped and unused; it is on a path already carrying the
    // host's answer, so it is not turned into an error the guest would see instead.
    let _ = gles.space().unmap(mapping.shadow, mapping.length.max(1));
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
    if mapping.access & MAP_WRITE != 0 && mapping.access & MAP_FLUSH_EXPLICIT == 0 {
        copy_back(c, &mapping, 0, mapping.length)?;
    }
    let r = gles.forward_value(call)?;
    release_shadow(gles, &mapping);
    write_return(c, call.signature, r);
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
        if inside && mapping.access & MAP_WRITE != 0 && mapping.access & MAP_FLUSH_EXPLICIT != 0 {
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
    c.mem().write_u64(at, mapping.shadow as u64, c.blame(2))?;
    c.ret().void();
    Ok(())
}
