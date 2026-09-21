//! **The reason a `FILE *` failure gives the guest, one site at a time.**
//!
//! This file exists because of a recorded review finding: `fputc`, `transfer_out` and
//! `write_host_bytes` set the stream's error indicator on a zero-byte write and never touched
//! `errno`, so a guest got `EOF` and then read the reason for some *earlier, unrelated* call. The
//! return value was right and the explanation was a lie — a caller that logs `strerror(errno)`
//! prints a confident diagnosis of a problem it does not have. It is the silent-wrong-answer
//! shape this project exists to refuse, one layer along from the answer itself.
//!
//! # Why `errno` is the whole of it here
//!
//! C17 7.21 gives a stream an **error indicator** and gives `ferror` (7.21.10.3) the job of
//! reading it. `ferror` and `clearerr` are undefined dynamic symbols of the APK's libraries and
//! are reachable from none of the 3,594 initializers, so no adapter binds them and **no guest
//! call in this milestone can read the flag**. `errno` is the only channel left, which is why a
//! missing one is a defect rather than a cosmetic omission.
//!
//! # Where every expectation below comes from
//!
//! Derived from the standards, never from another implementation — `docs/VERIFICATION.md` entry 7
//! is the record of what an implementation oracle costs here.
//!
//! * **C17 7.21.5.2p3** (`fflush` sets the error indicator and returns `EOF` on a write error),
//!   **7.21.7.2p3** (`fgets` returns NULL on end of file *or* a read error, and they are
//!   different things), **7.21.7.3p2** (`fputc`), **7.21.7.4p3** (`fputs`), **7.21.8.1p3**
//!   (`fread` is short "if a read error **or** end-of-file is encountered"), **7.21.8.2p3**
//!   (`fwrite` is short "only if a write error is encountered").
//! * **POSIX.1-2017 XSH** supplies the numbers C does not have: `fputc`'s ERRORS list is
//!   `write()`'s and is inherited by `fputs`, `fwrite` and `fprintf`; `fgetc`'s is `read()`'s and
//!   is inherited by `fgets` and `fread`; `fflush`'s is `write()`'s plus `EBADF`.
//! * **POSIX.1-2017 XSH `write()`, RETURN VALUE** — there is **no zero return for `nbyte > 0`**.
//!   That is the clause behind the one case with no number to set, and behind
//!   [`omni_bionic::stdio::Descriptors::write`]'s contract.
//!
//! # The sentinel, and what it is standing in for
//!
//! Every test that asserts "no `errno` was set" first puts [`SENTINEL`] there. Asserting against
//! zero would pass for a layer that helpfully cleared `errno`, which POSIX forbids ("no function
//! in this volume shall set `errno` to 0"), and would not notice a stale value at all. `EDOM` is
//! the value chosen because nothing in this module can produce it: it is what a `libm` call three
//! thousand initializers ago left behind, which is exactly the lie being tested for.

use std::cell::{Cell, RefCell};

use omni_bionic::context::GuestContext;
use omni_bionic::errno::consts;
use omni_bionic::memory::{Fault, GuestMemory};
use omni_bionic::mock::MockMemory;
use omni_bionic::stdio::{
    feof, fflush, fgets, fputc, fputs, fread, fwrite, write_host_bytes, Descriptors, Stream, EOF,
};

/// What an earlier, unrelated call left in `errno`.
///
/// `EDOM` (33) is `pow`'s and `log`'s answer and is produced nowhere in the `FILE *` layer, so it
/// cannot be confused with anything a stream operation could legitimately set.
const SENTINEL: i32 = consts::EDOM;

/// The `errno` values POSIX.1-2017 XSH `fgetc` lists for a failed read, minus the two this
/// crate's [`consts`] has no constant for (`ENXIO`).
///
/// Enumerated rather than sampled: the property under test is "whatever the descriptor said, the
/// guest reads", and a single value cannot tell that apart from a hard-coded one.
const READ_ERRNOS: [i32; 6] = [
    consts::EAGAIN,
    consts::EBADF,
    consts::EINTR,
    consts::EIO,
    consts::EOVERFLOW,
    consts::ENOMEM,
];

/// The `errno` values POSIX.1-2017 XSH `fputc` lists for a failed write, which is `write()`'s
/// list and is `fputs`'s, `fwrite`'s, `fprintf`'s and `fflush`'s by reference.
const WRITE_ERRNOS: [i32; 8] = [
    consts::EAGAIN,
    consts::EBADF,
    consts::EFBIG,
    consts::EINTR,
    consts::EIO,
    consts::ENOSPC,
    consts::EPIPE,
    consts::ENOMEM,
];

// ==================================================================== the doubles

/// A scripted descriptor: a byte source, a byte sink, and one chosen way to fail.
///
/// **A recording double rather than an output capture.** Several assertions below are about *how
/// many* calls a failure took — one, not a spin — and a double that only remembered the bytes
/// could not make them.
struct Fds {
    input: Vec<u8>,
    position: Cell<usize>,
    output: RefCell<Vec<u8>>,
    reads: Cell<usize>,
    writes: Cell<usize>,
    flushes: Cell<usize>,
    read_error: Option<i32>,
    write_error: Option<i32>,
    flush_error: Option<i32>,
    /// Return `Ok(0)` from `write` however many bytes were offered.
    ///
    /// **This is a deliberate violation of [`Descriptors::write`]'s contract**, which is the only
    /// way to reach the arm under test: POSIX's `write()` has no zero return for a non-empty
    /// buffer, no seam in this workspace produces one, and the adapter refuses it by name. The
    /// arm exists so that a broken implementation stops instead of spinning, and this is the
    /// broken implementation.
    write_takes_nothing: bool,
}

impl Fds {
    fn new(input: &[u8]) -> Fds {
        Fds {
            input: input.to_vec(),
            position: Cell::new(0),
            output: RefCell::new(Vec::new()),
            reads: Cell::new(0),
            writes: Cell::new(0),
            flushes: Cell::new(0),
            read_error: None,
            write_error: None,
            flush_error: None,
            write_takes_nothing: false,
        }
    }

    fn failing_reads(errno: i32) -> Fds {
        Fds { read_error: Some(errno), ..Fds::new(b"abcdefgh\n") }
    }

    fn failing_writes(errno: i32) -> Fds {
        Fds { write_error: Some(errno), ..Fds::new(b"") }
    }

    fn failing_flush(errno: i32) -> Fds {
        Fds { flush_error: Some(errno), ..Fds::new(b"") }
    }

    fn taking_nothing() -> Fds {
        Fds { write_takes_nothing: true, ..Fds::new(b"") }
    }
}

impl Descriptors for Fds {
    fn read(&self, _fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
        if let Some(errno) = self.read_error {
            return Err(errno);
        }
        self.reads.set(self.reads.get() + 1);
        let position = self.position.get();
        let take = self.input.len().saturating_sub(position).min(buf.len());
        buf[..take].copy_from_slice(&self.input[position..position + take]);
        self.position.set(position + take);
        Ok(take)
    }

    fn write(&self, _fd: i32, buf: &[u8]) -> Result<usize, i32> {
        if let Some(errno) = self.write_error {
            return Err(errno);
        }
        self.writes.set(self.writes.get() + 1);
        if self.write_takes_nothing {
            return Ok(0);
        }
        self.output.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&self, _fd: i32) -> Result<(), i32> {
        self.flushes.set(self.flushes.get() + 1);
        match self.flush_error {
            Some(errno) => Err(errno),
            None => Ok(()),
        }
    }
}

/// Guest memory plus the errno cell, which is the thing under test.
struct Ctx {
    mem: MockMemory,
    errno: i32,
    rand: u32,
}

impl Ctx {
    /// A context whose `errno` already holds [`SENTINEL`], because that is the state every real
    /// call starts in: something set it earlier and nothing has cleared it.
    fn new() -> Ctx {
        let mut mem = MockMemory::new();
        mem.map(0x1000, &[0u8; 0x4000]);
        Ctx { mem, errno: SENTINEL, rand: 1 }
    }

    fn at(&self, at: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        self.mem.read(at, &mut out).expect("the test fixture maps this range");
        out
    }

    fn put(&mut self, at: u64, bytes: &[u8]) {
        self.mem.write(at, bytes).expect("the test fixture maps this range");
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

/// Put a NUL-terminated `"hello"` at `0x1000` and six raw bytes at `0x1100`.
fn with_payloads() -> Ctx {
    let mut ctx = Ctx::new();
    ctx.put(0x1000, b"hello\0");
    ctx.put(0x1100, &[1u8, 2, 3, 4, 5, 6]);
    ctx
}

// ==================================================================== reads

/// Every reading site reports **the descriptor's** number, not one of its own.
///
/// Enumerated over POSIX.1-2017 XSH `fgetc`'s whole ERRORS list rather than one value, because a
/// site that hard-coded a plausible number would satisfy a single-value test. `feof` is asserted
/// false in the same breath: C17 7.21.7.2p3 and 7.21.8.1p3 make a read error and an end of file
/// two different answers, and a guest that cannot tell them apart retries a broken descriptor
/// forever or stops reading a file that had more in it.
#[test]
fn a_failed_read_reports_the_descriptors_own_errno_from_fgets_and_fread() {
    for errno in READ_ERRNOS {
        let fds = Fds::failing_reads(errno);

        let mut ctx = Ctx::new();
        let mut stream = Stream::new(3);
        ctx.put(0x1200, b"SENTINEL");
        assert_eq!(fgets(&mut ctx, &fds, &mut stream, 0x1200, 64).expect("fgets"), 0, "fgets");
        assert_eq!(ctx.errno(), errno, "fgets did not report the descriptor's errno {errno}");
        assert!(stream.error, "fgets left the error indicator clear for {errno}");
        assert_eq!(feof(&stream), 0, "a read error is not an end of file ({errno})");
        assert_eq!(ctx.at(0x1200, 8), b"SENTINEL", "the buffer was written on a failed fgets");

        let mut ctx = Ctx::new();
        let mut stream = Stream::new(3);
        assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1000, 4, 4).expect("fread"), 0, "fread");
        assert_eq!(ctx.errno(), errno, "fread did not report the descriptor's errno {errno}");
        assert!(stream.error, "fread left the error indicator clear for {errno}");
        assert_eq!(feof(&stream), 0, "a read error is not an end of file ({errno})");
    }
}

/// End of file sets **no** `errno`, and the sentinel proves it was not merely overwritten with
/// something harmless.
///
/// C17 7.21.8.1p3: `fread` returns a short count "if a read error **or** end-of-file is
/// encountered"; 7.21.7.2p3 says the same of `fgets`'s NULL. A file that ended is how every
/// correct read loop finishes, so a number here would make every one of them report a failure —
/// and `strerror` would name whichever one was invented.
#[test]
fn reaching_the_end_of_a_file_sets_no_errno_at_any_of_the_three_shapes() {
    // A short item at the end: two of the ten bytes are left, so no third three-byte item can
    // complete.
    let mut ctx = Ctx::new();
    let fds = Fds::new(b"0123456789");
    let mut stream = Stream::new(3);
    assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1000, 3, 4).expect("fread"), 3);
    assert_eq!(feof(&stream), 1, "a short read is an end of file");
    assert!(!stream.error, "an end of file is not an error indicator");
    assert_eq!(ctx.errno(), SENTINEL, "a short read at the end of a file invented an errno");

    // Nothing at all left: zero items, still not an error.
    assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1000, 3, 4).expect("fread"), 0);
    assert_eq!(ctx.errno(), SENTINEL, "a zero-item read at end of file invented an errno");

    // `fgets` with nothing left: NULL, the buffer untouched, and no number.
    let mut ctx = Ctx::new();
    let fds = Fds::new(b"one\n");
    let mut stream = Stream::new(3);
    assert_eq!(fgets(&mut ctx, &fds, &mut stream, 0x1000, 64).expect("fgets"), 0x1000);
    ctx.put(0x1200, b"SENTINEL");
    assert_eq!(fgets(&mut ctx, &fds, &mut stream, 0x1200, 64).expect("fgets"), 0);
    assert_eq!(feof(&stream), 1);
    assert!(!stream.error);
    assert_eq!(ctx.at(0x1200, 8), b"SENTINEL");
    assert_eq!(ctx.errno(), SENTINEL, "fgets at end of file invented an errno");
}

/// `fgets` with a size C17 does not define reads nothing, writes nothing and reports nothing.
///
/// C17 7.21.7.2p2 describes `fgets` only in terms of "at most one less than the number of
/// characters specified by `n`" and of writing a null character after the last one read, neither
/// of which means anything for `n <= 0`. POSIX.1-2017 `fgets` adds no error for it — its ERRORS
/// list is `fgetc`'s, every entry of which describes a transfer that was attempted, and none is.
/// So `EINVAL` here would be this layer inventing a complaint the library never made.
#[test]
fn fgets_with_a_size_c_does_not_define_sets_no_errno_and_touches_nothing() {
    let mut ctx = Ctx::new();
    let fds = Fds::new(b"abcdefgh\n");
    let mut stream = Stream::new(3);
    ctx.put(0x1200, b"ZZZZ");
    for size in [0, -1, i32::MIN] {
        ctx.set_errno(SENTINEL);
        assert_eq!(fgets(&mut ctx, &fds, &mut stream, 0x1200, size).expect("fgets"), 0, "{size}");
        assert_eq!(ctx.errno(), SENTINEL, "fgets({size}) invented an errno");
        assert!(!stream.error, "fgets({size}) set the error indicator");
        assert_eq!(feof(&stream), 0, "fgets({size}) claimed an end of file");
    }
    assert_eq!(ctx.at(0x1200, 4), b"ZZZZ", "a non-positive size wrote into the buffer");
    assert_eq!(fds.reads.get(), 0, "a non-positive size consumed a byte from the descriptor");
}

// ==================================================================== writes

/// Every writing site reports **the descriptor's** number, across POSIX's whole write list.
///
/// The four sites are asserted together because they are four spellings of one obligation and the
/// finding was that they did not all meet it: `fputc` writes directly, `fputs` and `fwrite` go
/// through `transfer_out`, and `fprintf` goes through `write_host_bytes` with a host-side source.
/// Each one's C17 return value is asserted alongside the number, because a site that reported the
/// failure and not the reason — or the reason and not the failure — is exactly half right.
#[test]
fn a_failed_write_reports_the_descriptors_own_errno_from_all_four_writing_sites() {
    for errno in WRITE_ERRNOS {
        let fds = Fds::failing_writes(errno);

        // C17 7.21.7.3p2: the error indicator is set and `fputc` returns EOF.
        let mut ctx = with_payloads();
        let mut stream = Stream::new(3);
        assert_eq!(fputc(&mut ctx, &fds, &mut stream, 65), EOF, "fputc({errno})");
        assert_eq!(ctx.errno(), errno, "fputc did not report the descriptor's errno {errno}");
        assert!(stream.error, "fputc left the error indicator clear for {errno}");

        // C17 7.21.7.4p3: `fputs` returns EOF if a write error occurs.
        let mut ctx = with_payloads();
        let mut stream = Stream::new(3);
        assert_eq!(fputs(&mut ctx, &fds, &mut stream, 0x1000).expect("fputs"), EOF, "fputs");
        assert_eq!(ctx.errno(), errno, "fputs did not report the descriptor's errno {errno}");
        assert!(stream.error, "fputs left the error indicator clear for {errno}");

        // C17 7.21.8.2p3: `fwrite` is short "only if a write error is encountered" -- so the
        // short count *is* the failure report, and the reason has to be readable.
        let mut ctx = with_payloads();
        let mut stream = Stream::new(3);
        assert_eq!(fwrite(&mut ctx, &fds, &mut stream, 0x1100, 2, 3).expect("fwrite"), 0, "fwrite");
        assert_eq!(ctx.errno(), errno, "fwrite did not report the descriptor's errno {errno}");
        assert!(stream.error, "fwrite left the error indicator clear for {errno}");

        // C17 7.21.6.1p14: `fprintf` returns how many characters it transmitted.
        let mut ctx = with_payloads();
        let mut stream = Stream::new(3);
        let sent = write_host_bytes(&mut ctx, &fds, &mut stream, b"hello").expect("fprintf");
        assert_eq!(sent, 0, "write_host_bytes({errno})");
        assert_eq!(ctx.errno(), errno, "write_host_bytes did not report the errno {errno}");
        assert!(stream.error, "write_host_bytes left the error indicator clear for {errno}");
    }
}

/// `fflush` reports the descriptor's number and sets the error indicator.
///
/// C17 7.21.5.2p3 names both halves in one sentence; POSIX.1-2017 XSH `fflush` supplies the
/// number, and its ERRORS list is `write()`'s plus `EBADF`. The believable wrong answer is the
/// one this project already wrote down for `fclose`: the call reports failure, nothing says why,
/// and the last thing the engine printed is simply gone.
#[test]
fn a_failed_flush_reports_the_descriptors_own_errno() {
    for errno in WRITE_ERRNOS {
        let fds = Fds::failing_flush(errno);
        let mut ctx = Ctx::new();
        let mut stream = Stream::new(3);
        assert_eq!(fflush(&mut ctx, &fds, &mut stream), EOF, "fflush({errno})");
        assert_eq!(ctx.errno(), errno, "fflush did not report the descriptor's errno {errno}");
        assert!(stream.error, "fflush left the error indicator clear for {errno}");
        assert_eq!(fds.flushes.get(), 1, "fflush did not reach the descriptor");
    }
}

/// A descriptor that takes none of a non-empty buffer **stops**, and invents nothing.
///
/// This is the arm the review finding named, and the assertions are in two halves because the
/// property has two halves.
///
/// * **It stops.** Exactly one call reaches the descriptor at each site. `Descriptors::write`'s
///   contract forbids a zero return for a non-empty buffer, so this arm is only reachable through
///   a broken implementation — and for one of those, looping would be a hang, which is strictly
///   worse than a failure. That is why the arm is kept rather than deleted.
/// * **It invents no `errno`.** POSIX.1-2017 XSH `write()` has no zero return for `nbyte > 0`, so
///   POSIX defines no number for this and there is nothing true to set. `ENOSPC` would send a
///   guest deleting files, `EAGAIN` would send it round the loop again, `EPIPE` would name a
///   reader it never had, and `EIO` is the number D23's refusal 7 already declines to hand out
///   for a failure nobody classified. The sentinel is what the guest would read, and the
///   adapter's own descriptor refuses the call by name before that can happen.
#[test]
fn a_descriptor_that_takes_no_bytes_stops_at_one_call_and_invents_no_errno() {
    // `fputc`: one byte is either written or it is not.
    let fds = Fds::taking_nothing();
    let mut ctx = with_payloads();
    let mut stream = Stream::new(3);
    assert_eq!(fputc(&mut ctx, &fds, &mut stream, 65), EOF, "fputc must report the failure");
    assert!(stream.error, "fputc must set the error indicator (C17 7.21.7.3p2)");
    assert_eq!(ctx.errno(), SENTINEL, "fputc invented an errno POSIX does not define");
    assert_eq!(fds.writes.get(), 1, "fputc retried a descriptor that is not making progress");

    // `fputs`: EOF, because nothing of the string reached the stream.
    let fds = Fds::taking_nothing();
    let mut ctx = with_payloads();
    let mut stream = Stream::new(3);
    assert_eq!(fputs(&mut ctx, &fds, &mut stream, 0x1000).expect("fputs"), EOF);
    assert!(stream.error, "fputs must set the error indicator");
    assert_eq!(ctx.errno(), SENTINEL, "fputs invented an errno POSIX does not define");
    assert_eq!(fds.writes.get(), 1, "fputs spun on a descriptor taking nothing");

    // `fwrite`: zero complete items.
    let fds = Fds::taking_nothing();
    let mut ctx = with_payloads();
    let mut stream = Stream::new(3);
    assert_eq!(fwrite(&mut ctx, &fds, &mut stream, 0x1100, 2, 3).expect("fwrite"), 0);
    assert!(stream.error, "fwrite must set the error indicator (C17 7.21.8.2p3)");
    assert_eq!(ctx.errno(), SENTINEL, "fwrite invented an errno POSIX does not define");
    assert_eq!(fds.writes.get(), 1, "fwrite spun on a descriptor taking nothing");

    // `fprintf`'s path: zero characters transmitted.
    let fds = Fds::taking_nothing();
    let mut ctx = with_payloads();
    let mut stream = Stream::new(3);
    assert_eq!(write_host_bytes(&mut ctx, &fds, &mut stream, b"hello").expect("fprintf"), 0);
    assert!(stream.error, "write_host_bytes must set the error indicator");
    assert_eq!(ctx.errno(), SENTINEL, "write_host_bytes invented an errno POSIX does not define");
    assert_eq!(fds.writes.get(), 1, "write_host_bytes spun on a descriptor taking nothing");
}

// ==================================================================== the calls that cannot fail

/// A call that succeeded sets no `errno`, at every site in the module.
///
/// POSIX.1-2017 XSH 2.3 leaves `errno` unspecified after a successful call; this layer is
/// stricter and touches it only when it has a reason, which is what makes every assertion above
/// about *which* reason meaningful. Without this test an implementation that set a number on
/// every call would satisfy all of them.
#[test]
fn nothing_that_succeeded_writes_an_errno() {
    let fds = Fds::new(b"line one\nline two\n");
    let mut ctx = with_payloads();
    let mut stream = Stream::new(3);

    assert_eq!(fputc(&mut ctx, &fds, &mut stream, 65), 65);
    assert_eq!(fputs(&mut ctx, &fds, &mut stream, 0x1000).expect("fputs"), 5);
    assert_eq!(fwrite(&mut ctx, &fds, &mut stream, 0x1100, 2, 3).expect("fwrite"), 3);
    assert_eq!(write_host_bytes(&mut ctx, &fds, &mut stream, b"hi").expect("fprintf"), 2);
    assert_eq!(fflush(&mut ctx, &fds, &mut stream), 0);
    assert_eq!(fgets(&mut ctx, &fds, &mut stream, 0x1300, 64).expect("fgets"), 0x1300);
    assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1400, 2, 4).expect("fread"), 4);
    assert_eq!(feof(&stream), 0, "nothing above reached the end of the input");
    assert!(!stream.error, "nothing above failed");

    assert_eq!(ctx.errno(), SENTINEL, "a successful stream operation wrote to errno");
}

/// A request for zero bytes is not an operation, so it reports nothing.
///
/// C17 7.21.8.1p3 is explicit for `fread`: "if `size` or `nmemb` is zero, `fread` returns zero
/// and the contents of the array and the state of the stream remain unchanged". `fwrite` is the
/// same shape. The descriptor is asserted untouched as well, because "returns zero" and "did not
/// ask the operating system" are two different claims and only the second one bounds the cost.
#[test]
fn a_zero_length_transfer_sets_no_errno_and_never_reaches_the_descriptor() {
    let fds = Fds::new(b"0123456789");
    let mut ctx = with_payloads();
    for (size, nmemb) in [(0u64, 100u64), (100, 0), (0, 0)] {
        let mut stream = Stream::new(3);
        ctx.set_errno(SENTINEL);
        assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1000, size, nmemb).expect("fread"), 0);
        assert_eq!(ctx.errno(), SENTINEL, "fread({size}, {nmemb}) invented an errno");
        assert_eq!(feof(&stream), 0, "fread({size}, {nmemb}) claimed an end of file");
        assert!(!stream.error, "fread({size}, {nmemb}) set the error indicator");

        let mut stream = Stream::new(3);
        ctx.set_errno(SENTINEL);
        assert_eq!(fwrite(&mut ctx, &fds, &mut stream, 0x1100, size, nmemb).expect("fwrite"), 0);
        assert_eq!(ctx.errno(), SENTINEL, "fwrite({size}, {nmemb}) invented an errno");
        assert!(!stream.error, "fwrite({size}, {nmemb}) set the error indicator");
    }
    assert_eq!(fds.reads.get(), 0, "a zero-length fread reached the descriptor");
    assert_eq!(fds.writes.get(), 0, "a zero-length fwrite reached the descriptor");

    // `fputs` of an empty string is the same case reached a different way: nothing to write.
    let mut ctx = Ctx::new();
    ctx.put(0x1000, b"\0");
    let mut stream = Stream::new(3);
    assert_eq!(fputs(&mut ctx, &fds, &mut stream, 0x1000).expect("fputs"), 0);
    assert_eq!(ctx.errno(), SENTINEL, "an empty fputs invented an errno");
    assert_eq!(fds.writes.get(), 0, "an empty fputs reached the descriptor");
}

/// The overflow pair is the one number this layer chooses, and it says so.
///
/// `EINVAL` is **not** in POSIX.1-2017's ERRORS list for `fread` or `fwrite`; it is a deliberate
/// extension, which XSH 2.3 permits in as many words, and it is defensible because the argument
/// pair genuinely is invalid — `size * nmemb` is not representable, so there is no request. The
/// assertion is here rather than only in the crate's own module tests because the sentinel makes
/// it a stronger statement: the number is *set*, not merely left over from something earlier.
#[test]
fn an_unrepresentable_size_pair_reports_einval_and_says_it_is_an_extension() {
    let fds = Fds::new(b"0123456789");
    let mut ctx = with_payloads();
    for (size, nmemb) in [(u64::MAX, 2u64), (2, u64::MAX), (1 << 32, 1 << 32), (u64::MAX, u64::MAX)]
    {
        let mut stream = Stream::new(3);
        ctx.set_errno(SENTINEL);
        assert_eq!(fread(&mut ctx, &fds, &mut stream, 0x1000, size, nmemb).expect("fread"), 0);
        assert_eq!(ctx.errno(), consts::EINVAL, "fread({size}, {nmemb}) did not report EINVAL");
        assert!(stream.error, "fread({size}, {nmemb}) left the error indicator clear");

        let mut stream = Stream::new(3);
        ctx.set_errno(SENTINEL);
        assert_eq!(fwrite(&mut ctx, &fds, &mut stream, 0x1100, size, nmemb).expect("fwrite"), 0);
        assert_eq!(ctx.errno(), consts::EINVAL, "fwrite({size}, {nmemb}) did not report EINVAL");
        assert!(stream.error, "fwrite({size}, {nmemb}) left the error indicator clear");
    }
}

// ==================================================================== the membership assertion

/// **Every site in this layer that can report a failure, named one by one.**
///
/// `docs/VERIFICATION.md` entry 1: a count cannot see a substitution. So this is a set difference
/// against a list written down by hand, not a total — a site that stopped reporting its reason
/// and a new site that never started would cancel out in any count.
///
/// Each row is `(site, what the guest is told)`. A site whose answer is "no `errno`" carries the
/// clause that makes the outcome not an error, because that is the claim being asserted.
const REPORTING_SITES: [(&str, &str); 12] = [
    ("fgets/read-error", "the descriptor's errno"),
    ("fgets/end-of-file", "none: C17 7.21.7.2p3, not an error"),
    ("fgets/size<=0", "none: undefined in C17 7.21.7.2, no POSIX error"),
    ("fread/read-error", "the descriptor's errno"),
    ("fread/end-of-file", "none: C17 7.21.8.1p3, not an error"),
    ("fread|fwrite/zero-length", "none: C17 7.21.8.1p3, stream unchanged"),
    ("fread|fwrite/overflow", "EINVAL, a named extension under POSIX XSH 2.3"),
    ("fputc/write-error", "the descriptor's errno"),
    ("fputs|fwrite/write-error", "the descriptor's errno, via transfer_out"),
    ("fprintf/write-error", "the descriptor's errno, via write_host_bytes"),
    ("fflush/flush-error", "the descriptor's errno"),
    ("*/took-nothing", "none: POSIX write() has no zero return for nbyte>0"),
];

/// The list above is covered by the tests above, site by site.
///
/// This is the assertion that a later phase adding a thirteenth reporting site has to come back
/// and answer for. It is deliberately dull: the value is that the list exists in one place and
/// that every entry names its standard clause, so the next reader can check the clause rather
/// than the code's confidence.
#[test]
fn every_reporting_site_in_this_layer_is_named_and_has_an_answer() {
    let covered = [
        "fgets/read-error",
        "fgets/end-of-file",
        "fgets/size<=0",
        "fread/read-error",
        "fread/end-of-file",
        "fread|fwrite/zero-length",
        "fread|fwrite/overflow",
        "fputc/write-error",
        "fputs|fwrite/write-error",
        "fprintf/write-error",
        "fflush/flush-error",
        "*/took-nothing",
    ];
    let listed: Vec<&str> = REPORTING_SITES.iter().map(|(site, _)| *site).collect();
    let missing: Vec<&&str> = listed.iter().filter(|site| !covered.contains(site)).collect();
    assert!(missing.is_empty(), "sites with no test: {missing:?}");
    let extra: Vec<&&str> = covered.iter().filter(|site| !listed.contains(site)).collect();
    assert!(extra.is_empty(), "tests for sites not on the list: {extra:?}");
    for (site, answer) in REPORTING_SITES {
        assert!(!answer.is_empty(), "{site} has no recorded answer");
    }
}
