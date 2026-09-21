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
//! # Unbuffered, which is what makes `fflush` honest
//!
//! [`omni_bionic::stdio`] has the argument. The short version: every write reaches the descriptor
//! before the call returns, so `fflush` has nothing of this layer's to flush and succeeding is
//! the contract being satisfied rather than a stub. `fflush` on a *standard* stream still reaches
//! the host's own `Stdout`, which does buffer.

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

use super::files::{errno_for, filesystem};
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
pub(super) struct HostDescriptors<'a> {
    fs: &'a Filesystem,
    unclassified: RefCell<Option<FsError>>,
}

impl<'a> HostDescriptors<'a> {
    fn new(fs: &'a Filesystem) -> Self {
        HostDescriptors { fs, unclassified: RefCell::new(None) }
    }

    /// Turn a seam failure into an errno, stashing one that has none.
    fn errno(&self, error: FsError) -> i32 {
        match error.kind().and_then(errno_for) {
            Some(errno) => errno,
            None => {
                let mut slot = self.unclassified.borrow_mut();
                if slot.is_none() {
                    *slot = Some(error);
                }
                // The stream logic needs *some* number to unwind with. It never reaches the
                // guest: the handler checks the stash first and refuses.
                consts::EIO
            }
        }
    }

    /// The stashed failure, if the call hit one.
    fn take_unclassified(&self) -> Option<FsError> {
        self.unclassified.borrow_mut().take()
    }
}

impl Descriptors for HostDescriptors<'_> {
    fn read(&self, fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
        self.fs.read(fd, buf).map_err(|error| self.errno(error))
    }

    fn write(&self, fd: i32, buf: &[u8]) -> Result<usize, i32> {
        self.fs.write(fd, buf).map_err(|error| self.errno(error))
    }

    fn flush(&self, fd: i32) -> Result<(), i32> {
        self.fs.flush(fd).map_err(|error| self.errno(error))
    }
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
    if let Some(error) = descriptors.take_unclassified() {
        return Err(view.refusal(error.to_string()));
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
        if let Some(error) = descriptors.take_unclassified() {
            return Err(view.refusal(error.to_string()));
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

/// `char *fgets(char *s, int size, FILE *stream)`
pub(super) fn fgets(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (s, size, file) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_i32()?, a.next_u64()?)
    };
    let result = with_stream(c, file, |view, descriptors, stream| {
        view.blaming(0);
        let produced = stdio::fgets(view, descriptors, stream, s, size);
        lift(view, produced)
    })?;
    c.ret().u64(result);
    Ok(())
}

/// `int fputs(const char *s, FILE *stream)`
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
pub(super) fn fread(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (ptr, size, nmemb, file) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let result = with_stream(c, file, |view, descriptors, stream| {
        view.blaming(0);
        let produced = stdio::fread(view, descriptors, stream, ptr, size, nmemb);
        lift(view, produced)
    })?;
    c.ret().u64(result);
    Ok(())
}

/// `size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream)`
pub(super) fn fwrite(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (ptr, size, nmemb, file) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let result = with_stream(c, file, |view, descriptors, stream| {
        view.blaming(0);
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
}
