//! The graphics allocator, served from the host: `android.hardware.graphics.allocator.IAllocator`
//! (AIDL, V2), the service libui's gralloc 5 asks for every graphics buffer (D2 design,
//! `docs/superpowers/specs/2026-09-27-d2-gralloc-design.md`).
//!
//! Each buffer is one [`Shm`] region: a metadata page, then the pixels at [`PIXELS_AT`]. The handle
//! it answers carries the region as its one file descriptor and describes the buffer in 14 ints;
//! the in-process mapper that reads them is `device/src/mapper.c` (the two must agree):
//!
//! | int | meaning |
//! |---|---|
//! | 0 | magic `'OMGB'` |
//! | 1 | layout version (1) |
//! | 2, 3 | width, height |
//! | 4 | layer count |
//! | 5 | pixel format as requested |
//! | 6, 7 | usage, low and high 32 bits |
//! | 8 | stride, in pixels |
//! | 9, 10 | buffer id, low and high 32 bits |
//! | 11, 12 | pixel bytes, low and high 32 bits |
//! | 13 | where the pixels start in the region |
//!
//! The metadata page is zero but for the requestor's name, NUL-terminated at [`NAME_AT`].
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use super::parcel::{Malformed, Reader, Writer, EX_ILLEGAL_ARGUMENT, EX_SERVICE_SPECIFIC, EX_UNSUPPORTED_OPERATION};
use crate::binder::{Broker, HostCall, HostReply, STABILITY_VINTF};
use crate::fd::{FileKind, OpenFile};
use crate::shm::Shm;

pub const DESCRIPTOR: &str = "android.hardware.graphics.allocator.IAllocator";
/// The instance libui waits for.
pub const INSTANCE: &str = "android.hardware.graphics.allocator.IAllocator/default";
/// The frozen V2 interface's hash (`aidl_api/android.hardware.graphics.allocator/2/.hash`).
const HASH: &str = "9499fec09c544e9de5be3c87125721600f8ade66";
const VERSION: i32 = 2;
/// `/vendor/lib64/hw/mapper.<suffix>.so`.
const MAPPER_SUFFIX: &str = "omni";

/// Where a buffer's pixels start in its region, after the metadata page.
pub const PIXELS_AT: u64 = 4096;
/// Where the requestor's name is in the metadata page.
pub const NAME_AT: u64 = 64;
const MAGIC: u32 = 0x4247_4d4f; // 'OMGB'
const LAYOUT_VERSION: i32 = 1;
/// The most buffers one `allocate2` makes.
const MAX_COUNT: i32 = 16;
/// The largest buffer, in bytes.
const MAX_BYTES: u64 = 1 << 29;

const ALLOCATE: u32 = 1;
const ALLOCATE2: u32 = 2;
const IS_SUPPORTED: u32 = 3;
const GET_IMAPPER_LIBRARY_SUFFIX: u32 = 4;
/// AIDL's meta-transactions (`FIRST_CALL_TRANSACTION + 16777214` and `+ 16777213`).
const GET_INTERFACE_VERSION: u32 = 0x00ff_ffff;
const GET_INTERFACE_HASH: u32 = 0x00ff_fffe;
/// libbinder's `IBinder::INTERFACE_TRANSACTION` (`'_NTF'`) and `PING_TRANSACTION` (`'_PNG'`).
const INTERFACE_TRANSACTION: u32 = 0x5f4e_5446;
const PING_TRANSACTION: u32 = 0x5f50_4e47;

/// `AllocationError`.
const BAD_DESCRIPTOR: i32 = 0;
const NO_RESOURCES: i32 = 1;
const UNSUPPORTED: i32 = 2;

/// `BufferUsage.PROTECTED`: memory the CPU may not see. There is none here.
const USAGE_PROTECTED: i64 = 1 << 14;

/// `PixelFormat`s this allocator lays out, and their bytes per pixel.
fn bytes_per_pixel(format: i32) -> Option<u64> {
    match format {
        // RGBA_8888, RGBX_8888, BGRA_8888, RGBA_1010102, and IMPLEMENTATION_DEFINED (as RGBA_8888).
        1 | 2 | 5 | 0x2b | 0x22 => Some(4),
        3 => Some(3),              // RGB_888
        4 => Some(2),              // RGB_565
        0x16 => Some(8),           // RGBA_FP16
        0x38 | 0x21 => Some(1),    // R_8, BLOB
        _ => None,
    }
}

const BLOB: i32 = 0x21;

struct Descriptor {
    name: Vec<u8>,
    width: i32,
    height: i32,
    layers: i32,
    format: i32,
    usage: i64,
}

impl Descriptor {
    /// `BufferDescriptorInfo`, as the NDK writes it.
    fn read(r: &mut Reader<'_>) -> Result<Self, Malformed> {
        let end = r.parcelable_start()?;
        let name = r.byte_array()?.ok_or(Malformed("a null name"))?;
        let name = name.iter().take_while(|&&b| b != 0).take(127).copied().collect();
        let (width, height, layers, format) = (r.i32()?, r.i32()?, r.i32()?, r.i32()?);
        let usage = r.i64()?;
        // reservedSize and additionalOptions follow; nothing here reads them.
        r.parcelable_end(end)?;
        Ok(Self { name, width, height, layers, format, usage })
    }

    /// The layout of a buffer of this description: (stride in pixels, pixel bytes), or the
    /// `AllocationError` it is refused with.
    fn layout(&self) -> Result<(u64, u64), i32> {
        let bpp = bytes_per_pixel(self.format).ok_or(UNSUPPORTED)?;
        if self.layers != 1 || self.usage & USAGE_PROTECTED != 0 {
            return Err(UNSUPPORTED);
        }
        let (Ok(width), Ok(height)) = (u64::try_from(self.width), u64::try_from(self.height)) else { return Err(BAD_DESCRIPTOR) };
        if width == 0 || height == 0 || (self.format == BLOB && height != 1) {
            return Err(BAD_DESCRIPTOR);
        }
        let stride = if self.format == BLOB { width } else { width.next_multiple_of(16) };
        let bytes = stride.checked_mul(height).and_then(|n| n.checked_mul(bpp)).filter(|&n| n <= MAX_BYTES).ok_or(NO_RESOURCES)?;
        Ok((stride, bytes))
    }
}

/// The allocator: every buffer it made that still exists, by id.
pub struct Allocator {
    next_id: AtomicU64,
    buffers: Mutex<HashMap<u64, Weak<Shm>>>,
}

impl Allocator {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self { next_id: AtomicU64::new(1), buffers: Mutex::default() })
    }

    /// Serve this allocator on `broker` and publish it with `servicemanager` as [`INSTANCE`].
    ///
    /// # Errors
    /// `servicemanager`'s refusal (e.g. the instance is not declared in VINTF).
    pub fn register(self: &Arc<Self>, broker: &Broker) -> Result<(), String> {
        let me = Arc::clone(self);
        let ptr = broker.create_host_service_objects(move |call| me.call(call));
        broker.add_service_with_stability(INSTANCE, ptr, STABILITY_VINTF)
    }

    /// The region of buffer `id`, while any process still holds it.
    #[must_use]
    pub fn buffer(&self, id: u64) -> Option<Arc<Shm>> {
        self.buffers.lock().get(&id).and_then(Weak::upgrade)
    }

    /// Every buffer some process still holds, by id.
    #[must_use]
    pub fn live(&self) -> Vec<(u64, Arc<Shm>)> {
        self.buffers.lock().iter().filter_map(|(id, b)| Some((*id, b.upgrade()?))).collect()
    }

    /// Answer one transaction.
    #[must_use]
    pub fn call(&self, call: HostCall) -> HostReply {
        let mut w = Writer::new();
        match call.code {
            INTERFACE_TRANSACTION => {
                w.string16(DESCRIPTOR);
                return w.into_reply();
            }
            PING_TRANSACTION => return w.into_reply(),
            _ => {}
        }
        if let Err(Malformed(why)) = self.answer(call.code, &call.data, &mut w) {
            w = Writer::new();
            w.exception(EX_ILLEGAL_ARGUMENT, why, None);
        }
        w.into_reply()
    }

    fn answer(&self, code: u32, data: &[u8], w: &mut Writer) -> Result<(), Malformed> {
        let mut r = Reader::new(data);
        r.interface_token(DESCRIPTOR)?;
        match code {
            ALLOCATE => w.exception(EX_SERVICE_SPECIFIC, "allocate takes a mapper@4 descriptor; use allocate2", Some(UNSUPPORTED)),
            ALLOCATE2 => {
                let d = Descriptor::read(&mut r)?;
                let count = r.i32()?;
                self.allocate(&d, count, w);
            }
            IS_SUPPORTED => {
                let d = Descriptor::read(&mut r)?;
                w.status_ok();
                w.bool(d.layout().is_ok());
            }
            GET_IMAPPER_LIBRARY_SUFFIX => {
                w.status_ok();
                w.string16(MAPPER_SUFFIX);
            }
            GET_INTERFACE_VERSION => {
                w.status_ok();
                w.i32(VERSION);
            }
            GET_INTERFACE_HASH => {
                w.status_ok();
                w.string16(HASH);
            }
            _ => w.exception(EX_UNSUPPORTED_OPERATION, "unknown transaction", None),
        }
        Ok(())
    }

    /// `allocate2`: `count` buffers of description `d`, or the `AllocationError` refusing them.
    fn allocate(&self, d: &Descriptor, count: i32, w: &mut Writer) {
        let refuse = |w: &mut Writer, e: i32| w.exception(EX_SERVICE_SPECIFIC, "", Some(e));
        let (stride, bytes) = match d.layout() {
            Ok(l) => l,
            Err(e) => return refuse(w, e),
        };
        if count <= 0 {
            return refuse(w, BAD_DESCRIPTOR);
        }
        if count > MAX_COUNT {
            return refuse(w, NO_RESOURCES);
        }
        let mut made = Vec::new();
        for _ in 0..count {
            match self.make(d, bytes) {
                Ok(b) => made.push(b),
                Err(()) => return refuse(w, NO_RESOURCES),
            }
        }
        w.status_ok();
        let result = w.parcelable_start();
        w.i32(stride as i32);
        w.i32(made.len() as i32);
        for (id, shm) in made {
            let handle = w.parcelable_start();
            w.i32(1);
            w.file(Arc::new(OpenFile { kind: Mutex::new(FileKind::Shared(shm)), flags: Mutex::new(2) }));
            let usage = d.usage as u64;
            let ints = [
                MAGIC as i32,
                LAYOUT_VERSION,
                d.width,
                d.height,
                d.layers,
                d.format,
                usage as u32 as i32,
                (usage >> 32) as u32 as i32,
                stride as i32,
                id as u32 as i32,
                (id >> 32) as u32 as i32,
                bytes as u32 as i32,
                (bytes >> 32) as u32 as i32,
                PIXELS_AT as i32,
            ];
            w.i32_array(&ints);
            w.parcelable_end(handle);
        }
        w.parcelable_end(result);
    }

    /// One buffer's region: zeroed, its name in the metadata page.
    fn make(&self, d: &Descriptor, bytes: u64) -> Result<(u64, Arc<Shm>), ()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let shm = Shm::create(&format!("omni-gralloc-{id}")).map_err(|_| ())?;
        shm.set_len(PIXELS_AT + bytes).map_err(|_| ())?;
        shm.as_graphics_buffer();
        let mut name = d.name.clone();
        name.push(0);
        shm.write_at(&name, NAME_AT).map_err(|_| ())?;
        let mut buffers = self.buffers.lock();
        buffers.retain(|_, b| b.strong_count() > 0);
        buffers.insert(id, Arc::downgrade(&shm));
        Ok((id, shm))
    }
}
