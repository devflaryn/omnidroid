//! Bionic's `FILE *` layer: `fgets`, `fputs`, `fputc`, `fread`, `fwrite`, `feof`, `fflush`, and
//! the `fopen`/`fdopen` mode string.
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
//! * `fflush` has nothing of **ours** to flush — and that clause is where a reader has already
//!   gone wrong once, so it is spelled out in full below.
//!
//! The cost is a `read(2)` per byte in `fgets`. Correct rather than fast, and nothing in the
//! 3,594 initializers reads a line in a hot loop.
//!
//! # "Nothing of ours to flush" is not "nothing to flush", and `fclose` must still flush
//!
//! [`Descriptors::flush`] exists, is called by [`fflush`], and is **not** a formality. This layer
//! holds no bytes, but the layer under it may: the adapter's standard streams are the host's own
//! `Stdout` and `Stderr`, which buffer, and its `flush` reaches them for real.
//!
//! C is explicit about what that costs anyone who forgets it:
//!
//! * **C17 7.21.5.1p2 (`fclose`)** — "causes the stream pointed to by `stream` to be flushed and
//!   the associated file to be closed. Any unwritten buffered data for the stream are delivered
//!   to the host environment to be written to the file."
//! * **C17 7.22.4.4p2 (`exit`)** — "all open streams with unwritten buffered data are flushed,
//!   all open streams are closed".
//!
//! So a `fclose` that closes without flushing first **silently drops whatever the layer below was
//! holding**, and on a standard stream that is the last thing the engine said before it stopped.
//! The believable wrong answer is the one this project already wrote down: `fclose` returns `0`,
//! every assertion about return values passes, and the output is simply not there.
//!
//! **This crate cannot fix that, and the reason is structural rather than an omission.**
//! [`Descriptors`] has `read`, `write` and `flush` and deliberately has **no `close`**: closing a
//! descriptor is an operating-system call, this crate has no OS access (D19), and there is
//! therefore no `omni_bionic::stdio::fclose` for the flush to live inside. Adding a `close` to the
//! trait to create one would be worse than the defect — it would be a fourth method whose only
//! implementation is in the adapter, for the sake of calling the third one first.
//!
//! The obligation is therefore stated here and discharged by the caller: **an adapter's `fclose`
//! must call [`Descriptors::flush`] (or `fflush`) on the stream before it closes the descriptor,
//! and must close it whatever the flush reported**, because C17 7.21.5.1p2 also says the stream is
//! no longer usable after `fclose` whether or not it succeeded.
//!
//! # The `fopen` mode string is parsed here, whole
//!
//! [`parse_mode`] is pure byte arithmetic over a guest-supplied string, which is this module's
//! own definition of what belongs on this side of the trait, and it is written up under that
//! function rather than here. The one fact worth stating at the top: **it is given the whole mode
//! string and it never shortens one.** A parse that silently drops the tail of a long mode can
//! drop a `+` with it and hand back a read-only stream to a caller that asked for a read-write
//! one, which is a wrong answer with nothing anywhere reporting it.
//!
//! # Every host allocation here is bounded by a constant, not by a guest argument
//!
//! `fread(p, 1, SIZE_MAX, f)` must not become a `SIZE_MAX` allocation. Transfers move through a
//! [`TRANSFER_CHUNK`]-byte buffer however large the request is, the same shape
//! `arc4random_buf` already uses — so the peak host allocation for any call in this module is
//! [`TRANSFER_CHUNK`] bytes and the guest cannot choose it.
//!
//! # Every failure the guest can see carries the `errno` that explains it
//!
//! C itself has no `errno` for stdio: C17 7.21 speaks only of the **error indicator** and of
//! `EOF`. POSIX is where the number comes from, and it gives one to every function here that can
//! fail. A caller that prints `strerror(errno)` after an `EOF` is reading that number, so **a
//! failure reported without setting it hands the caller the reason for some earlier, unrelated
//! call**. The return value is right and the explanation is a lie — the same silent-wrong-answer
//! shape this seam already refuses one layer down (D23's `pread` cursor), arriving as the
//! *reason* for a failure rather than as the failure itself.
//!
//! Every site in this module that a guest can observe therefore does exactly one of two things,
//! and says at the site which one and why:
//!
//! 1. **Sets the `errno` POSIX specifies**, which is always the one the descriptor classified.
//!    POSIX.1-2017 XSH defines the whole ERRORS list of `fputc` (and by reference `fputs` and
//!    `fwrite`) as the errors of `write()`, of `fgetc` (and by reference `fgets` and `fread`) as
//!    the errors of `read()`, and of `fflush` as the errors of `write()`. So this layer never
//!    *chooses* a number: it passes on the one [`Descriptors`] gave it.
//! 2. **Sets none, naming the clause that makes the outcome not an error at all.** End of file is
//!    the whole of this case. C17 7.21.8.1p3 has `fread` return a short count "if a read error
//!    **or end-of-file** is encountered", and `feof` is how a caller tells the two apart; an
//!    `errno` here would be an invented explanation for a normal outcome.
//!
//! There is deliberately no third case. The one state that would have needed one — a descriptor
//! that takes none of a non-empty buffer and reports no error, for which neither C nor POSIX
//! defines an `errno` — is excluded by [`Descriptors::write`]'s contract instead of being given
//! a plausible number. See that method: an `errno` nobody can justify is worse than none, because
//! it is a *specific, believable, wrong* reason.
//!
//! # The error indicator, and what it is for **here**
//!
//! C17 7.21.7.3p2 sets the error indicator on a write error, 7.21.5.2p3 on a failed `fflush`, and
//! C17 7.21.10.3 has `ferror` report it. [`Stream::error`] is that indicator and it is maintained
//! exactly as C says — but **nothing in this runtime can read it**: `ferror` and `clearerr` are
//! imported by the APK's libraries and are neither among the 188 statically reachable imports nor
//! bound by any adapter, so no guest call can observe the flag today. Until one does, `errno` is
//! the *whole* of what a guest learns about why a stream operation failed, which is precisely why
//! a missing one is a defect rather than a cosmetic omission.

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
    /// **`Ok(0)` is not a failure and must not carry an `errno`.** That is `read(2)`'s own
    /// meaning of a zero return, and C17 7.21.8.1p3 keeps the distinction alive all the way up:
    /// `fread` returns a short count "if a read error **or** end-of-file is encountered", and
    /// `feof` is what separates them. A descriptor that reported end of file as an error would
    /// make every caller that loops to the end of a file report a failure that did not happen.
    ///
    /// # Errors
    ///
    /// The guest `errno` to report. POSIX.1-2017 XSH `fgetc` defines the ERRORS list of every
    /// reading function in this module as the errors of `read()`, so the number chosen here is
    /// the number the guest reads out of `errno`.
    fn read(&self, fd: i32, buf: &mut [u8]) -> Result<usize, i32>;

    /// Write from `buf`, returning how many bytes were taken.
    ///
    /// # `Ok(0)` is forbidden for a non-empty `buf`, and that is a contract rather than a hope
    ///
    /// POSIX.1-2017 XSH `write()` has **no zero return for `nbyte > 0`**: it transfers at least
    /// one byte, or it fails and sets `errno`. Zero is defined only for `nbyte == 0`. So there is
    /// no `errno` anywhere in C or POSIX meaning "the descriptor took none of the bytes and did
    /// not say why" — the state does not exist on a device, so nothing had to name it.
    ///
    /// This layer will not invent one. `ENOSPC` would tell a guest to free space, `EAGAIN` to
    /// retry, `EPIPE` that its reader is gone, and `EIO` that hardware failed; all four are
    /// specific, believable and unfounded, and D23's refusal 7 already records why an
    /// unclassified failure is refused by name rather than given `EIO`.
    ///
    /// **An implementation that cannot take a byte must say why with an `errno` of its own.** The
    /// adapter enforces exactly that on its seam — `std::io::Write` is the only half of it whose
    /// own contract permits a zero return — and turns a violation into a refusal naming the
    /// symbol, so the guest gets a loud failure instead of `EOF` plus whatever `errno` happened
    /// to be left over.
    ///
    /// The three call sites in this module still **stop** on a zero return rather than trusting
    /// the contract, because this crate is generic over this trait and looping would be a hang
    /// rather than a failure. Each says at the site what it does about `errno` and why.
    ///
    /// # Errors
    ///
    /// The guest `errno` to report. POSIX.1-2017 XSH `fputc` defines the ERRORS list of every
    /// writing function in this module as the errors of `write()`, so the number chosen here is
    /// the number the guest reads out of `errno`.
    fn write(&self, fd: i32, buf: &[u8]) -> Result<usize, i32>;

    /// Push anything the layer below is holding at the operating system.
    ///
    /// # Errors
    ///
    /// The guest `errno` to report. POSIX.1-2017 XSH `fflush` defines its ERRORS list as the
    /// errors of `write()` plus `EBADF`, and C17 7.21.5.2p3 sets the stream's error indicator
    /// alongside it.
    fn flush(&self, fd: i32) -> Result<(), i32>;
}

/// One open stream: the descriptor behind a guest `FILE *`, and its two sticky flags.
///
/// The flags are sticky, which is C's rule and not an implementation detail: `feof` stays true
/// until something clears it, and the only things that can are `clearerr`, `fseek` and `rewind` —
/// **none of which is in the reachable import set**. So within this milestone a stream that has
/// reached its end reports it for the rest of its life, which is exactly what C promises.
///
/// "Not in the reachable import set" is narrower than "not imported", and the difference matters
/// enough to state: `clearerr` and `fseek` **are** undefined dynamic symbols of the APK's
/// libraries, they are simply not reachable from the 3,594 initializers and no adapter binds
/// them. A phase that binds one inherits the obligation D23 narrowed around `sizeof(FILE)`, and
/// it is the same phase that would make [`Stream::error`] observable through `ferror`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stream {
    /// The descriptor this stream reads and writes.
    pub fd: i32,
    /// Set when a read found end of file.
    ///
    /// C17 7.21.10.2 is what reads it (`feof`). It is **not** an error: see
    /// [`Descriptors::read`].
    pub eof: bool,
    /// Set when an operation failed — C's *error indicator*.
    ///
    /// Maintained where C17 says: 7.21.7.3p2 on a write error, 7.21.5.2p3 on a failed `fflush`,
    /// and 7.21.7.2p3's read error for `fgets`.
    ///
    /// **Nothing in this runtime can read it.** C17 7.21.10.3's `ferror` and 7.21.10.1's
    /// `clearerr` are imported by the APK's libraries but are neither statically reachable from
    /// the initializers nor bound by any adapter, so the flag is bookkeeping for the phase that
    /// binds them. Until then `errno` is the whole of what a guest learns about *why* an
    /// operation failed, which is why a site that sets this flag and no `errno` is a defect and
    /// not a rounding error — see the module documentation.
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
/// # What a failure tells the guest, and where the number comes from
///
/// **C17 7.21.5.2p3**: "the `fflush` function sets the error indicator for the stream and returns
/// `EOF` if a write error occurs, otherwise it returns zero." Both halves are here.
///
/// **POSIX.1-2017 XSH `fflush`** supplies the `errno`, and its ERRORS list is the errors of
/// `write()` plus `EBADF` — every one of them a condition the descriptor below is the only thing
/// that can recognise. So the number is [`Descriptors::flush`]'s, passed on unchanged rather than
/// chosen here; the believable wrong answer would be picking a *likely* one (`EIO`) and telling a
/// guest whose `fflush` failed on a closed descriptor that its hardware broke.
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
/// # The three ways this returns `NULL`, and which of them sets `errno`
///
/// A caller cannot tell them apart from the return value alone, which is why C gives it `feof`
/// and `ferror` and POSIX gives it `errno`. All three are answered here:
///
/// | why | flags | `errno` | source |
/// |---|---|---|---|
/// | a read failed | error indicator set | the descriptor's | C17 7.21.7.2p3; POSIX.1-2017 `fgets` → `fgetc` |
/// | end of file, nothing read | end-of-file set | **untouched** | C17 7.21.7.2p3 — not an error |
/// | `size <= 0` | neither | **untouched** | undefined in C17 7.21.7.2; see below |
///
/// `size <= 0` is the one that deserves spelling out. C17 7.21.7.2p2 defines `fgets` only in
/// terms of "at most one less than the number of characters specified by `n`" and of writing a
/// null character after the last one read, neither of which means anything for `n <= 0`, so the
/// call is undefined. **POSIX.1-2017 `fgets` specifies no `errno` for it either** — its ERRORS
/// list is `fgetc`'s, and every entry there describes a transfer that was attempted and failed,
/// where this attempts none. So nothing is read, nothing is written, and no `errno` is set:
/// `EINVAL` would be this layer inventing a diagnosis for a call C declined to define, and a
/// caller that logged it would be told its *argument* was rejected by a library that in fact
/// made no complaint at all.
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
        // NULL, the buffer untouched, and **no errno**: C17 7.21.7.2 does not define the call and
        // POSIX names no error for it, so there is nothing true to report. See the table above
        // for why an invented `EINVAL` would be worse than the silence.
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
            // End of file. The flag is set and **no `errno` is**: C17 7.21.7.2p3 makes this a
            // return value rather than a failure, and `feof` is how a caller tells it from the
            // error arm below. A number here would describe a file that simply ended.
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
                // C17 7.21.7.2p3: a read error sets the error indicator and returns NULL, and
                // the buffer contents are indeterminate. Nothing partial is committed, so
                // "indeterminate" here means "unchanged", which is the stronger and safer of the
                // two readings.
                //
                // The `errno` is the descriptor's, not one chosen here: POSIX.1-2017 `fgets`
                // defines its ERRORS list as `fgetc`'s, which is `read()`'s, and the descriptor
                // is the only layer that can tell `EBADF` from `EIO` from `EAGAIN`. Without this
                // line the guest would read whatever the last unrelated call left behind, which
                // is the finding this module's `errno` rule exists to close.
                stream.error = true;
                ctx.set_errno(errno);
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
        //
        // No `errno`, deliberately (C17 7.21.7.2p3): this NULL and the read-error NULL are
        // different answers, and `feof` is the call that separates them. Giving this one a number
        // would make every `while (fgets(...))` loop end by reporting a failure.
        return Ok(0);
    }
    write_all(ctx, s + written, &[0u8])?;
    Ok(s)
}

/// `int fputs(const char *s, FILE *stream)`
///
/// Returns a non-negative value on success and `EOF` on failure. C fixes only the sign (C17
/// 7.21.7.4p3: "returns `EOF` if a write error occurs; otherwise it returns a nonnegative value"),
/// and this returns the number of bytes written, which is what bionic does.
///
/// # The `EOF` carries the descriptor's `errno`, except in the one state POSIX does not define
///
/// POSIX.1-2017 XSH `fputs` defines its ERRORS list as `fputc`'s, which is `write()`'s, so a
/// short write that came from a failing descriptor reports that descriptor's number — set in
/// `transfer_out`, which is where the write happens.
///
/// The exception is a descriptor that takes **none** of the bytes and reports no error. That is
/// forbidden by [`Descriptors::write`]'s contract because POSIX gives `write()` no zero return
/// for a non-empty buffer and therefore gives the state no `errno`; `transfer_out` stops on it
/// so this cannot hang, and does not invent a number. See both.
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
///
/// # What an `EOF` from here tells the guest
///
/// **C17 7.21.7.3p2**: "if a write error occurs, the error indicator for the stream is set and
/// `fputc` returns `EOF`." **POSIX.1-2017 XSH `fputc`** adds the `errno`, and its ERRORS list —
/// `EAGAIN`, `EBADF`, `EFBIG`, `EINTR`, `EIO`, `ENOSPC`, `EPIPE`, and `ENOMEM`/`ENXIO` as "may
/// fail" — is `write()`'s. Every entry is a condition only the descriptor can recognise, so the
/// failing arm below reports the descriptor's number and chooses none of its own.
///
/// The second arm is the state POSIX does not define, and it is the one this function cannot
/// report honestly: see [`Descriptors::write`] for why it has no `errno` and why the adapter,
/// which *can* refuse by name, is where it is stopped.
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
            // byte is either written or it is not. The error indicator is set, per C17
            // 7.21.7.3p2, and `EOF` is returned.
            //
            // **No `errno`, and that is the decision rather than an omission.** POSIX.1-2017 XSH
            // `write()` has no zero return for `nbyte > 0`, so no POSIX errno describes this and
            // there is nothing true to set. The four numbers that would look right here --
            // `ENOSPC`, `EAGAIN`, `EPIPE`, `EIO` -- each name a cause nobody observed, and a
            // caller logging `strerror(errno)` would print a confident diagnosis of the wrong
            // problem. `Descriptors::write`'s contract forbids the state instead, and the adapter
            // enforces it with a refusal that names the symbol, so no guest reaches this arm
            // through the shipping descriptor; it survives only to stop the call rather than
            // spin, for an implementation that has broken the contract.
            stream.error = true;
            EOF
        }
        Err(errno) => {
            // The descriptor's number, unchanged: POSIX.1-2017 XSH `fputc`'s ERRORS list is
            // `write()`'s, and only the layer that made the call can tell those apart. Dropping
            // this line is the defect this module's `errno` rule exists to close -- the guest
            // would still see `EOF`, and would read the reason for some earlier call.
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
/// **Where that `EINVAL` comes from, since POSIX does not list it.** POSIX.1-2017 XSH `fread`
/// defines its ERRORS list as `fgetc`'s, and `EINVAL` is not in it — this is a *deliberate
/// extension*, which XSH 2.3 permits in as many words ("implementations may generate errors
/// included in this list under circumstances other than those described here"), and it is named
/// as an extension rather than presented as the standard's answer. It is defensible because the
/// argument pair genuinely is invalid: `size * nmemb` is not representable, so there is no
/// request to carry out. On a device C's unsigned arithmetic would wrap and `fread` would read
/// the wrapped count, which is the wrong answer the wrap produces here too.
///
/// # The rest of the return values, and which of them set `errno`
///
/// | outcome | flags | `errno` | source |
/// |---|---|---|---|
/// | short at end of file | end-of-file set | **untouched** | C17 7.21.8.1p3 — not an error |
/// | short on a read failure | error indicator set | the descriptor's | C17 7.21.8.1p3; POSIX `fread` → `fgetc` |
/// | `size` or `nmemb` zero | neither | **untouched** | C17 7.21.8.1p3 — "the state of the stream remain\[s\] unchanged" |
/// | `size * nmemb` overflows | error indicator set | `EINVAL` (extension, above) | XSH 2.3 |
///
/// The first row is the one worth stating rather than assuming: a file that ended is the normal
/// way a read loop finishes, and an `errno` there would make every correct caller report a
/// failure at the end of every file it read.
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
            // End of file: the flag, and **no `errno`**. C17 7.21.8.1p3 makes a short count at
            // the end of a file a return value rather than a failure, and `feof` is what tells
            // it from the error arm below. See the table on this function.
            Ok(0) => {
                stream.eof = true;
                break;
            }
            Ok(got) => {
                write_all(ctx, ptr + done, &buffer[..got])?;
                done += got as u64;
            }
            Err(errno) => {
                // The descriptor's number, unchanged: POSIX.1-2017 XSH `fread`'s ERRORS list is
                // `fgetc`'s, which is `read()`'s. Without this the guest gets the same short
                // count as an ordinary end of file, `feof` answers false, and `errno` explains
                // some earlier call.
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
/// [`fread`] gives, and the `EINVAL` it reports is the same named extension.
///
/// # A short count *is* the failure report, so it must carry the reason
///
/// **C17 7.21.8.2p3**: `fwrite` "returns the number of elements successfully written, which will
/// be less than `nmemb` only if a write error is encountered." There is no `EOF` here — the count
/// is the whole signal — so a guest that wants to know *why* has only `errno`, and
/// POSIX.1-2017 XSH `fwrite` defines that list as `fputc`'s, which is `write()`'s. The number is
/// therefore the descriptor's, set in `transfer_out`.
///
/// The single case that carries none is a descriptor that took nothing and reported no error,
/// which POSIX gives no `errno` because `write()` cannot do it; [`Descriptors::write`] forbids it
/// and the adapter refuses it by name.
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
/// # The short count is the failure report, so it carries the same `errno`
///
/// **C17 7.21.6.1p14**: `fprintf` "returns the number of characters transmitted, or a negative
/// value if an output or encoding error occurred". A caller that got fewer characters than it
/// expected has only `errno` to tell it why, and POSIX.1-2017 XSH `fprintf` defines that list as
/// `fputc`'s, which is `write()`'s. So a failing descriptor's number is passed on here exactly as
/// `transfer_out` passes it on for [`fwrite`]; the one state with no number is the same one,
/// and [`Descriptors::write`] says why.
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
    let mut done = 0usize;
    while done < bytes.len() {
        match descriptors.write(stream.fd, &bytes[done..]) {
            Ok(0) => {
                // As `transfer_out`: a descriptor accepting nothing is not making progress, and
                // looping would be a hang rather than a failure.
                //
                // **No `errno`, for the reason `Descriptors::write` gives**: POSIX's `write()`
                // has no zero return for a non-empty buffer, so no number describes this, and
                // the four that would look plausible here each name a cause nobody observed.
                // The contract forbids the state and the adapter refuses it by name; this arm
                // exists to stop rather than to diagnose.
                stream.error = true;
                break;
            }
            Ok(took) => done += took,
            Err(errno) => {
                // The descriptor's own number -- see the `fputc` arm for why none is chosen here.
                stream.error = true;
                ctx.set_errno(errno);
                break;
            }
        }
    }
    Ok(done as u64)
}

// ================================================================== the fopen mode string

/// What a `fopen` or `fdopen` mode string asked for, in the vocabulary of `open(2)`.
///
/// **This crate's own type, not `omni-platform`'s.** The flags an adapter finally hands its
/// filesystem seam are that crate's business; what a *mode string means* is byte arithmetic over
/// a guest argument, so it is here, where it can be tested without a filesystem — the same split
/// [`Descriptors`] already makes. An adapter maps this onto its own open flags in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenMode {
    /// The stream may be read.
    pub read: bool,
    /// The stream may be written.
    pub write: bool,
    /// Create the file if it does not exist (`O_CREAT`).
    pub create: bool,
    /// Truncate an existing file to zero length (`O_TRUNC`).
    pub truncate: bool,
    /// Every write goes to the end of the file (`O_APPEND`).
    pub append: bool,
    /// Fail if the file already exists (`O_EXCL`).
    ///
    /// **Never set without [`create`](Self::create)**, and that is load-bearing rather than
    /// tidiness: POSIX.1-2017 `open` says "if `O_EXCL` is set and `O_CREAT` is not set, the
    /// result is undefined", so an `x` on a mode that does not create has no defined meaning to
    /// pass on. See [`parse_mode`] for what happens to it.
    pub exclusive: bool,
    /// The descriptor should not survive an `exec` (`O_CLOEXEC`), from bionic's `e` modifier.
    ///
    /// Carried rather than dropped. Nothing in this milestone execs, so an adapter that ignores
    /// it is ignoring something unobservable — but the drop is then in one place that can say so,
    /// instead of being a character the parse quietly forgot.
    pub close_on_exec: bool,
}

/// Why [`parse_mode`] refused a mode string.
///
/// Every variant is `EINVAL` to the guest — see [`ModeRefusal::errno`]. The variants exist so
/// that a caller can say *which byte* it refused and why, which is the difference between a
/// diagnosable `fopen` returning `NULL` and an unexplained one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeRefusal {
    /// The mode string was empty, or its first byte was the terminator.
    ///
    /// C17 7.21.5.3p3 lists the permitted mode strings and the empty string is not among them;
    /// bionic reaches its `default:` arm on the terminator and reports `EINVAL`.
    Empty,
    /// The first byte was not one of `r`, `w` or `a`. Carries the byte.
    Access(u8),
    /// A byte after the first was not one of `+`, `b`, `x`, `e`. Carries the byte.
    Modifier(u8),
}

impl ModeRefusal {
    /// The `errno` a guest sees for this refusal.
    ///
    /// `EINVAL` for every variant, which is what `fopen` reports for a mode it cannot parse. It
    /// is a method rather than a constant at the call site so that there is one place to read.
    #[must_use]
    pub const fn errno(self) -> i32 {
        consts::EINVAL
    }
}

impl core::fmt::Display for ModeRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            ModeRefusal::Empty => f.write_str("the mode string is empty"),
            ModeRefusal::Access(byte) => write!(
                f,
                "the mode string starts with {}, which is not one of `r`, `w` or `a`",
                Quoted(byte)
            ),
            ModeRefusal::Modifier(byte) => write!(
                f,
                "the mode string contains the modifier {}, which is not one of `+`, `b`, `x` or \
                 `e`",
                Quoted(byte)
            ),
        }
    }
}

/// A single byte printed so that a non-printable one is still readable in a message.
struct Quoted(u8);

impl core::fmt::Display for Quoted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.is_ascii_graphic() {
            write!(f, "`{}`", self.0 as char)
        } else {
            write!(f, "the byte {:#04x}", self.0)
        }
    }
}

/// Read a `fopen`/`fdopen` mode string into the access it asks for.
///
/// `mode` is the bytes of the guest's mode string. It may be **any bytes at all** — the guest
/// chose them — and this function is total over them: it returns [`OpenMode`] or a named
/// [`ModeRefusal`], never a shortened mode and never a panic.
///
/// # The rule, in one sentence
///
/// The **first** byte selects the access and is mandatory; every **remaining** byte must be a
/// modifier this layer understands, and each one is honoured.
///
/// | mode | access | source |
/// |---|---|---|
/// | `r` | read | C17 7.21.5.3p3 |
/// | `r+` | read and write | C17 7.21.5.3p3 |
/// | `w` | write, create, truncate | C17 7.21.5.3p3 |
/// | `w+` | read and write, create, truncate | C17 7.21.5.3p3 |
/// | `a` | write, create, append | C17 7.21.5.3p3 |
/// | `a+` | read and write, create, append | C17 7.21.5.3p3 |
///
/// `r+b` and `rb+` are **the same mode**, and C17 7.21.5.3p3 lists both spellings explicitly;
/// the modifiers are a set here rather than a sequence, so order cannot matter.
///
/// * **`b`** is accepted and does nothing. POSIX.1-2017 `fopen`: "the character `b` shall have no
///   effect". `std::fs` translates no line ending on any target, so a stream opened without `b` is
///   already binary — which matters more on a Windows host than on a device, and is why nothing in
///   this layer ever touches a `\r`.
/// * **`e`** is bionic's and glibc's `O_CLOEXEC` modifier. It is recorded in
///   [`OpenMode::close_on_exec`] rather than discarded here.
/// * **`x`** is `O_EXCL` (C11 added `wx` and its spellings to 7.21.5.3p3). It is honoured with
///   `w` and `a`, and with `r` it is accepted and has **no effect**, because
///   [`OpenMode::exclusive`] is never set without [`OpenMode::create`] and POSIX.1-2017 `open`
///   leaves `O_EXCL` without `O_CREAT` undefined. That drop is stated here and asserted by a
///   test; it changes no access, only a flag with no defined meaning to change.
/// * **Anything else** after the first byte is [`ModeRefusal::Modifier`].
///
/// # Why there is no length bound, and why that IS the fix
///
/// This function used to live in the adapter behind a 16-byte bound whose own documentation said
/// a longer mode was `EINVAL` — and the code **truncated** instead, so a 17-byte
/// `"rbbbbbbbbbbbbbbb+"` lost its `+` and opened a **read-only** stream for a caller that asked
/// for a read-write one. Nothing reported it: `fopen` returned a perfectly good `FILE *`, and the
/// first `fwrite` failed much later somewhere else. That is the finding this function exists to
/// close.
///
/// Raising the bound would not have closed it; it would have moved it. Refusing past the bound —
/// making the code match the old doc — would have closed the silence but bought a *second*
/// disagreement: C17 7.21.5.3's footnote says an implementation "might choose to ignore the
/// remaining characters" of a mode string that begins with a valid sequence, and bionic's own
/// `__sflags` walks the mode to its terminator with no limit at all, so a long-but-meant mode is
/// one a real device opens and we would have refused.
///
/// So the bound is gone and the whole string is read. **A bound was never what made this safe.**
/// The work is one pass over a slice the *caller* already bounded — the adapter reads the mode
/// with a `cstr` walk capped at `GuestMem::STRING_LIMIT` (64 KiB), the same cap every other guest
/// string in that layer gets — and this pass is strictly cheaper than the walk that produced it.
/// With no bound there is no tail to lose, at any length, so the property the finding is about
/// ("the access granted is the access asked for") holds by construction rather than by a
/// constant.
///
/// # An interior NUL ends the mode, because that is where a C string ends
///
/// `fopen(path, "r\0b+")` asks for **`"r"`**: the `b+` is not part of the string, it is memory
/// after it. Honouring that is not a truncation — it is C's own definition of a string, and it
/// makes this function give the same answer whether a caller hands it the mode or the buffer the
/// mode lives in. Today's adapter cannot reach it (its `cstr` already stops at the NUL), so this
/// is a property of the function's contract rather than a live check on guest input, and it is
/// tested directly.
///
/// # Provenance
///
/// * **C17 7.21.5.3p3 and its footnote** — the mode list, the `x` spellings, and the permission
///   to ignore trailing characters. Normative.
/// * **POSIX.1-2017 `fopen` and `open`** — `b` has no effect; `O_EXCL` without `O_CREAT` is
///   undefined.
/// * **bionic's `__sflags` (`bionic/libc/stdio/flags.cpp`)** — that the first character is
///   switched on and the rest walked to the terminator with no length limit. **RECALLED, not
///   verified: there is no NDK and no bionic checkout on this machine.** Nothing above depends on
///   it — it is cited only as the reason a length *refusal* was rejected, and C17 7.21.5.3p3
///   makes every mode it could disagree about undefined behaviour, so a disagreement cannot make
///   this layer non-conforming.
///
/// # Errors
///
/// A [`ModeRefusal`] naming the byte that refused. All of them are `EINVAL`.
pub fn parse_mode(mode: &[u8]) -> Result<OpenMode, ModeRefusal> {
    // Where the C string ends. `position` and not a bound: nothing is dropped that was part of
    // the string.
    let mode = match mode.iter().position(|&byte| byte == 0) {
        Some(end) => &mode[..end],
        None => mode,
    };
    let Some((&access, modifiers)) = mode.split_first() else {
        return Err(ModeRefusal::Empty);
    };
    // The first byte, and only the first byte, selects the access. A leading space, a `R`, a `+`
    // on its own -- all of them land here, which is bionic's `default:` arm and C17 7.21.5.3p3's
    // undefined case.
    let (reads, writes, create, truncate, append) = match access {
        b'r' => (true, false, false, false, false),
        b'w' => (false, true, true, true, false),
        b'a' => (false, true, true, false, true),
        other => return Err(ModeRefusal::Access(other)),
    };
    let mut plus = false;
    let mut exclusive = false;
    let mut close_on_exec = false;
    // Every remaining byte, however many there are. No `take`, no `min`, no cap: see above.
    for &byte in modifiers {
        match byte {
            b'+' => plus = true,
            b'b' => {}
            b'x' => exclusive = true,
            b'e' => close_on_exec = true,
            other => return Err(ModeRefusal::Modifier(other)),
        }
    }
    Ok(OpenMode {
        // `+` adds the half the access letter did not give. `r+` and `w+` and `a+` are all
        // read-and-write, which is why this is an `||` in both fields rather than a per-arm
        // assignment that has to be right three times.
        read: reads || plus,
        write: writes || plus,
        create,
        truncate,
        append,
        exclusive: exclusive && create,
        close_on_exec,
    })
}

// ------------------------------------------------------------------ the shared machinery

/// Read `length` bytes of guest memory in chunks and hand them to the descriptor.
///
/// Returns how many bytes the descriptor took, which is less than `length` on a short write or an
/// error. The chunking is the bound described in the module documentation.
///
/// **This is where `fputs`'s `EOF` and `fwrite`'s short count get their `errno`**, which is why
/// both of those functions' documentation points here rather than repeating it: a failing
/// descriptor's number is stored, and the one state POSIX gives no number to is stopped without
/// inventing one. See [`Descriptors::write`].
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
                //
                // **The error indicator is set and no `errno` is** -- C17 7.21.7.4p3 and
                // 7.21.8.2p3 make this a failure of `fputs`/`fwrite`, but POSIX.1-2017 XSH
                // `write()` has no zero return for a non-empty buffer, so it defines no errno
                // for the state and this layer will not invent one. `ENOSPC` would send a guest
                // deleting files, `EAGAIN` would send it round the loop again, `EPIPE` would
                // tell it a reader it never had is gone, and `EIO` is the number D23's refusal 7
                // already declines to hand out for a failure nobody classified. The contract on
                // `Descriptors::write` forbids the state; the adapter, which has a refusal
                // channel this crate does not, turns a violation into one naming the symbol.
                stream.error = true;
                break;
            }
            Ok(took) => done += took as u64,
            Err(errno) => {
                // The descriptor's number, unchanged: POSIX.1-2017 XSH `fputc` -- which is
                // `fputs`'s and `fwrite`'s ERRORS list by reference -- is `write()`'s, and only
                // the layer that made the call can tell `ENOSPC` from `EPIPE` from `EBADF`.
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
