//! AIDL parcels as a host HAL reads and writes them: what libbinder's `Parcel` and the NDK backend
//! (`binder_parcel_utils.h`, `ndk/parcel.cpp`) put on the wire. A parcel is guest input: every read
//! is bounds-checked and a malformed parcel is a [`Malformed`] error, never a panic.
//!
//! The wire, as the NDK backend writes it: every value little-endian, the position always 4-byte
//! aligned. `boolean`, `byte`, `char` and `int` are one `i32` each; `long` is an `i64` and `double`
//! an `f64` (8 bytes, not 8-aligned); `float` an `f32`. A string is a `String16`. An array is its
//! `i32` length (-1 for null), then its elements -- a `byte[]` (and an array of a byte-backed enum)
//! packed and padded to 4, any other primitive array one element each. A parcelable anywhere is a
//! non-null marker (`i32` 1, or 0 for null), then its size (counting itself) and its fields; a
//! union is a marker, its tag and the one field. A `ParcelFileDescriptor` is a marker, an `i32` 0
//! (no comm channel) and a `TYPE_FD` object; a binder is a `flat_binder_object` and its stability.
use std::sync::Arc;

use crate::fd::OpenFile;

/// A parcel that does not hold what its reader expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

/// `binder::Status` exception codes (`Status.h`).
pub const EX_ILLEGAL_ARGUMENT: i32 = -3;
pub const EX_UNSUPPORTED_OPERATION: i32 = -7;
pub const EX_SERVICE_SPECIFIC: i32 = -8;
/// A "fat" reply header (Java's strict-mode reply), which native code skips.
pub const EX_HAS_REPLY_HEADER: i32 = -128;

/// `flat_binder_object` types.
const TYPE_FD: u32 = 0x6664_2a85;
const TYPE_BINDER: u32 = 0x7362_2a85;
const TYPE_WEAK_BINDER: u32 = 0x7762_2a85;
const TYPE_HANDLE: u32 = 0x7368_2a85;
const TYPE_WEAK_HANDLE: u32 = 0x7768_2a85;
const FLAT_BINDER_FLAG_ACCEPTS_FDS: u32 = 0x100;
/// The size of a `flat_binder_object`.
const OBJECT_SIZE: usize = 24;

/// libbinder's interface header for the system partition, `'SYST'`.
const INTERFACE_HEADER_SYSTEM: i32 = 0x5359_5354;

/// How deep parcelables and unions may nest in one parcel: guest input must not recurse the host
/// out of stack.
const MAX_DEPTH: u32 = 64;

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    /// Where the parcel's objects are, as the transaction listed them.
    offsets: &'a [u64],
    /// The file descriptors the transaction carried, in object order (`HostCall::fds`).
    fds: &'a [Arc<OpenFile>],
    depth: u32,
}

impl<'a> Reader<'a> {
    /// A parcel of bytes only: reading a file descriptor or a binder handle from it is malformed.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, offsets: &[], fds: &[], depth: 0 }
    }

    /// A transaction's parcel with its objects: the offsets it listed, and the files its `TYPE_FD`
    /// objects carried, in object order (as `HostCall` has them).
    #[must_use]
    pub fn with_objects(data: &'a [u8], offsets: &'a [u64], fds: &'a [Arc<OpenFile>]) -> Self {
        Self { data, pos: 0, offsets, fds, depth: 0 }
    }

    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// The bytes after the position.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        let end = self.pos.checked_add(n).ok_or(Malformed("length overflows"))?;
        let b = self.data.get(self.pos..end).ok_or(Malformed("reads past the end"))?;
        self.pos = end;
        Ok(b)
    }

    /// Skip `n` bytes (and the padding to 4 after them).
    pub fn skip(&mut self, n: usize) -> Result<(), Malformed> {
        self.take(n)?;
        self.align();
        Ok(())
    }

    pub fn i32(&mut self) -> Result<i32, Malformed> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    pub fn i64(&mut self) -> Result<i64, Malformed> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }

    /// A `byte`: an `i32`, of which the reader keeps the low 8 bits (`Parcel::readByte`).
    pub fn i8(&mut self) -> Result<i8, Malformed> {
        Ok(self.i32()? as i8)
    }

    /// A `boolean`: an `i32`, true when not 0.
    pub fn bool(&mut self) -> Result<bool, Malformed> {
        Ok(self.i32()? != 0)
    }

    /// A `char`: a UTF-16 unit in an `i32`.
    pub fn char16(&mut self) -> Result<u16, Malformed> {
        Ok(self.i32()? as u16)
    }

    pub fn f32(&mut self) -> Result<f32, Malformed> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    pub fn f64(&mut self) -> Result<f64, Malformed> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().expect("8")))
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
        let Some(len) = self.array_len(1)? else { return Ok(None) };
        let b = self.take(len)?;
        self.align();
        Ok(Some(b))
    }

    /// An array's length (-1 for null): refused when `len` elements of at least `min_bytes` each
    /// cannot fit in what is left, so a guest's length never sizes a host allocation by itself.
    pub fn array_len(&mut self, min_bytes: usize) -> Result<Option<usize>, Malformed> {
        let len = self.i32()?;
        if len == -1 {
            return Ok(None);
        }
        let len = usize::try_from(len).map_err(|_| Malformed("negative array length"))?;
        if len.checked_mul(min_bytes.max(1)).is_none_or(|n| n > self.remaining()) {
            return Err(Malformed("array longer than the parcel"));
        }
        Ok(Some(len))
    }

    /// An array of fixed-size elements, each `size` bytes (`i32`s for `boolean`, `char` and `int`).
    fn array_of<T>(&mut self, size: usize, f: impl Fn(&[u8]) -> T) -> Result<Option<Vec<T>>, Malformed> {
        let Some(len) = self.array_len(size)? else { return Ok(None) };
        let bytes = self.take(len * size)?;
        Ok(Some(bytes.chunks_exact(size).map(f).collect()))
    }

    pub fn i32_array(&mut self) -> Result<Option<Vec<i32>>, Malformed> {
        self.array_of(4, |b| i32::from_le_bytes(b.try_into().expect("4")))
    }

    pub fn i64_array(&mut self) -> Result<Option<Vec<i64>>, Malformed> {
        self.array_of(8, |b| i64::from_le_bytes(b.try_into().expect("8")))
    }

    pub fn f32_array(&mut self) -> Result<Option<Vec<f32>>, Malformed> {
        self.array_of(4, |b| f32::from_le_bytes(b.try_into().expect("4")))
    }

    pub fn f64_array(&mut self) -> Result<Option<Vec<f64>>, Malformed> {
        self.array_of(8, |b| f64::from_le_bytes(b.try_into().expect("8")))
    }

    pub fn bool_array(&mut self) -> Result<Option<Vec<bool>>, Malformed> {
        self.array_of(4, |b| i32::from_le_bytes(b.try_into().expect("4")) != 0)
    }

    pub fn char16_array(&mut self) -> Result<Option<Vec<u16>>, Malformed> {
        self.array_of(4, |b| i32::from_le_bytes(b.try_into().expect("4")) as u16)
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

    /// A nullable value's marker (`AParcel_readNullableParcelable`): 0 for null, anything else
    /// for a value that follows.
    pub fn marker(&mut self) -> Result<bool, Malformed> {
        Ok(self.i32()? != 0)
    }

    /// A non-null parcelable's start: the NDK's non-null marker, then the parcelable's size
    /// (counting itself). The position its fields end at.
    pub fn parcelable_start(&mut self) -> Result<usize, Malformed> {
        if self.i32()? != 1 {
            return Err(Malformed("a null parcelable"));
        }
        self.parcelable_body()
    }

    /// A parcelable's body starts (after its marker): its size, counting itself. The position its
    /// fields end at; pass it to [`Self::within`] and [`Self::parcelable_end`].
    pub fn parcelable_body(&mut self) -> Result<usize, Malformed> {
        let start = self.pos;
        let size = usize::try_from(self.i32()?).map_err(|_| Malformed("negative parcelable size"))?;
        if size < 4 {
            return Err(Malformed("parcelable size below its header"));
        }
        let end = start.checked_add(size).filter(|&end| end <= self.data.len()).ok_or(Malformed("parcelable past the end"))?;
        self.enter()?;
        Ok(end)
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
        self.leave();
        Ok(())
    }

    /// One level deeper into nested parcelables or unions; refused past `MAX_DEPTH` (64).
    pub fn enter(&mut self) -> Result<(), Malformed> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Malformed("parcelables nested too deep"));
        }
        Ok(())
    }

    pub fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// The `flat_binder_object` at the position, which the transaction must list as an object:
    /// its type and its `binder`/`handle`/`fd` word.
    fn object(&mut self) -> Result<(usize, u32, u64), Malformed> {
        let at = self.pos;
        let obj = self.take(OBJECT_SIZE)?;
        let kind = u32::from_le_bytes(obj[0..4].try_into().expect("4"));
        let value = u64::from_le_bytes(obj[8..16].try_into().expect("8"));
        Ok((at, kind, value))
    }

    fn listed(&self, at: usize) -> bool {
        self.offsets.contains(&(at as u64))
    }

    fn type_at(&self, at: u64) -> Option<u32> {
        let at = usize::try_from(at).ok()?;
        Some(u32::from_le_bytes(self.data.get(at..at.checked_add(4)?)?.try_into().ok()?))
    }

    /// The file of the `TYPE_FD` object at the position: the transaction's file for it, by the
    /// object's place among the `TYPE_FD` objects the transaction listed.
    fn fd_object(&mut self) -> Result<Arc<OpenFile>, Malformed> {
        let (at, kind, _) = self.object()?;
        if kind != TYPE_FD {
            return Err(Malformed("not a file descriptor object"));
        }
        if !self.listed(at) {
            return Err(Malformed("a file descriptor the transaction did not list"));
        }
        let index = self.offsets.iter().take_while(|&&o| o != at as u64).filter(|&&o| self.type_at(o) == Some(TYPE_FD)).count();
        self.fds.get(index).cloned().ok_or(Malformed("a file descriptor the transaction did not carry"))
    }

    /// A `ParcelFileDescriptor` (`AParcel_readNullableParcelFileDescriptor`): its marker (`None`
    /// for null), whether it has a comm channel, its `TYPE_FD` object (and the channel's, which is
    /// dropped).
    pub fn file(&mut self) -> Result<Option<Arc<OpenFile>>, Malformed> {
        if !self.marker()? {
            return Ok(None);
        }
        let comm = self.i32()?;
        let file = self.fd_object()?;
        if comm != 0 {
            self.fd_object()?;
        }
        Ok(Some(file))
    }

    /// A strong binder (`readStrongBinder`): a `flat_binder_object` and its stability. The handle
    /// (in the host's table) of a binder the broker translated for the host; `None` for a null
    /// binder. A handle the transaction did not list is refused: a guest cannot name the host's
    /// handles by writing them.
    pub fn binder(&mut self) -> Result<Option<u32>, Malformed> {
        let (at, kind, value) = self.object()?;
        let handle = match kind {
            TYPE_BINDER | TYPE_WEAK_BINDER if value == 0 => None,
            TYPE_HANDLE | TYPE_WEAK_HANDLE if self.listed(at) => Some(value as u32),
            TYPE_HANDLE | TYPE_WEAK_HANDLE => return Err(Malformed("a binder the transaction did not list")),
            _ => return Err(Malformed("not a binder the host holds a handle to")),
        };
        self.i32()?; // stability
        Ok(handle)
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

    /// A `byte`, sign-extended into an `i32`.
    pub fn i8(&mut self, v: i8) {
        self.i32(i32::from(v));
    }

    pub fn bool(&mut self, v: bool) {
        self.i32(i32::from(v));
    }

    /// A `char`, in an `i32`.
    pub fn char16(&mut self, v: u16) {
        self.i32(i32::from(v));
    }

    pub fn f32(&mut self, v: f32) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    pub fn f64(&mut self, v: f64) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    pub fn string16(&mut self, s: &str) {
        let units: Vec<u16> = s.encode_utf16().collect();
        self.i32(units.len() as i32);
        for u in units.iter().chain([0u16].iter()) {
            self.data.extend_from_slice(&u.to_le_bytes());
        }
        self.pad();
    }

    fn pad(&mut self) {
        self.data.resize((self.data.len() + 3) & !3, 0);
    }

    /// A null array or string: length -1.
    pub fn null(&mut self) {
        self.i32(-1);
    }

    /// A byte array: its length, the bytes, padded to 4.
    pub fn byte_array(&mut self, v: &[u8]) {
        self.i32(v.len() as i32);
        self.data.extend_from_slice(v);
        self.pad();
    }

    pub fn i32_array(&mut self, v: &[i32]) {
        self.i32(v.len() as i32);
        for x in v {
            self.i32(*x);
        }
    }

    pub fn i64_array(&mut self, v: &[i64]) {
        self.i32(v.len() as i32);
        for x in v {
            self.i64(*x);
        }
    }

    pub fn f32_array(&mut self, v: &[f32]) {
        self.i32(v.len() as i32);
        for x in v {
            self.f32(*x);
        }
    }

    pub fn f64_array(&mut self, v: &[f64]) {
        self.i32(v.len() as i32);
        for x in v {
            self.f64(*x);
        }
    }

    pub fn bool_array(&mut self, v: &[bool]) {
        self.i32(v.len() as i32);
        for x in v {
            self.bool(*x);
        }
    }

    pub fn char16_array(&mut self, v: &[u16]) {
        self.i32(v.len() as i32);
        for x in v {
            self.char16(*x);
        }
    }

    /// `Parcel::writeInterfaceToken`, as the host's libbinder-shaped parcels write it
    /// (`binder.rs`): strict-mode policy, work source, the system partition's header, the name.
    pub fn interface_token(&mut self, interface: &str) {
        self.i32(i32::MIN);
        self.i32(-1);
        self.i32(INTERFACE_HEADER_SYSTEM);
        self.string16(interface);
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

    /// A nullable value's marker: 1 when a value follows, 0 for null.
    pub fn marker(&mut self, present: bool) {
        self.i32(i32::from(present));
    }

    /// A non-null parcelable's start (the NDK's marker and a size placeholder); pass the returned
    /// position to [`Self::parcelable_end`].
    pub fn parcelable_start(&mut self) -> usize {
        self.i32(1);
        self.parcelable_body_start()
    }

    /// A parcelable's body starts (after its marker): a size placeholder, which
    /// [`Self::parcelable_end`] fills in.
    pub fn parcelable_body_start(&mut self) -> usize {
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

    /// A nullable `ParcelFileDescriptor`: [`Self::file`], or the null marker.
    pub fn nullable_file(&mut self, file: Option<Arc<OpenFile>>) {
        match file {
            Some(f) => self.file(f),
            None => self.i32(0),
        }
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

    /// A strong binder the host holds a handle to (in the host's table): a `TYPE_HANDLE` object,
    /// which the broker translates for the receiver, and its stability.
    pub fn handle(&mut self, handle: u32, stability: i32) {
        let at = self.data.len();
        self.data.extend_from_slice(&TYPE_HANDLE.to_le_bytes());
        self.data.extend_from_slice(&0u32.to_le_bytes());
        self.data.extend_from_slice(&u64::from(handle).to_le_bytes());
        self.data.extend_from_slice(&0u64.to_le_bytes());
        self.i32(stability);
        self.binders.push(at);
    }

    /// A null binder (`writeStrongBinder(nullptr)`): a `TYPE_BINDER` object of 0, which libbinder
    /// does not list as an object, and the stability `UNDECLARED` (0).
    pub fn null_binder(&mut self) {
        self.data.extend_from_slice(&TYPE_BINDER.to_le_bytes());
        self.data.extend_from_slice(&0u32.to_le_bytes());
        self.data.extend_from_slice(&0u64.to_le_bytes());
        self.data.extend_from_slice(&0u64.to_le_bytes());
        self.i32(0);
    }

    #[must_use]
    pub fn into_reply(self) -> crate::binder::HostReply {
        crate::binder::HostReply { data: self.data, fds: self.fds, binders: self.binders }
    }
}
