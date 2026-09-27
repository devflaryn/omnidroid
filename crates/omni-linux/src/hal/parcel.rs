//! AIDL parcels as a host HAL reads and writes them: what libbinder's `Parcel` and the NDK backend
//! (`binder_parcel_utils.h`, `ndk/parcel.cpp`) put on the wire. A parcel is guest input: every read
//! is bounds-checked and a malformed parcel is a [`Malformed`] error, never a panic.
use std::sync::Arc;

use crate::fd::OpenFile;

/// A parcel that does not hold what its reader expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

/// `binder::Status` exception codes (`Status.h`).
pub const EX_ILLEGAL_ARGUMENT: i32 = -3;
pub const EX_UNSUPPORTED_OPERATION: i32 = -7;
pub const EX_SERVICE_SPECIFIC: i32 = -8;

/// `flat_binder_object` of a file descriptor.
const TYPE_FD: u32 = 0x6664_2a85;
const TYPE_BINDER: u32 = 0x7362_2a85;
const FLAT_BINDER_FLAG_ACCEPTS_FDS: u32 = 0x100;

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        let end = self.pos.checked_add(n).ok_or(Malformed("length overflows"))?;
        let b = self.data.get(self.pos..end).ok_or(Malformed("reads past the end"))?;
        self.pos = end;
        Ok(b)
    }

    pub fn i32(&mut self) -> Result<i32, Malformed> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    pub fn i64(&mut self) -> Result<i64, Malformed> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }

    /// A `String16`: its length in UTF-16 units (-1 for null), the units, a NUL, padded to 4.
    pub fn string16(&mut self) -> Result<Option<String>, Malformed> {
        let len = self.i32()?;
        if len == -1 {
            return Ok(None);
        }
        let len = usize::try_from(len).map_err(|_| Malformed("negative string length"))?;
        let bytes = self.take(len.checked_mul(2).and_then(|n| n.checked_add(2)).ok_or(Malformed("string too long"))?)?;
        self.align();
        let units: Vec<u16> = bytes[..len * 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Ok(Some(String::from_utf16_lossy(&units)))
    }

    /// A byte array: its length (-1 for null), the bytes, padded to 4.
    pub fn byte_array(&mut self) -> Result<Option<&'a [u8]>, Malformed> {
        let len = self.i32()?;
        if len == -1 {
            return Ok(None);
        }
        let len = usize::try_from(len).map_err(|_| Malformed("negative array length"))?;
        let b = self.take(len)?;
        self.align();
        Ok(Some(b))
    }

    fn align(&mut self) {
        self.pos = (self.pos + 3) & !3;
    }

    /// The interface token libbinder's `writeInterfaceToken` leads a transaction with: strict-mode
    /// policy, work source, partition header, the interface's name -- which must be `interface`.
    pub fn interface_token(&mut self, interface: &str) -> Result<(), Malformed> {
        self.i32()?;
        self.i32()?;
        self.i32()?;
        match self.string16()? {
            Some(name) if name == interface => Ok(()),
            _ => Err(Malformed("another interface's token")),
        }
    }

    /// A non-null parcelable's start: the NDK's non-null marker, then the parcelable's size
    /// (counting itself). The position its fields end at.
    pub fn parcelable_start(&mut self) -> Result<usize, Malformed> {
        if self.i32()? != 1 {
            return Err(Malformed("a null parcelable"));
        }
        let start = self.pos;
        let size = usize::try_from(self.i32()?).map_err(|_| Malformed("negative parcelable size"))?;
        if size < 4 {
            return Err(Malformed("parcelable size below its header"));
        }
        start.checked_add(size).filter(|&end| end <= self.data.len()).ok_or(Malformed("parcelable past the end"))
    }

    /// Whether the parcelable's fields end before `end` (an older writer sent fewer).
    #[must_use]
    pub fn within(&self, end: usize) -> bool {
        self.pos < end
    }

    /// Continue after a parcelable that ends at `end` (a newer writer's extra fields are skipped).
    pub fn parcelable_end(&mut self, end: usize) -> Result<(), Malformed> {
        if self.pos > end {
            return Err(Malformed("fields past the parcelable's size"));
        }
        self.pos = end;
        Ok(())
    }
}

/// A reply being written: its bytes, and the objects in them.
#[derive(Default)]
pub struct Writer {
    pub data: Vec<u8>,
    pub fds: Vec<(usize, Arc<OpenFile>)>,
    pub binders: Vec<usize>,
}

impl Writer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn i32(&mut self, v: i32) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bool(&mut self, v: bool) {
        self.i32(i32::from(v));
    }

    pub fn string16(&mut self, s: &str) {
        let units: Vec<u16> = s.encode_utf16().collect();
        self.i32(units.len() as i32);
        for u in units.iter().chain([0u16].iter()) {
            self.data.extend_from_slice(&u.to_le_bytes());
        }
        self.data.resize((self.data.len() + 3) & !3, 0);
    }

    pub fn i32_array(&mut self, v: &[i32]) {
        self.i32(v.len() as i32);
        for x in v {
            self.i32(*x);
        }
    }

    /// `binder::Status` of success: `EX_NONE`.
    pub fn status_ok(&mut self) {
        self.i32(0);
    }

    /// `binder::Status::writeToParcel` of an exception: its code, message, an empty remote stack
    /// trace header, and a service-specific error's code.
    pub fn exception(&mut self, code: i32, message: &str, service_specific: Option<i32>) {
        self.i32(code);
        self.string16(message);
        self.i32(0);
        if let Some(e) = service_specific {
            self.i32(e);
        }
    }

    /// A non-null parcelable's start (the NDK's marker and a size placeholder); pass the returned
    /// position to [`Self::parcelable_end`].
    pub fn parcelable_start(&mut self) -> usize {
        self.i32(1);
        let at = self.data.len();
        self.i32(0);
        at
    }

    pub fn parcelable_end(&mut self, at: usize) {
        let size = (self.data.len() - at) as i32;
        self.data[at..at + 4].copy_from_slice(&size.to_le_bytes());
    }

    /// A required `ParcelFileDescriptor` (`AParcel_writeRequiredParcelFileDescriptor`): non-null,
    /// no comm channel, the descriptor as a `TYPE_FD` object.
    pub fn file(&mut self, file: Arc<OpenFile>) {
        self.i32(1);
        self.i32(0);
        let at = self.data.len();
        self.data.extend_from_slice(&TYPE_FD.to_le_bytes());
        self.data.extend_from_slice(&0x7fu32.to_le_bytes());
        self.data.extend_from_slice(&u64::MAX.to_le_bytes());
        self.data.extend_from_slice(&1u64.to_le_bytes()); // the receiver owns it
        self.fds.push((at, file));
    }

    /// A strong binder to host service `ptr`, with its stability (`Parcel::writeStrongBinder`).
    pub fn binder(&mut self, ptr: u64, stability: i32) {
        let at = self.data.len();
        self.data.extend_from_slice(&TYPE_BINDER.to_le_bytes());
        self.data.extend_from_slice(&(0x7f | FLAT_BINDER_FLAG_ACCEPTS_FDS).to_le_bytes());
        self.data.extend_from_slice(&ptr.to_le_bytes());
        self.data.extend_from_slice(&ptr.to_le_bytes());
        self.i32(stability);
        self.binders.push(at);
    }

    #[must_use]
    pub fn into_reply(self) -> crate::binder::HostReply {
        crate::binder::HostReply { data: self.data, fds: self.fds, binders: self.binders }
    }
}
