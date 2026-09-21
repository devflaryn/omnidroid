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

/// The longest `fopen` mode string this layer will read.
///
/// `"rb+xe"` is five; sixteen is generous and is the bound that stops a guest handing a 64 KiB
/// "mode" string. A longer one is `EINVAL`, which is what `fopen` reports for a mode it cannot
/// parse anyway.
const MAX_MODE_BYTES: usize = 16;

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

/// What a `fopen` mode string asked for.
///
/// # The modes, and what each modifier does here
///
/// | mode | access |
/// |---|---|
/// | `r` | read |
/// | `r+` | read and write |
/// | `w` | write, create, truncate |
/// | `w+` | read and write, create, truncate |
/// | `a` | write, create, append |
/// | `a+` | read and write, create, append |
///
/// `b` is accepted and does nothing, which is correct rather than lazy: POSIX says the binary
/// modifier has no effect, and `std::fs` performs no line-ending translation on any target, so a
/// stream opened without `b` is already binary. That matters here more than on a real device —
/// a Windows host is where a text mode would otherwise appear — and it is why nothing in this
/// layer ever touches a `\r`.
///
/// `e` (bionic's `O_CLOEXEC` modifier) is accepted and does nothing, because nothing here execs.
/// `x` is `O_EXCL` and is honoured with `w`. Any other character is `EINVAL`.
fn parse_mode(mode: &[u8]) -> Option<OpenFlags> {
    let first = *mode.first()?;
    let plus = mode.contains(&b'+');
    let exclusive = mode.contains(&b'x');
    for byte in &mode[1..] {
        if !matches!(byte, b'+' | b'b' | b'e' | b'x') {
            return None;
        }
    }
    let flags = match first {
        b'r' => OpenFlags { read: true, write: plus, ..OpenFlags::default() },
        b'w' => OpenFlags {
            read: plus,
            write: true,
            create: true,
            truncate: true,
            exclusive,
            ..OpenFlags::default()
        },
        b'a' => OpenFlags {
            read: plus,
            write: true,
            create: true,
            append: true,
            exclusive,
            ..OpenFlags::default()
        },
        _ => return None,
    };
    Some(flags)
}

/// Read a `fopen` mode string, bounded.
fn mode_argument(view: &GuestView<'_>, pointer: u64, argument: usize) -> AbiResult<Vec<u8>> {
    if pointer == 0 {
        return Err(view.refusal(format!("argument {argument} is a null mode string")));
    }
    let at = GuestAddr::try_from(pointer)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
    let bytes = view.mem().cstr(at, Blame::new(view.symbol(), view.address(), argument))?;
    Ok(bytes.into_iter().take(MAX_MODE_BYTES).collect())
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
        let Some(flags) = parse_mode(&mode_bytes) else {
            view.set_errno(consts::EINVAL);
            c.ret().u64(0);
            return Ok(());
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
        if parse_mode(&mode_bytes).is_none() {
            view.set_errno(consts::EINVAL);
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
/// Closes the descriptor and releases the `FILE` object's slot. Returns 0, or `EOF` with `errno`
/// if the descriptor could not be closed.
///
/// The slot is released **whatever the descriptor did**, which is C's own rule: after `fclose`
/// the stream may not be used again even if it reported a failure. Keeping the slot on a failed
/// close would leak one per failure and let a guest exhaust the table.
pub(super) fn fclose(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let file = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let stream = stream_of(&view, file)?;
        let fs = filesystem(&view)?;
        state.bionic.close_stream(file);
        match super::files::settle(&view, fs.close(stream.fd))? {
            super::files::Settled::Done(()) => 0,
            super::files::Settled::Failed(errno) => {
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

    /// Every `fopen` mode C defines, and the modifiers, and the ones that are `EINVAL`.
    #[test]
    fn the_fopen_modes_are_the_six_c_defines_plus_the_modifiers() {
        let read = parse_mode(b"r").expect("r");
        assert_eq!((read.read, read.write, read.create), (true, false, false));
        let update = parse_mode(b"r+").expect("r+");
        assert_eq!((update.read, update.write, update.create), (true, true, false));
        let write = parse_mode(b"w").expect("w");
        assert_eq!(
            (write.read, write.write, write.create, write.truncate, write.append),
            (false, true, true, true, false)
        );
        let write_update = parse_mode(b"w+").expect("w+");
        assert_eq!((write_update.read, write_update.truncate), (true, true));
        let append = parse_mode(b"a").expect("a");
        assert_eq!(
            (append.read, append.write, append.create, append.append, append.truncate),
            (false, true, true, true, false)
        );
        assert!(parse_mode(b"a+").expect("a+").read);
        // `b` is accepted and changes nothing, which is what POSIX says it does -- and matters
        // here because a Windows host is exactly where a text mode would otherwise appear.
        assert_eq!(parse_mode(b"rb"), parse_mode(b"r"));
        assert_eq!(parse_mode(b"wb+"), parse_mode(b"w+"));
        assert_eq!(parse_mode(b"rbe"), parse_mode(b"r"), "bionic's O_CLOEXEC modifier");
        // `x` is O_EXCL, and only with a creating mode.
        assert!(parse_mode(b"wx").expect("wx").exclusive);
        assert!(!parse_mode(b"r").expect("r").exclusive);
        // Everything else is EINVAL rather than a guess.
        for bad in [&b""[..], b"z", b"+", b"rz", b"w!", b"R", b"rw"] {
            assert_eq!(parse_mode(bad), None, "`{}` was accepted", String::from_utf8_lossy(bad));
        }
    }

    /// The mode bound is a constant, so a 64 KiB "mode" string cannot be walked.
    #[test]
    fn the_mode_string_is_bounded() {
        assert_eq!(MAX_MODE_BYTES, 16);
        // A long mode that starts legally is still rejected, because the characters past the
        // first are checked and a `y` is not a modifier.
        assert_eq!(parse_mode(b"rbbbbbbbbbbbbbby"), None);
    }
}
