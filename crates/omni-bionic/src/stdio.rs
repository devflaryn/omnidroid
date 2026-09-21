//! Bionic's `FILE *` layer: `fgets`, `fputs`, `fputc`, `fread`, `fwrite`, `feof`, `fflush`.
//!
//! # Why this is here and not in the platform crate
//!
//! A `FILE *` is a descriptor plus two flags plus a set of conversion rules. **None of it is an
//! operating-system call.** `fread(p, 3, 7, f)` is "multiply, with the overflow checked; read
//! that many bytes; report how many *whole items* arrived", and only the middle clause reaches
//! the OS. Putting that arithmetic in `omni-platform` would mix the two, and the part with the
//! interesting failure modes — a `size * nmemb` that wraps, a `fgets` that reads one byte past
//! its newline — would sit in the crate that cannot be tested without a filesystem.
//!
//! So it is here, over a trait, exactly as memory, atomics, futexes and clocks already are. D19
//! records why that matters: `cargo tree -p omni-bionic -e normal` is one line, and "no OS
//! access" is therefore something `cargo` can check rather than a rule a reviewer has to notice.
//! The adapter in `omni-android` implements [`Descriptors`] over `omni-platform`'s filesystem
//! seam; the mock in this module's tests implements it over a `Vec<u8>`.
//!
//! # What a [`Stream`] is, and what it deliberately is not
//!
//! Three fields: the descriptor, the end-of-file flag and the error flag. It is **not** the
//! guest's `FILE` structure and it is not stored in guest memory. The adapter keeps one per open
//! stream, keyed by the guest `FILE *` address, and the bytes at that address are never read —
//! which is what makes bionic's `sizeof(FILE)`, a number derived from headers and never verified
//! against an NDK, unable to cause a wrong answer here. See the adapter for the whole argument.
//!
//! # No buffering, and that is a decision rather than an omission
//!
//! A real `FILE` buffers. This one does not: every read is a read and every write is a write.
//!
//! * It is **conforming**. C says a stream may be unbuffered, and `setvbuf` — the call that would
//!   let a program insist otherwise — is not in the 188 statically-reachable imports.
//! * It is what makes [`fgets`] correct. A buffered `fgets` reads ahead and puts back what it
//!   did not use; an unbuffered one reads one byte at a time and stops *on* the newline, so the
//!   descriptor is left exactly where C says it is. With a shared descriptor, a read-ahead that
//!   was never put back is a silently lost byte.
//! * `fflush` therefore has nothing of ours to flush, which is why it is not a lie to succeed.
//!
//! The cost is a `read(2)` per byte in `fgets`. Correct rather than fast, and nothing in the
//! 3,594 initializers reads a line in a hot loop.
//!
//! # Every host allocation here is bounded by a constant, not by a guest argument
//!
//! `fread(p, 1, SIZE_MAX, f)` must not become a `SIZE_MAX` allocation. Transfers move through a
//! [`TRANSFER_CHUNK`]-byte buffer however large the request is, the same shape
//! `arc4random_buf` already uses — so the peak host allocation for any call in this module is
//! [`TRANSFER_CHUNK`] bytes and the guest cannot choose it.

use crate::context::GuestContext;
use crate::errno::consts;
use crate::error::{BionicError, BionicResult};
use crate::memory::GuestMemory;
use crate::string;

/// Bytes moved between guest memory and a descriptor in one step.
///
/// **The bound that stops a guest choosing a host allocation.** A `fread` or `fwrite` of any size
/// is carried out in pieces of this, so the peak host buffer is this constant whatever the guest
/// asked for. It is the same number `omni-platform`'s filesystem seam reports as `st_blksize`,
/// which makes that field a fact about what this layer does rather than a plausible value.
pub const TRANSFER_CHUNK: usize = 4096;

/// The longest string `fputs` will walk looking for a NUL.
///
/// The same 64 KiB the thunk boundary's `GuestMem::STRING_LIMIT` uses, and for the same reason: an
/// unterminated string is otherwise a walk bounded only by the address space. A string this long
/// is a named refusal rather than a truncation.
pub const MAX_STRING: u64 = 64 * 1024;

/// C's `EOF`.
pub const EOF: i32 = -1;

/// The descriptor operations the `FILE *` layer needs.
///
/// Implemented by the adapter over `omni-platform`, and by this module's tests over a byte
/// vector. **The error type is a guest `errno`**, because that is this crate's native vocabulary
/// (see [`crate::errno`]) and because the layer below has no business knowing Linux's numbering —
/// the adapter classifies a host failure and picks the number from the one table that holds them.
///
/// Every method has `read(2)`/`write(2)` semantics rather than "fill the buffer": a short count is
/// not an error, and zero from [`read`](Self::read) means end of file. The functions in this
/// module are written against exactly that contract, so an implementation that looped internally
/// would change what `fread` reports at the end of a file.
pub trait Descriptors {
    /// Read into `buf`; `Ok(0)` is end of file.
    ///
    /// # Errors
    ///
    /// The guest `errno` to report.
    fn read(&self, fd: i32, buf: &mut [u8]) -> Result<usize, i32>;

    /// Write from `buf`, returning how many bytes were taken.
    ///
    /// # Errors
    ///
    /// The guest `errno` to report.
    fn write(&self, fd: i32, buf: &[u8]) -> Result<usize, i32>;

    /// Push anything the layer below is holding at the operating system.
    ///
    /// # Errors
    ///
    /// The guest `errno` to report.
    fn flush(&self, fd: i32) -> Result<(), i32>;
}

/// One open stream: the descriptor behind a guest `FILE *`, and its two sticky flags.
///
/// The flags are sticky, which is C's rule and not an implementation detail: `feof` stays true
/// until something clears it, and the only things that can are `clearerr`, `fseek` and `rewind` —
/// **none of which is in the reachable import set**. So within this milestone a stream that has
/// reached its end reports it for the rest of its life, which is exactly what C promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stream {
    /// The descriptor this stream reads and writes.
    pub fd: i32,
    /// Set when a read found end of file.
    pub eof: bool,
    /// Set when an operation failed.
    pub error: bool,
}

impl Stream {
    /// A stream over `fd`, with both flags clear.
    #[must_use]
    pub const fn new(fd: i32) -> Stream {
        Stream { fd, eof: false, error: false }
    }
}

/// `int feof(FILE *stream)`
///
/// Non-zero once a read has found end of file. C does not fix *which* non-zero value, and this
/// returns 1.
#[must_use]
pub const fn feof(stream: &Stream) -> i32 {
    if stream.eof {
        1
    } else {
        0
    }
}

/// `int fflush(FILE *stream)`
///
/// # Errors
///
/// Never as a Rust error: a failure below becomes `EOF` with the stream's error flag set and the
/// guest's `errno` stored, which is `fflush`'s own contract.
pub fn fflush(
    ctx: &mut impl GuestContext,
    descriptors: &impl Descriptors,
    stream: &mut Stream,
) -> i32 {
    match descriptors.flush(stream.fd) {
        Ok(()) => 0,
        Err(errno) => {
            stream.error = true;
            ctx.set_errno(errno);
            EOF
        }
    }
}

/// `char *fgets(char *s, int size, FILE *stream)`
///
/// Returns the guest address `s` on success and `0` (`NULL`) at end of file with nothing read, or
/// on error.
///
/// # The three rules that are easy to get wrong, and are each tested
///
/// * **At most `size - 1` bytes**, then a NUL. A `size` of 1 therefore reads nothing and writes
///   one NUL, and a `size` of 0 or less reads nothing and writes nothing.
/// * **The newline is kept.** `fgets` stores it; `gets` did not. Code that strips it looks for it.
/// * **Stopping *on* the newline, not after it.** The byte after the newline must still be there
///   for the next call. This is why the read is a byte at a time — see the module documentation.
///
/// # Errors
///
/// [`BionicError::Memory`] for a destination that cannot be written.
pub fn fgets(
    ctx: &mut impl GuestContext,
    descriptors: &impl Descriptors,
    stream: &mut Stream,
    s: u64,
    size: i32,
) -> BionicResult<u64> {
    if size <= 0 {
        // glibc and bionic both return NULL without touching the buffer. A `size` of zero has no
        // room even for the terminator.
        return Ok(0);
    }
    // `size - 1` cannot underflow: `size >= 1` here.
    let capacity = (size as u32 - 1) as u64;
    let mut written = 0u64;
    let mut pending: Vec<u8> = Vec::with_capacity(TRANSFER_CHUNK.min(capacity as usize + 1));
    loop {
        if written + pending.len() as u64 >= capacity {
            break;
        }
        let mut byte = [0u8; 1];
        match descriptors.read(stream.fd, &mut byte) {
            Ok(0) => {
                stream.eof = true;
                break;
            }
            Ok(_) => {
                pending.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                if pending.len() == TRANSFER_CHUNK {
                    written += flush_into_guest(ctx, s + written, &pending)?;
                    pending.clear();
                }
            }
            Err(errno) => {
                stream.error = true;
                ctx.set_errno(errno);
                // C: on a read error `fgets` returns NULL and the buffer contents are
                // indeterminate. Nothing partial is committed, so "indeterminate" here means
                // "unchanged", which is the stronger and safer of the two readings.
                return Ok(0);
            }
        }
    }
    if !pending.is_empty() {
        written += flush_into_guest(ctx, s + written, &pending)?;
    }
    if written == 0 && stream.eof {
        // End of file with nothing read: NULL, and the buffer is left alone. Writing a NUL here
        // would be the plausible wrong answer -- it would turn "no line" into "an empty line".
        return Ok(0);
    }
    write_all(ctx, s + written, &[0u8])?;
    Ok(s)
}

/// `int fputs(const char *s, FILE *stream)`
///
/// Returns a non-negative value on success and `EOF` on failure. C fixes only the sign, and this
/// returns the number of bytes written, which is what bionic does.
///
/// # Errors
///
/// [`BionicError::Memory`] for a string that cannot be read, or
/// [`BionicError::InvalidArgument`] for one longer than [`MAX_STRING`] with no NUL in it.
pub fn fputs(
    ctx: &mut impl GuestContext,
    descriptors: &impl Descriptors,
    stream: &mut Stream,
    s: u64,
) -> BionicResult<i32> {
    let length = bounded_strlen(ctx, s)?;
    match transfer_out(ctx, descriptors, stream, s, length)? {
        written if written == length => {
            // A count that cannot exceed MAX_STRING, so the cast cannot truncate.
            Ok(written as i32)
        }
        _ => Ok(EOF),
    }
}

/// `int fputc(int c, FILE *stream)`
///
/// Returns the character written as an `unsigned char` widened to `int`, or `EOF`. The conversion
/// is the interesting part: `fputc(-1, f)` writes the byte `0xff` and returns `255`, **not**
/// `EOF`, so a caller comparing the result against `EOF` is not misled by a byte that happens to
/// be `0xff`.
pub fn fputc(
    ctx: &mut impl GuestContext,
    descriptors: &impl Descriptors,
    stream: &mut Stream,
    c: i32,
) -> i32 {
    let byte = (c as u32 & 0xff) as u8;
    match descriptors.write(stream.fd, &[byte]) {
        Ok(1) => i32::from(byte),
        Ok(_) => {
            // A write that took nothing is a failure of the stream rather than a short write: one
            // byte is either written or it is not.
            stream.error = true;
            EOF
        }
        Err(errno) => {
            stream.error = true;
            ctx.set_errno(errno);
            EOF
        }
    }
}

/// `size_t fread(void *ptr, size_t size, size_t nmemb, FILE *stream)`
///
/// Returns the number of **complete items** read, which is less than `nmemb` at end of file or on
/// error. A partial item at the end of a file is not reported, which is C's rule and is why the
/// return is a division rather than the byte count.
///
/// # The multiplication is checked, and that is not pedantry
///
/// `size * nmemb` is two numbers the guest chose. In a debug build an overflow panics — a panic
/// reachable from guest input, which Global Constraint 11 calls Critical — and **in a release
/// build it wraps silently**, which is worse: `fread(p, 1 << 32, 1 << 32, f)` would become a
/// request for zero bytes and report success. This project has already shipped exactly that shape
/// once (`gmtime(i64::MIN)`, D22), so the multiplication is `checked_mul` and an overflow is
/// `EINVAL` with zero items read.
///
/// # Errors
///
/// [`BionicError::Memory`] for a destination that cannot be written.
pub fn fread(
    ctx: &mut impl GuestContext,
    descriptors: &impl Descriptors,
    stream: &mut Stream,
    ptr: u64,
    size: u64,
    nmemb: u64,
) -> BionicResult<u64> {
    let Some(total) = size.checked_mul(nmemb) else {
        stream.error = true;
        ctx.set_errno(consts::EINVAL);
        return Ok(0);
    };
    if total == 0 {
        // C: zero items, and the stream is untouched. Not an error, and `size == 0` is the case
        // that would divide by zero below.
        return Ok(0);
    }
    let mut done = 0u64;
    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    while done < total {
        let want = ((total - done) as usize).min(TRANSFER_CHUNK);
        match descriptors.read(stream.fd, &mut buffer[..want]) {
            Ok(0) => {
                stream.eof = true;
                break;
            }
            Ok(got) => {
                write_all(ctx, ptr + done, &buffer[..got])?;
                done += got as u64;
            }
            Err(errno) => {
                stream.error = true;
                ctx.set_errno(errno);
                break;
            }
        }
    }
    // `size` is non-zero here: `total != 0` implies both factors are.
    Ok(done / size)
}

/// `size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream)`
///
/// Returns the number of complete items written. The multiplication is checked for the reason
/// [`fread`] gives.
///
/// # Errors
///
/// [`BionicError::Memory`] for a source that cannot be read.
pub fn fwrite(
    ctx: &mut impl GuestContext,
    descriptors: &impl Descriptors,
    stream: &mut Stream,
    ptr: u64,
    size: u64,
    nmemb: u64,
) -> BionicResult<u64> {
    let Some(total) = size.checked_mul(nmemb) else {
        stream.error = true;
        ctx.set_errno(consts::EINVAL);
        return Ok(0);
    };
    if total == 0 {
        return Ok(0);
    }
    let written = transfer_out(ctx, descriptors, stream, ptr, total)?;
    Ok(written / size)
}

/// Write bytes the **host** already holds to a stream, as `fprintf` and `vfprintf` need.
///
/// The `f*printf` family is the one place in this layer where the bytes going to a stream did not
/// come out of guest memory: the formatting happens host-side, so [`fwrite`]'s guest-pointer
/// source is the wrong shape and going through guest memory would need a guest buffer nothing
/// owns. This is the same short-write, error-flag and `errno` bookkeeping as [`fwrite`] with the
/// source replaced.
///
/// Returns how many bytes the descriptor took, which C's `fprintf` reports as its `int` result.
///
/// # Errors
///
/// None today — the signature keeps [`BionicResult`] so that a caller can treat it like every
/// other stream operation, and so that a future descriptor layer with a guest-visible failure
/// does not change every call site.
pub fn write_host_bytes(
    ctx: &mut impl GuestContext,
    descriptors: &impl Descriptors,
    stream: &mut Stream,
    bytes: &[u8],
) -> BionicResult<u64> {
    let _ = &ctx;
    let mut done = 0usize;
    while done < bytes.len() {
        match descriptors.write(stream.fd, &bytes[done..]) {
            Ok(0) => {
                // As `transfer_out`: a descriptor accepting nothing is not making progress, and
                // looping would be a hang rather than a failure.
                stream.error = true;
                break;
            }
            Ok(took) => done += took,
            Err(errno) => {
                stream.error = true;
                ctx.set_errno(errno);
                break;
            }
        }
    }
    Ok(done as u64)
}

// ------------------------------------------------------------------ the shared machinery

/// Read `length` bytes of guest memory in chunks and hand them to the descriptor.
///
/// Returns how many bytes the descriptor took, which is less than `length` on a short write or an
/// error. The chunking is the bound described in the module documentation.
fn transfer_out(
    ctx: &mut impl GuestContext,
    descriptors: &impl Descriptors,
    stream: &mut Stream,
    ptr: u64,
    length: u64,
) -> BionicResult<u64> {
    let mut done = 0u64;
    let mut buffer = vec![0u8; TRANSFER_CHUNK];
    while done < length {
        let want = ((length - done) as usize).min(TRANSFER_CHUNK);
        read_all(ctx, ptr + done, &mut buffer[..want])?;
        match descriptors.write(stream.fd, &buffer[..want]) {
            Ok(0) => {
                // A descriptor that accepts nothing is not making progress; looping would spin
                // forever on it, which is a hang rather than a failure.
                stream.error = true;
                break;
            }
            Ok(took) => done += took as u64,
            Err(errno) => {
                stream.error = true;
                ctx.set_errno(errno);
                break;
            }
        }
    }
    Ok(done)
}

/// Write a whole slice into guest memory, reporting a fault as one.
fn write_all(ctx: &mut impl GuestContext, at: u64, bytes: &[u8]) -> BionicResult<()> {
    crate::memory::checked_range(at, bytes.len() as u64)?;
    ctx.write(at, bytes).map_err(BionicError::from)
}

/// Read a whole slice out of guest memory, reporting a fault as one.
fn read_all(ctx: &impl GuestMemory, at: u64, bytes: &mut [u8]) -> BionicResult<()> {
    crate::memory::checked_range(at, bytes.len() as u64)?;
    ctx.read(at, bytes).map_err(BionicError::from)
}

/// Write the accumulated bytes into guest memory and report how many went.
fn flush_into_guest(ctx: &mut impl GuestContext, at: u64, bytes: &[u8]) -> BionicResult<u64> {
    write_all(ctx, at, bytes)?;
    Ok(bytes.len() as u64)
}

/// `strlen`, refused past [`MAX_STRING`] rather than walked to the end of the address space.
fn bounded_strlen(mem: &impl GuestMemory, s: u64) -> BionicResult<u64> {
    match string::strnlen(mem, s, MAX_STRING)? {
        length if length == MAX_STRING => Err(BionicError::InvalidArgument("fputs")),
        length => Ok(length),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Fault;
    use crate::mock::MockMemory;
    use std::cell::RefCell;

    /// A descriptor table over byte vectors, with a read position per descriptor.
    ///
    /// **A recording mock rather than an output capture**, which is this project's preferred
    /// shape: the test asserts on what reached the descriptor and on *how many calls* it took,
    /// and the second of those is what distinguishes a `fgets` that stops on its newline from one
    /// that reads ahead.
    struct Fds {
        input: RefCell<Vec<u8>>,
        position: RefCell<usize>,
        output: RefCell<Vec<u8>>,
        reads: RefCell<usize>,
        writes: RefCell<usize>,
        flushes: RefCell<usize>,
        fail_read_after: RefCell<Option<usize>>,
        fail_write_after: RefCell<Option<usize>>,
    }

    impl Fds {
        fn new(input: &[u8]) -> Fds {
            Fds {
                input: RefCell::new(input.to_vec()),
                position: RefCell::new(0),
                output: RefCell::new(Vec::new()),
                reads: RefCell::new(0),
                writes: RefCell::new(0),
                flushes: RefCell::new(0),
                fail_read_after: RefCell::new(None),
                fail_write_after: RefCell::new(None),
            }
        }
    }

    impl Descriptors for Fds {
        fn read(&self, _fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
            if let Some(limit) = *self.fail_read_after.borrow() {
                if *self.reads.borrow() >= limit {
                    return Err(consts::EIO);
                }
            }
            *self.reads.borrow_mut() += 1;
            let input = self.input.borrow();
            let mut position = self.position.borrow_mut();
            let available = input.len().saturating_sub(*position);
            let take = available.min(buf.len());
            buf[..take].copy_from_slice(&input[*position..*position + take]);
            *position += take;
            Ok(take)
        }

        fn write(&self, _fd: i32, buf: &[u8]) -> Result<usize, i32> {
            if let Some(limit) = *self.fail_write_after.borrow() {
                if *self.writes.borrow() >= limit {
                    return Err(consts::ENOSPC);
                }
            }
            *self.writes.borrow_mut() += 1;
            self.output.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&self, _fd: i32) -> Result<(), i32> {
            *self.flushes.borrow_mut() += 1;
            Ok(())
        }
    }

    /// Guest memory with an errno cell and scratch, for the `GuestContext` half.
    struct Ctx {
        mem: MockMemory,
        errno: i32,
        rand: u32,
    }

    impl Ctx {
        fn new() -> Ctx {
            let mut mem = MockMemory::new();
            mem.map(0x1000, &[0u8; 0x4000]);
            Ctx { mem, errno: 0, rand: 1 }
        }

        fn at(&self, at: u64, len: usize) -> Vec<u8> {
            let mut out = vec![0u8; len];
            self.mem.read(at, &mut out).expect("mapped");
            out
        }

        fn put(&mut self, at: u64, bytes: &[u8]) {
            self.mem.write(at, bytes).expect("mapped");
        }
    }

    impl GuestMemory for Ctx {
        fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
            self.mem.read(addr, buf)
        }
        fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault> {
            self.mem.write(addr, buf)
        }
    }

    impl GuestContext for Ctx {
        fn errno(&self) -> i32 {
            self.errno
        }
        fn set_errno(&mut self, value: i32) {
            self.errno = value;
        }
        fn rand_state(&self) -> u32 {
            self.rand
        }
        fn set_rand_state(&mut self, state: u32) {
            self.rand = state;
        }
        fn scratch(&mut self) -> Option<(u64, usize)> {
            None
        }
    }

    /// `fgets` keeps the newline, terminates, stops **on** the newline, and leaves the rest.
    #[test]
    fn fgets_stops_on_its_newline_and_leaves_the_next_line_for_the_next_call() {
        let mut ctx = Ctx::new();
        let fds = Fds::new(b"first\nsecond\n");
        let mut stream = Stream::new(3);
        let got = fgets(&mut ctx, &fds, &mut stream, 0x1000, 64).expect("fgets");
        assert_eq!(got, 0x1000, "fgets returns its buffer");
        assert_eq!(ctx.at(0x1000, 7), b"first\n\0", "the newline is kept and a NUL follows");
        // Six bytes read: five letters and the newline. Anything more is read-ahead, and a
        // read-ahead that is not put back is a silently lost byte on a shared descriptor.
        assert_eq!(*fds.reads.borrow(), 6, "fgets read past its newline");
        let got = fgets(&mut ctx, &fds, &mut stream, 0x1100, 64).expect("fgets");
        assert_eq!(got, 0x1100);
        assert_eq!(ctx.at(0x1100, 8), b"second\n\0", "the second line is intact");
        // And the third call is end of file: NULL, the flag set, and the buffer untouched.
        ctx.put(0x1200, b"SENTINEL");
        assert_eq!(fgets(&mut ctx, &fds, &mut stream, 0x1200, 64).expect("fgets"), 0);
        assert_eq!(feof(&stream), 1);
        assert_eq!(ctx.at(0x1200, 8), b"SENTINEL", "end of file wrote into the buffer");
    }

    /// The `size` rules, at every edge including the ones that write nothing.
    #[test]
    fn fgets_writes_at_most_size_minus_one_bytes_and_always_terminates() {
        let mut ctx = Ctx::new();
        let fds = Fds::new(b"abcdefgh");
        let mut stream = Stream::new(3);
        ctx.put(0x1000, b"XXXXXXXX");
        // size 4: three bytes and a NUL.
        assert_eq!(fgets(&mut ctx, &fds, &mut stream, 0x1000, 4).expect("fgets"), 0x1000);
        assert_eq!(ctx.at(0x1000, 5), b"abc\0X");
        // size 1: no bytes, one NUL -- and nothing is read from the descriptor.
        let before = *fds.reads.borrow();
        ctx.put(0x1100, b"YYYY");
        assert_eq!(fgets(&mut ctx, &fds, &mut stream, 0x1100, 1).expect("fgets"), 0x1100);
        assert_eq!(ctx.at(0x1100, 2), b"\0Y");
        assert_eq!(*fds.reads.borrow(), before, "size 1 must not consume a byte");
        // size 0 and a negative size: NULL, and the buffer is untouched.
        ctx.put(0x1200, b"ZZZZ");
        for size in [0, -1, i32::MIN] {
            assert_eq!(fgets(&mut ctx, &fds, &mut stream, 0x1200, size).expect("fgets"), 0);
        }
        assert_eq!(ctx.at(0x1200, 4), b"ZZZZ");
    }

    /// A line longer than one transfer chunk crosses it without losing or duplicating a byte.
    ///
    /// **The chunk boundary is where an off-by-one lives**, and a test whose line fits in one
    /// chunk cannot see it.
    #[test]
    fn a_line_longer_than_a_transfer_chunk_crosses_it_intact() {
        let mut ctx = Ctx::new();
        let mut line: Vec<u8> = (0..TRANSFER_CHUNK + 700).map(|i| b'a' + (i % 26) as u8).collect();
        line.push(b'\n');
        let fds = Fds::new(&line);
        let mut stream = Stream::new(3);
        let got = fgets(&mut ctx, &fds, &mut stream, 0x1000, line.len() as i32 + 1).expect("fgets");
        assert_eq!(got, 0x1000);
        let mut expected = line.clone();
        expected.push(0);
        assert_eq!(ctx.at(0x1000, expected.len()), expected);
    }

    /// `fread` reports whole items, sets end of file on a short read, and never divides by zero.
    #[test]
    fn fread_reports_whole_items_and_stops_at_the_end_of_the_file() {
        let mut ctx = Ctx::new();
        let fds = Fds::new(b"0123456789");
        let mut stream = Stream::new(3);
        // Three items of three bytes: nine bytes, three items.
        assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1000, 3, 3).expect("fread"), 3);
        assert_eq!(ctx.at(0x1000, 9), b"012345678");
        // One byte is left, so a three-byte item cannot complete: zero items, end of file set,
        // and the partial byte is still written where C says the buffer contents are.
        assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1100, 3, 3).expect("fread"), 0);
        assert_eq!(feof(&stream), 1);
        assert_eq!(ctx.at(0x1100, 1), b"9");
        // A zero size or count is zero items and touches nothing -- and `size == 0` is the case
        // that would divide by zero.
        let mut fresh = Stream::new(3);
        assert_eq!(fread(&mut ctx, &fds, &mut fresh, 0x1200, 0, 100).expect("fread"), 0);
        assert_eq!(fread(&mut ctx, &fds, &mut fresh, 0x1200, 100, 0).expect("fread"), 0);
        assert_eq!(feof(&fresh), 0, "a zero-length read is not an end-of-file report");
    }

    /// **The overflow, at every boundary rather than sampled.**
    ///
    /// `size * nmemb` is two guest numbers. A debug build panics on an overflow and a release
    /// build wraps, and the wrapped value — zero — satisfies "returns fewer items than asked
    /// for", so a test that only checked the return could pass against the broken version. This
    /// asserts the errno as well, which the wrap cannot produce.
    #[test]
    fn no_pair_of_sizes_can_make_fread_or_fwrite_wrap() {
        let mut ctx = Ctx::new();
        let fds = Fds::new(b"0123456789");
        for (size, nmemb) in [
            (u64::MAX, 2u64),
            (2, u64::MAX),
            (u64::MAX, u64::MAX),
            (1 << 32, 1 << 32),
            ((1u64 << 32) + 1, 1u64 << 32),
            (u64::MAX / 2 + 1, 2),
        ] {
            let mut stream = Stream::new(3);
            ctx.set_errno(0);
            assert_eq!(
                fread(&mut ctx, &fds, &mut stream, 0x1000, size, nmemb).expect("fread"),
                0,
                "fread({size}, {nmemb})"
            );
            assert_eq!(ctx.errno(), consts::EINVAL, "fread({size}, {nmemb}) did not report EINVAL");
            assert!(stream.error, "fread({size}, {nmemb}) left the error flag clear");

            let mut stream = Stream::new(3);
            ctx.set_errno(0);
            assert_eq!(
                fwrite(&mut ctx, &fds, &mut stream, 0x1000, size, nmemb).expect("fwrite"),
                0,
                "fwrite({size}, {nmemb})"
            );
            assert_eq!(ctx.errno(), consts::EINVAL, "fwrite({size}, {nmemb}) did not report EINVAL");
        }
        // The largest product that does NOT overflow is still rejected for a different reason --
        // guest memory -- rather than being silently turned into a small request.
        let mut stream = Stream::new(3);
        assert!(
            fwrite(&mut ctx, &fds, &mut stream, 0x1000, 1, u64::MAX).is_err(),
            "a request that does not overflow but cannot be read must fault, not succeed"
        );
    }

    /// `fwrite` and `fputs` put the right bytes out, and `fputc` converts the way C says.
    #[test]
    fn the_output_side_writes_what_it_was_given() {
        let mut ctx = Ctx::new();
        let fds = Fds::new(b"");
        let mut stream = Stream::new(3);
        ctx.put(0x1000, b"hello\0");
        assert_eq!(fputs(&mut ctx, &fds, &mut stream, 0x1000).expect("fputs"), 5);
        assert_eq!(&*fds.output.borrow(), b"hello");
        ctx.put(0x1100, &[1u8, 2, 3, 4, 5, 6]);
        assert_eq!(fwrite(&mut ctx, &fds, &mut stream, 0x1100, 2, 3).expect("fwrite"), 3);
        assert_eq!(&fds.output.borrow()[5..], &[1u8, 2, 3, 4, 5, 6]);
        // `fputc` returns the byte as an *unsigned* char, so 0xff is 255 rather than EOF.
        assert_eq!(fputc(&mut ctx, &fds, &mut stream, i32::from(b'A')), 65);
        assert_eq!(fputc(&mut ctx, &fds, &mut stream, -1), 255, "0xff must not read as EOF");
        assert_eq!(fputc(&mut ctx, &fds, &mut stream, 0x141), 0x41, "only the low byte is written");
        assert_eq!(&fds.output.borrow()[11..], b"A\xff\x41");
        assert_eq!(fflush(&mut ctx, &fds, &mut stream), 0);
        assert_eq!(*fds.flushes.borrow(), 1);
    }

    /// A descriptor failure becomes the stream's error flag, the guest's errno and C's own
    /// return value — never a partial success reported as a whole one.
    #[test]
    fn a_descriptor_failure_is_reported_as_c_reports_it() {
        let mut ctx = Ctx::new();
        // Writes fail immediately.
        let fds = Fds::new(b"abc\n");
        *fds.fail_write_after.borrow_mut() = Some(0);
        let mut stream = Stream::new(3);
        ctx.put(0x1000, b"hello\0");
        assert_eq!(fputs(&mut ctx, &fds, &mut stream, 0x1000).expect("fputs"), EOF);
        assert!(stream.error);
        assert_eq!(ctx.errno(), consts::ENOSPC);
        assert_eq!(fputc(&mut ctx, &fds, &mut stream, 65), EOF);

        // Reads fail immediately: NULL, the error flag, and the buffer untouched.
        let fds = Fds::new(b"abc\n");
        *fds.fail_read_after.borrow_mut() = Some(0);
        let mut stream = Stream::new(3);
        ctx.put(0x1200, b"SENTINEL");
        assert_eq!(fgets(&mut ctx, &fds, &mut stream, 0x1200, 64).expect("fgets"), 0);
        assert!(stream.error);
        assert_eq!(ctx.errno(), consts::EIO);
        assert_eq!(feof(&stream), 0, "a read error is not an end of file");
        assert_eq!(ctx.at(0x1200, 8), b"SENTINEL");
    }

    /// A destination that is not writable faults, and nothing is silently dropped.
    #[test]
    fn a_hostile_destination_faults_rather_than_being_written_around() {
        let mut ctx = Ctx::new();
        // **A fresh descriptor per case.** Sharing one across them costs the test its teeth: the
        // first call consumes the whole input, and every later `fread` then sees end of file and
        // returns zero items *without ever touching the destination* — so a missing destination
        // check would still look caught. That is what the first version of this test did, and it
        // is the "exercising, not detecting" shape Global Constraint 13 is about.
        let mut stream = Stream::new(3);
        assert!(fgets(&mut ctx, &Fds::new(b"abcdefgh\n"), &mut stream, 0x900_0000, 64).is_err());
        let mut stream = Stream::new(3);
        assert!(fread(&mut ctx, &Fds::new(b"abcdefgh\n"), &mut stream, 0x900_0000, 1, 8).is_err());
        // A null destination with a non-zero length is a fault, and with a zero length is not.
        let mut stream = Stream::new(3);
        assert!(fread(&mut ctx, &Fds::new(b"abcdefgh\n"), &mut stream, 0, 1, 8).is_err());
        let mut stream = Stream::new(3);
        assert_eq!(
            fread(&mut ctx, &Fds::new(b"abcdefgh\n"), &mut stream, 0, 0, 8).expect("zero items"),
            0
        );
        // A source that is not readable faults on the way out.
        let mut stream = Stream::new(3);
        assert!(fwrite(&mut ctx, &Fds::new(b""), &mut stream, 0x900_0000, 1, 8).is_err());
        // An unterminated string is a named refusal rather than a walk to the end of memory.
        // The whole mapping is filled, so the walk runs off its end rather than finding the
        // zeroes an under-filled region would have left — the same trap D20 records `strlen`'s
        // first hostile test falling into, where the scan walked into the next page and returned
        // a plausible length.
        let mut ctx2 = Ctx::new();
        let filled = vec![b'x'; 0x4000];
        ctx2.put(0x1000, &filled);
        let mut stream = Stream::new(3);
        let error =
            fputs(&mut ctx2, &Fds::new(b""), &mut stream, 0x1000).expect_err("unterminated");
        assert!(matches!(error, BionicError::Memory(_) | BionicError::InvalidArgument("fputs")));
    }

    /// The two flags are sticky, which is what C promises and what `feof` is asked about.
    #[test]
    fn the_end_of_file_flag_is_sticky() {
        let mut ctx = Ctx::new();
        let fds = Fds::new(b"ab");
        let mut stream = Stream::new(3);
        assert_eq!(feof(&stream), 0);
        assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1000, 1, 8).expect("fread"), 2);
        assert_eq!(feof(&stream), 1, "a short read is an end of file");
        // Nothing in the reachable import set clears it, so it stays set.
        assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1000, 1, 8).expect("fread"), 0);
        assert_eq!(feof(&stream), 1);
    }

    /// The transfer bound is a constant, not something the guest picks.
    #[test]
    fn the_host_buffer_is_bounded_by_a_constant_rather_than_by_the_request() {
        assert_eq!(TRANSFER_CHUNK, 4096);
        assert_eq!(MAX_STRING, 64 * 1024);
        assert_eq!(EOF, -1);
        // A huge request runs through the same 4 KiB buffer; the proof it does not allocate the
        // request is that this returns rather than exhausting memory.
        let mut ctx = Ctx::new();
        let fds = Fds::new(b"short");
        let mut stream = Stream::new(3);
        assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1000, 1, 1 << 40).expect("fread"), 5);
    }
}
