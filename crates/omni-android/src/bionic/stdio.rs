//! Bionic's `FILE *` layer: the eleven stream symbols, over the descriptors in [`super::files`].
//!
//! `fopen`, `fdopen`, `fclose`, `feof`, `fflush`, `fgets`, `fileno`, `fputc`, `fputs`, `fread`,
//! `fwrite`. The stream *logic* — the `size * nmemb` multiplication, `fgets`'s three rules, the
//! sticky flags — is in [`omni_bionic::stdio`], over a trait that crate defines, so that it stays
//! in the zero-dependency crate where it can be tested without a filesystem (D19). What is here
//! is the binding: which guest address is which stream, and how a host failure becomes an errno.
//!
//! # ANSWERING THE `sizeof(FILE)` QUESTION: no field of a guest `FILE` is ever read or written
//!
//! D21 recorded `sizeof(FILE) = 152` as **derived from bionic's `struct __sFILE` and not verified
//! against an NDK**, and said the phase that implements stdio "must confirm it against a real
//! header before reading a field". There is still no NDK on this machine. This phase does not
//! need one, because **it does not read a field**:
//!
//! * A `FILE *` is a **key**, not a structure. The stream's descriptor and its two flags live in
//!   a host-side table on [`Bionic`](super::Bionic), keyed by the guest address. `feof`, `fileno`
//!   and `fflush` answer from that table.
//! * The bytes at a `FILE *` are written exactly once — to **zero** — when the object is handed
//!   out, and are never read. A zeroed bionic `FILE` has `_flags == 0`, which that library's own
//!   `__sfp` calls a free slot, so the bytes say "not an open stream", which is a safe reading
//!   rather than a description of one.
//! * So a wrong `FILE_BYTES` cannot produce a wrong *answer*. It can only produce a wrong
//!   *address*: the three standard streams are placed at `__sF + n * FILE_BYTES`, and a guest
//!   translation unit compiled against an old header where `stdout` was the macro `(&__sF[1])`
//!   would compute a different one. That address is not in the table, and every function here
//!   **refuses by name** naming the address — a loud failure rather than a silent one.
//!
//! **The obligation therefore stands, narrowed and stated rather than discharged.** It remains
//! open for anything that makes a `FILE` field observable to the guest — a `ferror` or `clearerr`
//! that the guest inlines as a macro rather than calling, or a phase that writes real bytes into
//! one. Neither is in the reachable set: `ferror`, `clearerr`, `fseek` and `setvbuf` are all
//! absent from the 188. If a later phase adds one, this is the paragraph it invalidates.
//!
//! One residual, recorded because it is the honest edge: a guest translation unit that *inlines*
//! a `FILE` field access instead of calling the function reads our zeroes. For `_flags` that
//! reads as "closed stream", which makes an inlined `feof`/`ferror` macro answer false. That is
//! the safe direction, and it needs a TU built against a pre-Lollipop NDK header to happen at
//! all.
//!
//! # Order: admit the guest's buffer, then touch the descriptor
//!
//! [`fgets`], [`fread`] and [`fwrite`] validate the guest's **whole** buffer before they call
//! into [`omni_bionic::stdio`], because everything past that point is irreversible: a descriptor
//! read is destructive on a pipe and a descriptor write is destructive everywhere. It is review
//! finding **M1**'s rule — `files::read_into_guest` is where the argument is written out in full
//! — arriving in the stream layer, and `admit_transfer` is the one place it is spelled.
//!
//! **Why here and not in `omni-bionic`, where the loops are.** That crate's whole guest-memory
//! vocabulary is [`omni_bionic::memory::GuestMemory`], which has exactly two methods, `read` and
//! `write`. There is no way to ask it whether a range is mapped without *writing* to it, and
//! writing to it is the side effect being avoided — it would also turn `fgets`'s documented
//! "nothing partial is committed, so indeterminate means unchanged" into a false statement.
//! Adding a third, probe method to the trait is what D19 forbids: a defaulted one returning
//! `Ok(())` is a plausible stub for every other implementer (the crate's own `MockMemory`, and
//! the tests that drive it), and an undefaulted one is a method whose only honest implementation
//! needs mapping and protection state, which is precisely the OS-adjacent knowledge that crate
//! has none of and must keep having none of. This layer already holds it, in
//! [`GuestMem::checked_ptr`](crate::mem::GuestMem::checked_ptr), and already has the refusal
//! channel to report it — the same two reasons the zero-byte-write contract above is enforced
//! here rather than there.
//!
//! What it promises and what it does not is `files::read_into_guest`'s paragraph unchanged: **no
//! byte leaves the descriptor unless the entire buffer was usable at the moment it was checked**,
//! and no claim at all about another guest thread unmapping it mid-transfer.
//!
//! # Unbuffered, which is what makes `fflush` honest
//!
//! [`omni_bionic::stdio`] has the argument. The short version: every write reaches the descriptor
//! before the call returns, so `fflush` has nothing of this layer's to flush and succeeding is
//! the contract being satisfied rather than a stub. `fflush` on a *standard* stream still reaches
//! the host's own `Stdout`, which does buffer.
//!
//! # `ferror` and `clearerr` are imported and are not bound, which is what the error flag is for
//!
//! The paragraph above says `ferror`, `clearerr`, `fseek` and `setvbuf` are absent from the 188.
//! That is true and it is **narrower than "not imported"**, so it is worth separating the two:
//! all four are undefined dynamic symbols of the APK's libraries — `ferror` from
//! `libroblox.so`, `libbacktrace-native.so` and `libzstd-jni`, `clearerr` from `libroblox.so` —
//! and none is reachable from the 3,594 initializers, so none is bound here and no guest call in
//! this milestone can reach one.
//!
//! The consequence is about **`errno`, not about `FILE`**. C17 7.21.10.3's `ferror` is the only
//! call that can read a stream's error indicator, so with it unbound the flag
//! [`omni_bionic::stdio::Stream`] keeps is write-only: correct, maintained where C says, and
//! unobservable. **`errno` is therefore the whole of what a guest learns about why a stream
//! operation failed**, which is why a site that reports `EOF` without setting one is a silent
//! wrong answer rather than a missing convenience — the guest has no second place to look.

use std::cell::RefCell;

use omni_bionic::context::GuestContext;
use omni_bionic::errno::consts;
use omni_bionic::error::{BionicError, BionicResult};
use omni_bionic::stdio::{self, Descriptors, Stream, EOF};
use omni_mem::GuestAddr;
use omni_platform::fs::{Filesystem, FsError, OpenFlags};

use crate::boundary::ImportCall;
use crate::error::AbiResult;
use crate::mem::Blame;

use super::files::{errno_for, filesystem, settle, Settled};
use super::view::GuestView;
use super::{active, enter};

// **There is no bound on a `fopen` mode string here, and there used to be.**
//
// Review finding M5: a mode over sixteen bytes was *silently truncated*, so
// `"rbbbbbbbbbbbbbbb+"` lost its `+` and yielded a read-only stream while the code's own doc
// said `EINVAL`. The caller is already bounded — `GuestMem::cstr` caps at `STRING_LIMIT`, 64 KiB
// — so the parse is one pass over a slice that is already finite, and with no second bound there
// is no tail to lose at any length.
//
// Refusing past a length was considered and rejected: C17 7.21.5.3p3's footnote lets an
// implementation ignore the characters after a valid mode, and bionic's own `__sflags` walks to
// the terminator with no limit, so a long-but-meant mode is one a real device opens.

/// [`omni_bionic::stdio::Descriptors`] over `omni-platform`'s filesystem seam.
///
/// # Why it stashes the failure it could not classify
///
/// The trait's error type is a guest `errno`, because that is the vocabulary the layer above it
/// speaks. But [`files::errno_for`](super::files::errno_for) deliberately has no errno for a host
/// failure `std::io::ErrorKind` could not classify — giving one `EIO` would hand guest code a
/// specific, actionable failure for something nobody identified.
///
/// So an unclassifiable failure is **stashed** and `EIO` is returned to the stream logic to
/// unwind with; the handler then finds the stash and turns the whole call into a refusal naming
/// the symbol and the host's own message. It is the same shape [`GuestView`] already uses to
/// carry a rich `AbiError` through `omni-bionic`'s thin `Fault`.
///
/// # The second thing it stashes: a write that took none of a non-empty buffer
///
/// [`omni_bionic::stdio::Descriptors::write`]'s contract is that a non-empty buffer yields at
/// least one byte taken or an errno, because **POSIX.1-2017 XSH `write()` has no zero return for
/// `nbyte > 0`** — it transfers at least one byte or fails with `errno` set. The state therefore
/// has no `errno` anywhere in POSIX, and `omni-bionic` refuses to invent one: it sets the stream's
/// error indicator, stops (looping would hang), and leaves `errno` alone. On its own that is the
/// finding this file was reviewed for — the guest gets `EOF` from `fputc`, or a short count from
/// `fwrite`, and reads whatever `errno` an earlier, unrelated call left behind.
///
/// **This is the layer that can do better, so it does.** Only `omni-platform`'s `std::io::Write`
/// half can produce a zero return at all: the `/dev/*` entries return `buf.len()`, a pipe returns
/// at least one byte or `EAGAIN`/`EPIPE`, and a directory or a read-only descriptor is `EBADF`
/// before any write happens. A regular file and the standard streams go through
/// `std::io::Write::write`, whose own contract permits `Ok(0)` for "the underlying object is no
/// longer able to accept bytes". If that ever happens the failure is exactly the kind
/// `errno_for` has no number for, so it takes the same road: stashed, and refused by name.
///
/// **Stated plainly rather than implied: no input I could construct reaches it.** Nothing in the
/// seam returns `Ok(0)` for a non-empty buffer today, so this is a guard on a `std` contract
/// rather than on guest input, and it is kept rather than deleted because it is the only place a
/// `std::io` zero return could become a guest-visible answer with a borrowed reason.
pub(super) struct HostDescriptors<'a> {
    fs: &'a Filesystem,
    /// The message a refusal should carry, if this call hit something with no honest errno.
    ///
    /// A `String` rather than an `FsError` because both producers are now in this file and only
    /// one of them has a host error object: the zero-byte write is a contract violation the seam
    /// reported as success, so there is nothing to carry but the sentence describing it.
    unclassified: RefCell<Option<String>>,
}

impl<'a> HostDescriptors<'a> {
    fn new(fs: &'a Filesystem) -> Self {
        HostDescriptors { fs, unclassified: RefCell::new(None) }
    }

    /// Turn a seam failure into an errno, stashing one that has none.
    fn errno(&self, error: FsError) -> i32 {
        match error.kind().and_then(errno_for) {
            Some(errno) => errno,
            None => self.stash(error.to_string()),
        }
    }

    /// Record why this call must be refused, and hand the stream logic a number to unwind with.
    ///
    /// The number **never reaches the guest**: every caller of this type checks the stash before
    /// it returns and refuses. The first reason wins, because it is the one that describes the
    /// state the rest of the call then ran in.
    fn stash(&self, why: String) -> i32 {
        let mut slot = self.unclassified.borrow_mut();
        if slot.is_none() {
            *slot = Some(why);
        }
        consts::EIO
    }

    /// The stashed failure, if the call hit one.
    fn take_unclassified(&self) -> Option<String> {
        self.unclassified.borrow_mut().take()
    }
}

impl Descriptors for HostDescriptors<'_> {
    fn read(&self, fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
        // `Ok(0)` is passed straight through: it is end of file, which C17 7.21.8.1p3 makes a
        // return value and not an error, and `feof` is what reports it. The asymmetry with
        // `write` below is `read(2)`'s and `write(2)`'s own.
        self.fs.read(fd, buf).map_err(|error| self.errno(error))
    }

    fn write(&self, fd: i32, buf: &[u8]) -> Result<usize, i32> {
        match self.fs.write(fd, buf) {
            Ok(taken) => match write_contract_violation(fd, buf.len(), taken) {
                Some(why) => Err(self.stash(why)),
                None => Ok(taken),
            },
            Err(error) => Err(self.errno(error)),
        }
    }

    fn flush(&self, fd: i32) -> Result<(), i32> {
        self.fs.flush(fd).map_err(|error| self.errno(error))
    }
}

/// Why a *successful* seam write must nevertheless refuse, if it must.
///
/// The one rule: **a non-empty buffer must yield at least one byte.** POSIX.1-2017 XSH `write()`
/// has no zero return for `nbyte > 0`, so a zero here is a state POSIX never had to name — and
/// therefore one it gives no `errno`. Reporting it as a short count would hand the guest `EOF`
/// from `fputc`, or fewer items from `fwrite`, with `errno` still holding whatever an earlier
/// call left there; that is the finding this function exists to close.
///
/// A free function rather than an arm inside [`Descriptors::write`] so that the rule can be
/// tested directly: no descriptor in `omni-platform` returns zero for a non-empty buffer, so the
/// arm itself is not reachable from any input, and a rule that cannot be exercised is a rule
/// nobody is checking. The two negative cases matter as much as the positive one — an empty
/// buffer returning zero is POSIX's own answer, and a short-but-non-zero return is an ordinary
/// partial write the stream layer loops on.
fn write_contract_violation(fd: i32, requested: usize, taken: usize) -> Option<String> {
    if requested == 0 || taken > 0 {
        return None;
    }
    Some(format!(
        "the host took none of a {requested} byte write to fd {fd} and reported no error. POSIX's \
         `write()` has no zero return for a non-zero count, so there is no errno that describes \
         this and none is invented: a guest told `ENOSPC` would delete files it does not need to, \
         one told `EAGAIN` would retry forever, and one told `EIO` would report broken hardware \
         nobody observed"
    ))
}

/// `omni-bionic`'s answer in the seam's vocabulary — the one place the two flag sets meet.
///
/// The parse itself is [`omni_bionic::stdio::parse_mode`], in the crate whose module header says
/// byte arithmetic over a guest argument belongs there: it needs no OS, and it is testable
/// without a filesystem. What is left here is the mapping, and the one thing the mapping drops.
///
/// **`close_on_exec` is dropped here, named.** `OpenFlags` has no field for it because nothing in
/// this process execs; recording it in `OpenMode` and dropping it at one visible site is the
/// difference between a decision and a character that vanished in a parse, which is what `e` used
/// to do.
fn open_flags(mode: omni_bionic::stdio::OpenMode) -> OpenFlags {
    OpenFlags {
        read: mode.read,
        write: mode.write,
        create: mode.create,
        exclusive: mode.exclusive,
        truncate: mode.truncate,
        append: mode.append,
        directory: false,
    }
}

/// Read a `fopen` mode string. Bounded by `GuestMem::cstr` and by nothing else — see the note
/// where `MAX_MODE_BYTES` used to be.
fn mode_argument(view: &GuestView<'_>, pointer: u64, argument: usize) -> AbiResult<Vec<u8>> {
    if pointer == 0 {
        return Err(view.refusal(format!("argument {argument} is a null mode string")));
    }
    let at = GuestAddr::try_from(pointer)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
    view.mem().cstr(at, Blame::new(view.symbol(), view.address(), argument))
}

/// Turn an `omni-bionic` failure into the boundary's, keeping the rich error the view stashed.
///
/// The same conversion `handlers`' `Lift` makes, spelled again here rather than made public: a
/// memory failure keeps the `BadPointer` that names the argument and which of `admit`'s rules
/// refused it, and anything else becomes a refusal carrying its own reason.
fn lift<T>(view: &GuestView<'_>, result: BionicResult<T>) -> AbiResult<T> {
    result.map_err(|error| match error {
        BionicError::Memory(fault) => view.fault(fault),
        other => view.refusal(other.to_string()),
    })
}

/// The stream a guest `FILE *` names, or a refusal naming the address.
///
/// **A refusal rather than `EBADF`**, and that is the decision. Every `FILE *` in this guest's
/// world came out of this module, so a pointer that is not in the table is not a closed stream —
/// it is a wild pointer, a use-after-`fclose`, or the `&__sF[n]` arithmetic described in the
/// module documentation. Reporting it as an ordinary invalid stream would let guest code route
/// around it, and this project's whole failure mode is a plausible answer surfacing later.
///
/// **It is a deviation from POSIX and is named as one.** POSIX.1-2017 XSH `fileno` says the call
/// "shall return -1 and set `errno` to indicate the error", with `EBADF` for a stream that is not
/// valid; `fflush` lists `EBADF` too. A refusal answers neither, on purpose: `EBADF` is the
/// answer for a stream that *was* one, and none of the three causes above ever was. The deviation
/// is safe in the direction that matters — the guest gets a named failure instead of a number it
/// could branch past — and it is the one place in this file where the `errno` a standard names is
/// deliberately not set.
fn stream_of(view: &GuestView<'_>, file: u64) -> AbiResult<Stream> {
    view.active.bionic.stream_of(file).ok_or_else(|| {
        view.refusal(format!(
            "the guest passed the FILE pointer {file:#x}, which this instance never handed out. \
             Every FILE * here comes from `fopen`, `fdopen` or the three `__sF` streams, so this \
             is a wild pointer, a stream used after `fclose`, or a translation unit computing \
             `&__sF[n]` with a different `sizeof(FILE)` than the {file_bytes} this layer places \
             them at (which is derived from bionic's headers and unverified -- see the module \
             documentation)",
            file_bytes = super::FILE_BYTES
        ))
    })
}

/// Run `body` with the stream logic wired up, and write the stream's flags back afterwards.
///
/// The write-back is the part that is easy to forget: [`omni_bionic::stdio`] takes a
/// `&mut Stream`, and a handler that dropped the mutated copy would leave `feof` answering false
/// forever after an end of file.
fn with_stream<T>(
    c: &mut ImportCall<'_, '_>,
    file: u64,
    body: impl FnOnce(&mut GuestView<'_>, &HostDescriptors<'_>, &mut Stream) -> AbiResult<T>,
) -> AbiResult<T> {
    let state = active(c.symbol(), c.address())?;
    let mut view = enter(c, &state);
    let fs = filesystem(&view)?;
    let mut stream = stream_of(&view, file)?;
    let descriptors = HostDescriptors::new(fs);
    let produced = body(&mut view, &descriptors, &mut stream);
    state.bionic.update_stream(file, stream);
    // **After the write-back, not before.** The flags the stream logic set are C's own record of
    // what happened and they stay true whether or not the call then refuses; a refusal that
    // returned first would leave `feof` answering false about a stream that did reach its end.
    if let Some(why) = descriptors.take_unclassified() {
        return Err(view.refusal(why));
    }
    produced
}

/// Format-and-write: push bytes the **host** already holds at a guest stream.
///
/// **The binding HANDOFF called "one binding away" for three phases.** Both halves existed from
/// phase 3b — `format::render` formats and this module has the streams — and what was missing was
/// that the formatted text lives *host*-side, so [`fwrite`](omni_bionic::stdio::fwrite)'s
/// guest-pointer source is the wrong shape for it. `omni_bionic::stdio::write_host_bytes` is the
/// same short-write, error-flag and `errno` bookkeeping with the source replaced.
///
/// Returns what C's `fprintf` returns: how many bytes were written.
pub(super) fn print_to_stream(
    c: &mut ImportCall<'_, '_>,
    file: u64,
    text: &str,
) -> AbiResult<i32> {
    let bytes = text.as_bytes().to_vec();
    let written = with_stream(c, file, |view, descriptors, stream| {
        let produced =
            omni_bionic::stdio::write_host_bytes(view, descriptors, stream, &bytes);
        lift(view, produced)
    })?;
    i32::try_from(written).map_err(|_| crate::AbiError::Refused {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: "more bytes were written than an int can report".to_string(),
    })
}

// ================================================================== opening and closing

/// `FILE *fopen(const char *pathname, const char *mode)`
///
/// Returns a guest `FILE *` — the address of this stream's own object in the adapter's arena — or
/// `NULL` with `errno` set.
pub(super) fn fopen(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (path, mode) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let path_bytes = super::files::path_for(view.blaming(0), path, 0)?;
        let mode_bytes = mode_argument(view.blaming(1), mode, 1)?;
        let flags = match omni_bionic::stdio::parse_mode(&mode_bytes) {
            Ok(mode) => open_flags(mode),
            Err(why) => {
                view.set_errno(why.errno());
                c.ret().u64(0);
                return Ok(());
            }
        };
        let fs = filesystem(&view)?;
        match super::files::settle(&view, fs.open(&path_bytes, flags))? {
            super::files::Settled::Done(fd) => match state.bionic.open_stream(&view, fd)? {
                Some(pointer) => pointer as u64,
                None => {
                    let _ = fs.close(fd);
                    view.set_errno(consts::EMFILE);
                    0
                }
            },
            super::files::Settled::Failed(errno) => {
                view.set_errno(errno);
                0
            }
        }
    };
    c.ret().u64(result);
    Ok(())
}

/// `FILE *fdopen(int fd, const char *mode)`
///
/// Associates a stream with a descriptor the guest already has. The descriptor is **checked**
/// against this instance's table rather than taken on trust: `fdopen(41, "r")` on a number the
/// guest invented would otherwise produce a `FILE *` whose every operation failed later, instead
/// of the `EBADF` `fdopen` is defined to give.
///
/// The mode is parsed for validity and is otherwise not re-checked against the descriptor's own
/// access, which is what the C library does — a mode inconsistent with the descriptor is
/// undefined behaviour there, and here it simply fails at the first operation with `EBADF` from
/// the seam.
pub(super) fn fdopen(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (fd, mode) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let mode_bytes = mode_argument(view.blaming(1), mode, 1)?;
        if let Err(why) = omni_bionic::stdio::parse_mode(&mode_bytes) {
            view.set_errno(why.errno());
            c.ret().u64(0);
            return Ok(());
        }
        let fs = filesystem(&view)?;
        if !fs.is_open(fd) {
            view.set_errno(consts::EBADF);
            c.ret().u64(0);
            return Ok(());
        }
        match state.bionic.open_stream(&view, fd)? {
            Some(pointer) => pointer as u64,
            None => {
                view.set_errno(consts::EMFILE);
                0
            }
        }
    };
    c.ret().u64(result);
    Ok(())
}

/// `int fclose(FILE *stream)`
///
/// **Flushes, then closes**, then releases the `FILE` object's slot. Returns 0, or `EOF` with
/// `errno` if either half failed.
///
/// # The flush, which was missing and mattered
///
/// C17 7.21.5.1p2: `fclose` flushes the stream before closing it, and any unwritten buffered data
/// are delivered to the host environment. This layer buffers nothing of its own — which is what
/// the old comment here said, and why the omission looked harmless — but **the layer below does**:
/// `Filesystem::flush` reaches `std::io::Stdout` and `Stderr`, which buffer, and
/// `Filesystem::close` only removes the table entry. CONFIRMED live: `fclose(stdout)` returned 0
/// with the last line the engine printed simply not there.
///
/// A regular file is unaffected — `std::fs::File` is unbuffered — so this is entirely about the
/// three standard streams, which is exactly where a lost diagnostic costs most.
///
/// **There is no matching obligation for `exit`.** C17 7.22.4.4p2 has `exit` flush every stream,
/// but only `_exit` is bound here, and 7.22.4.5p2 leaves flushing implementation-defined for
/// `_Exit` while POSIX says `_exit` does not flush stdio.
///
/// The slot is released and the descriptor closed **whatever the flush did**, which is C's own
/// rule: after `fclose` the stream may not be used again even if it reported a failure. Keeping
/// the slot on a failure would leak one per failure and let a guest exhaust the table.
pub(super) fn fclose(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let file = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let stream = stream_of(&view, file)?;
        let fs = filesystem(&view)?;
        state.bionic.close_stream(file);
        // Flush first, close second, and close **whatever the flush reported**: the stream is
        // gone after `fclose` either way, and a descriptor left open because its flush failed
        // would be a leak with a legitimate-looking cause.
        let flushed = super::files::settle(&view, fs.flush(stream.fd))?;
        let closed = super::files::settle(&view, fs.close(stream.fd))?;
        match (flushed, closed) {
            (super::files::Settled::Done(()), super::files::Settled::Done(())) => 0,
            // Whichever failed, the guest is told once. The flush's errno wins when both fail,
            // because it is the one that says data was lost rather than that a handle was.
            (super::files::Settled::Failed(errno), _)
            | (_, super::files::Settled::Failed(errno)) => {
                view.set_errno(errno);
                EOF
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

// ================================================================== state

/// `int feof(FILE *stream)`
///
/// Answers from the host-side table, never from the guest's `FILE` bytes — see the module
/// documentation on why that is what keeps an unverified `sizeof(FILE)` harmless.
pub(super) fn feof(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let file = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let view = enter(c, &state);
        stdio::feof(&stream_of(&view, file)?)
    };
    c.ret().i32(result);
    Ok(())
}

/// `int ferror(FILE *stream)`
///
/// The stream's error indicator, from the host-side table as [`feof`]'s end-of-file indicator is.
/// MEASURED reader: the engine's worker running `SingleSurfaceApp::initializeWithAppStarter`, once
/// the engine settings reached a live engine.
pub(super) fn ferror(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let file = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let view = enter(c, &state);
        stdio::ferror(&stream_of(&view, file)?)
    };
    c.ret().i32(result);
    Ok(())
}

/// `void clearerr(FILE *stream)` -- both indicators cleared, in the host-side table `feof` and
/// `ferror` read.
///
/// MEASURED reader: a guest worker (started at link `0x284d168`) on the **second** launch of a
/// kept data directory, at `0x4ece754` -- a code path a fresh install never takes.
pub(super) fn clearerr(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let file = c.args().next_u64()?;
    with_stream(c, file, |_, _, stream| {
        stdio::clearerr(stream);
        Ok(())
    })?;
    c.ret().void();
    Ok(())
}

/// `int fseeko(FILE *stream, off_t offset, int whence)`
///
/// A seek on the stream's descriptor and **the end-of-file indicator cleared**, which C17
/// 7.21.9.2p5 says a successful `fseek` does -- the one thing a seek owes the stream beyond the
/// descriptor. There is no buffer to discard or flush: this layer is unbuffered (see the module
/// documentation), so the descriptor's offset *is* the stream's position. Returns 0, or -1 with
/// `errno` (`EINVAL` for a bad `whence` or a negative position, `ESPIPE` for a pipe).
///
/// No `FILE` field is read or written, so the `sizeof(FILE)` obligation the module documentation
/// narrows is not touched: the indicator is the host-side table's, as for `feof`.
///
/// MEASURED reader: a guest worker at `libroblox.so` link `0x2b59734`, a file class's
/// `seek(offset, whence)` that calls `ftello` (`0x2b5975c`) on success -- both stubs decoded by
/// `tools/init_reach.py`'s PLT map, which only names a stub when two encodings agree. The worker
/// died on the `Unbound` this replaces, and a second worker then faulted reading what looks like
/// the dead one's stack; see the gate.
pub(super) fn fseeko(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (file, offset, whence) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()? as i64, a.next_i32()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let mut stream = stream_of(&view, file)?;
        match settle(&view, fs.seek(stream.fd, offset, whence))? {
            Settled::Done(_) => {
                stream.eof = false;
                state.bionic.update_stream(file, stream);
                0
            }
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `off_t ftello(FILE *stream)`
///
/// The stream's position, which on an unbuffered stream is the descriptor's offset: `lseek(fd,
/// 0, SEEK_CUR)`, moving nothing. -1 with `errno` on failure (`ESPIPE` for a pipe).
///
/// **Bound because it was decoded, not because a run reached it**: it is the call on
/// [`fseeko`]'s success path at `0x2b5975c`, so the run that reaches `fseeko` reaches this one
/// instruction later, and waiting a three-minute run to be told so would be ceremony.
pub(super) fn ftello(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let file = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let stream = stream_of(&view, file)?;
        match settle(&view, fs.seek(stream.fd, 0, 1))? {
            Settled::Done(at) => i64::try_from(at).unwrap_or(-1),
            Settled::Failed(errno) => {
                view.set_errno(errno);
                -1
            }
        }
    };
    c.ret().u64(result as u64);
    Ok(())
}

/// `int fileno(FILE *stream)`
pub(super) fn fileno(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let file = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let view = enter(c, &state);
        stream_of(&view, file)?.fd
    };
    c.ret().i32(result);
    Ok(())
}

/// `int fflush(FILE *stream)`
///
/// **`fflush(NULL)` flushes every output stream**, which is C's rule and is the form a program
/// uses before it aborts or forks. Implementing it as a no-op would be the plausible wrong
/// answer: the call would succeed and the host's own `Stdout` buffer would still be holding the
/// last thing the engine said before it died.
///
/// # Which `errno` a `fflush(NULL)` leaves behind when more than one stream fails
///
/// C17 7.21.5.2p3 gives `fflush` one return value and one error indicator per stream, and says
/// nothing about `errno` at all; POSIX.1-2017 XSH `fflush` gives it a single `errno` and does not
/// say whose it is when the argument is `NULL` and several streams fail. **The last failing
/// stream's number is what the guest reads here**, because every stream is flushed — stopping at
/// the first failure would leave the rest of the engine's output in the host's buffers, which is
/// the whole reason this form exists. Each stream's own error indicator is set as C requires, so
/// the per-stream truth is not lost; it is only unreadable until `ferror` is bound.
///
/// The one thing that is *not* left to chance: `EOF` is returned if **any** stream failed, so a
/// later failure cannot mask an earlier one in the return value.
pub(super) fn fflush(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let file = c.args().next_u64()?;
    if file == 0 {
        let state = active(c.symbol(), c.address())?;
        let mut view = enter(c, &state);
        let fs = filesystem(&view)?;
        let descriptors = HostDescriptors::new(fs);
        let mut code = 0;
        for pointer in state.bionic.stream_pointers() {
            let Some(mut stream) = state.bionic.stream_of(pointer) else {
                continue;
            };
            if stdio::fflush(&mut view, &descriptors, &mut stream) == EOF {
                code = EOF;
            }
            state.bionic.update_stream(pointer, stream);
        }
        if let Some(why) = descriptors.take_unclassified() {
            return Err(view.refusal(why));
        }
        drop(view);
        c.ret().i32(code);
        return Ok(());
    }
    let result = with_stream(c, file, |view, descriptors, stream| {
        Ok(stdio::fflush(view, descriptors, stream))
    })?;
    c.ret().i32(result);
    Ok(())
}

// ================================================================== transfers

/// Admit the **whole** of a guest transfer buffer, before anything irreversible happens to a
/// descriptor.
///
/// Review finding **M1** in the stream layer. `files::transfer_buffer` is the same rule for the
/// raw descriptor calls and carries the full argument; the module header above says why this
/// side of the boundary is where it has to live rather than inside `omni-bionic`'s loops.
///
/// **A zero-length transfer is admitted without a check, and that is load-bearing rather than an
/// optimisation.** `fread(p, 1, 0, f)` and `fwrite(p, 0, n, f)` leave the stream untouched by C17
/// 7.21.8.1p3's own words, `fgets` with a non-positive `size` reads and writes nothing, and
/// `read(fd, NULL, 0)` is legal C. [`GuestMem::checked_ptr`](crate::mem::GuestMem::checked_ptr)
/// does **not** short-circuit an empty range the way `read_bytes` and `write_bytes` do — it goes
/// straight to `admit`, which answers for the address — so a check here without this guard would
/// refuse a correct program. `order-B1` exists in the mutation table because over-correcting in
/// exactly this way is the plausible mistake.
///
/// `write` chooses the access the *operation* needs and no more: a `fwrite` source is admitted
/// **readable**, not writable, because a guest handing `fwrite` a pointer into its own `.rodata`
/// is ordinary and a stricter-looking check would refuse it (`order-B2`, for the same over-
/// correction one layer down).
fn admit_transfer(
    view: &GuestView<'_>,
    pointer: u64,
    length: u64,
    write: bool,
    argument: usize,
) -> AbiResult<()> {
    if length == 0 {
        return Ok(());
    }
    let at = GuestAddr::try_from(pointer)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
    // The same target-width refusal made for a length. On an LP64 host it cannot fire; on a
    // 32-bit one it is the difference between a refusal and a truncated length that would admit
    // less memory than the transfer goes on to touch.
    let len = usize::try_from(length)
        .map_err(|_| view.refusal("a transfer length wider than the host's usize"))?;
    view.mem().checked_ptr(at, len, write, Blame::new(view.symbol(), view.address(), argument))?;
    Ok(())
}

/// `char *fgets(char *s, int size, FILE *stream)`
///
/// The whole of `s` is admitted before the descriptor is read from — see the module header. The
/// length admitted is **`size`, not `size - 1`**: C17 7.21.7.2p2 has `fgets` read at most one
/// less than `size` characters and then store a null character after the last one, so the object
/// it is given has to be `size` characters long and `size` is what a correct call can write.
///
/// The honest edge, stated rather than left to be discovered: this layer therefore refuses a call
/// whose `size` overruns the object even when the line that arrives would have fit. C licenses
/// the refusal — the argument really does describe an array the guest does not have — but a
/// device would not have noticed, so the deviation is named here. The alternative is the one M1
/// rejected: admit as you go, and report a buffer that is half unmapped as a stream that ended.
pub(super) fn fgets(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (s, size, file) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?, a.next_u64()?)
    };
    let result = with_stream(c, file, |view, descriptors, stream| {
        view.blaming(0);
        if size > 0 {
            // `size <= 0` is the case C17 7.21.7.2 does not define, and `omni_bionic::stdio`
            // answers it by reading nothing, writing nothing and setting no `errno`. There is no
            // transfer to admit, so admitting one would refuse a call that touches neither the
            // buffer nor the descriptor.
            admit_transfer(view, s, size as u64, true, 0)?;
        }
        let produced = stdio::fgets(view, descriptors, stream, s, size);
        lift(view, produced)
    })?;
    c.ret().u64(result);
    Ok(())
}

/// `int fputs(const char *s, FILE *stream)`
///
/// **No admission here, and the reason is structural rather than an omission.**
/// `omni_bionic::stdio::fputs` begins with `bounded_strlen`, which walks `[s, s + length]` a byte
/// at a time looking for the NUL **before** `transfer_out` makes its first descriptor write. So
/// the whole source has not merely been probed, it has been *read*, and a range that faults ends
/// the call with nothing yet in the descriptor. Adding `admit_transfer` in front of it would be a
/// check no input can fail, which `VERIFICATION.md` entry 12 is about.
///
/// It shares the cross-thread window everything else here shares, and shares it in the same
/// place: the walk and the transfer are two passes, and a range another guest thread unmaps
/// between them is a race no check on this side can close.
pub(super) fn fputs(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (s, file) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let result = with_stream(c, file, |view, descriptors, stream| {
        view.blaming(0);
        let produced = stdio::fputs(view, descriptors, stream, s);
        lift(view, produced)
    })?;
    c.ret().i32(result);
    Ok(())
}

/// `int fputc(int c, FILE *stream)`
pub(super) fn fputc(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (value, file) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?)
    };
    let result =
        with_stream(c, file, |view, descriptors, stream| {
            Ok(stdio::fputc(view, descriptors, stream, value))
        })?;
    c.ret().i32(result);
    Ok(())
}

/// `size_t fread(void *ptr, size_t size, size_t nmemb, FILE *stream)`
///
/// The whole of `ptr` is admitted before the descriptor is read from — see the module header.
///
/// # The overflowing `size * nmemb` is deliberately **not** admitted, and stays `EINVAL`
///
/// `omni_bionic::stdio::fread` answers an unrepresentable product with `EINVAL` and zero items
/// (POSIX XSH 2.3's permitted extension, argued at that function), and that is a fact about the
/// *arguments* which holds whatever guest memory looks like. Admitting `size.wrapping_mul(nmemb)`
/// here would hand the guest a refusal about its pointer for a call whose pointer was never the
/// problem, and admitting the saturated product would refuse where `EINVAL` is the answer. So the
/// product is computed with the same `checked_mul`, and a `None` skips the admission and lets the
/// layer below give the answer it already gives.
pub(super) fn fread(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (ptr, size, nmemb, file) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let result = with_stream(c, file, |view, descriptors, stream| {
        view.blaming(0);
        if let Some(total) = size.checked_mul(nmemb) {
            admit_transfer(view, ptr, total, true, 0)?;
        }
        let produced = stdio::fread(view, descriptors, stream, ptr, size, nmemb);
        lift(view, produced)
    })?;
    c.ret().u64(result);
    Ok(())
}

/// `size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream)`
///
/// **The source side of the same finding, and it is an instance rather than a mirror-image that
/// does not apply.** `omni_bionic::stdio`'s `transfer_out` reads one `TRANSFER_CHUNK` out of
/// guest memory, hands it to the descriptor, and only then looks at the next chunk — so a source
/// readable for its first page and not its second put 4 KiB into a **pipe** and then reported the
/// whole call as a failure. A byte in a pipe cannot be taken back out: the reader has already
/// been told a message started, and for the glue's command pipe half a message is a command.
/// That is exactly why M1 named `__write_chk` beside `read` and `pread`.
///
/// Below one chunk the defect cannot show — the single `read_all` already failed before any host
/// write — which is why the regression test crosses the boundary and says so.
///
/// The source is admitted **readable**, not writable: a guest that hands `fwrite` a pointer into
/// its own `.rodata` is doing something ordinary, and demanding more than the operation needs
/// reads as stricter while being wrong (`order-B2` is that over-correction one layer down).
pub(super) fn fwrite(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (ptr, size, nmemb, file) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let result = with_stream(c, file, |view, descriptors, stream| {
        view.blaming(0);
        if let Some(total) = size.checked_mul(nmemb) {
            // As `fread`: an unrepresentable product is `EINVAL` from the layer below, not a
            // refusal about a pointer that was never the problem.
            admit_transfer(view, ptr, total, false, 0)?;
        }
        let produced = stdio::fwrite(view, descriptors, stream, ptr, size, nmemb);
        lift(view, produced)
    })?;
    c.ret().u64(result);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The mapping, which is all that is left here.** The parse itself moved to
    /// `omni_bionic::stdio::parse_mode` and is tested there, case by case, in
    /// `crates/omni-bionic/tests/stdio_modes_tests.rs`.
    ///
    /// What this asserts is the thing a move can silently get wrong: that every field of
    /// `OpenMode` reaches the field of `OpenFlags` that means the same thing. A mapping that
    /// swapped `truncate` and `append` would pass every test in the other crate.
    #[test]
    fn every_open_mode_field_reaches_the_seam_flag_that_means_the_same_thing() {
        use omni_bionic::stdio::parse_mode;
        let read = open_flags(parse_mode(b"r").expect("r"));
        assert_eq!(
            (read.read, read.write, read.create, read.truncate, read.append, read.exclusive),
            (true, false, false, false, false, false)
        );
        let write = open_flags(parse_mode(b"w").expect("w"));
        assert_eq!(
            (write.read, write.write, write.create, write.truncate, write.append),
            (false, true, true, true, false),
            "`w` truncates and does not append"
        );
        let append = open_flags(parse_mode(b"a").expect("a"));
        assert_eq!(
            (append.read, append.write, append.create, append.truncate, append.append),
            (false, true, true, false, true),
            "`a` appends and does not truncate -- the pair a swapped mapping would hide"
        );
        assert!(open_flags(parse_mode(b"r+").expect("r+")).write);
        assert!(open_flags(parse_mode(b"w+").expect("w+")).read);
        assert!(open_flags(parse_mode(b"wx").expect("wx")).exclusive);
        // Never a directory: `fopen` opens a stream, and `O_DIRECTORY` would make the seam
        // refuse every ordinary file.
        assert!(!open_flags(parse_mode(b"r").expect("r")).directory);
    }

    /// **Review finding M5, in the shipping path.**
    ///
    /// The bound that truncated a long mode is gone, so the `+` survives at any length. The
    /// review named `"rbbbbbbbbbbbbbb+"`, which is **sixteen** bytes and parsed correctly even
    /// with the bound; the shape begins at seventeen. Both are asserted by name so the
    /// correction cannot be lost again.
    #[test]
    fn a_long_mode_keeps_its_plus_all_the_way_through_the_adapter() {
        use omni_bionic::stdio::parse_mode;
        for mode in [&b"rbbbbbbbbbbbbbb+"[..], b"rbbbbbbbbbbbbbbb+", b"rbbbbbbbbbbbbbbbbbbbbbb+"] {
            let flags = open_flags(parse_mode(mode).expect("a legal mode at any length"));
            assert!(
                flags.read && flags.write,
                "`{}` is a read-write mode and the adapter must not shorten it",
                String::from_utf8_lossy(mode)
            );
        }
    }

    /// **The review finding, as a rule that can be exercised.**
    ///
    /// `fputc`, `transfer_out` and `write_host_bytes` set a stream's error indicator on a
    /// zero-byte write and never touched `errno`, so the guest got `EOF` and then read the reason
    /// for some earlier, unrelated call. `omni-bionic` cannot fix that — POSIX.1-2017 XSH
    /// `write()` has no zero return for `nbyte > 0` and therefore no `errno` for the state, and
    /// inventing one would be a specific, believable, wrong reason. This layer can, because it
    /// has a refusal channel, and this is the rule it refuses by.
    ///
    /// All three cases are asserted, not just the refusing one: `docs/VERIFICATION.md` entry 12
    /// is about branches nothing can take, and a rule that refused an ordinary short write or an
    /// empty write would break every chunked transfer in the layer above while still passing a
    /// test that only checked the zero case.
    #[test]
    fn a_write_that_took_none_of_a_non_empty_buffer_is_refused_and_nothing_else_is() {
        // The violation: bytes were offered and none was taken, with no error reported.
        let why = write_contract_violation(7, 4096, 0).expect("a zero-byte write must refuse");
        assert!(why.contains("fd 7"), "the refusal must name the descriptor: {why}");
        assert!(why.contains("4096"), "the refusal must name how much was offered: {why}");
        assert!(
            why.contains("ENOSPC") && why.contains("EAGAIN") && why.contains("EIO"),
            "the refusal must say which plausible errno it is declining to invent: {why}"
        );

        // POSIX's own zero: `write(fd, "", 0)` returns zero and that is success. The stream layer
        // never asks for one, and a rule that refused it would be wrong about the standard.
        assert!(write_contract_violation(7, 0, 0).is_none(), "an empty write is not a failure");

        // An ordinary short write, which is what `read(2)`/`write(2)` semantics are for: the
        // layer above loops on it. Refusing here would turn every transfer longer than one seam
        // write into a refusal.
        assert!(write_contract_violation(7, 4096, 1).is_none(), "a one-byte short write");
        assert!(write_contract_violation(7, 4096, 4095).is_none(), "an almost-complete write");
        assert!(write_contract_violation(7, 4096, 4096).is_none(), "a complete write");
    }
}
