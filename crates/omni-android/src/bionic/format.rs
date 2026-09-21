//! The `printf` family: the boundary's variadic walk feeding `omni-bionic`'s formatting core.
//!
//! # Three ways to be silently wrong, and where each is stopped
//!
//! * **A C variadic call carries no types.** The format string is the only description of its
//!   own arguments, so the arguments are fetched from the list
//!   [`omni_bionic::printf::plan`] returns — one walk of the format string, shared with
//!   `format` itself, so planner and formatter cannot disagree by one argument. Reading a
//!   `double` out of the integer bank does not fail; it returns a number.
//! * **A variadic `float` has already been promoted to `double`.** Every floating-point
//!   argument is taken with `next_f64`, never `next_f32`: D18 records that reading one back as
//!   a `float` gives exactly `0.0` for `1.0`.
//! * **A `va_list` is 32 bytes, so AAPCS64 passes it indirectly.** `vsnprintf`'s `X3` holds a
//!   *pointer* to the record, and [`GuestVaList::read`] walks the record's five fields with
//!   every one of them range-checked and every read going through [`GuestMem`].
//!
//! # `long double`, and F6
//!
//! Task 2's review found that `VarArgs` has no refusal API at all, and that a handler meeting
//! `%Lf` therefore had no correct call and no way to decline. The refusal lives in `plan`, which
//! runs **before** any argument is fetched: nothing is read out of the wrong bank and no number
//! is produced. The same is true of `%ls` and `%lc`, where the guest's `wchar_t` is 32 bits.
//!
//! # Bytes, not text
//!
//! A guest format string is bytes the guest chose and is under no obligation to be UTF-8, so it
//! is mapped one byte to one `char` (Latin-1) rather than validated, which is exactly what the
//! formatting core does with the literal bytes it copies. The result is mapped back the same
//! way. A character above `U+00FF` cannot appear — nothing in the pipeline produces one — and if
//! one ever did it would be a refusal rather than a substituted byte.

use omni_bionic::printf::{format as format_core, plan, ArgKind, FormatArg};

use crate::boundary::ImportCall;
use crate::error::{AbiError, AbiResult};
use crate::mem::Blame;
use crate::varargs::{GuestVaList, VarArgs};

use super::view::GuestView;
use super::{active, enter};

/// Where the variadic arguments come from: registers-and-stack, or a guest `va_list`.
///
/// The two walks are genuinely different — one starts where the named arguments stopped, the
/// other reads a record the *callee* filled in — but a `printf` implementation should not care,
/// and duplicating the collection loop for each would be two places for the bank rules to drift.
pub(super) trait VaSource {
    /// The next 64-bit integer or pointer.
    fn next_u64(&mut self) -> AbiResult<u64>;
    /// The next `double`. A variadic `float` was promoted by the caller, so there is no
    /// `next_f32` here on purpose.
    fn next_f64(&mut self) -> AbiResult<f64>;
}

impl VaSource for VarArgs<'_> {
    fn next_u64(&mut self) -> AbiResult<u64> {
        VarArgs::next_u64(self)
    }
    fn next_f64(&mut self) -> AbiResult<f64> {
        VarArgs::next_f64(self)
    }
}

impl VaSource for GuestVaList<'_> {
    fn next_u64(&mut self) -> AbiResult<u64> {
        GuestVaList::next_u64(self)
    }
    fn next_f64(&mut self) -> AbiResult<f64> {
        GuestVaList::next_f64(self)
    }
}

/// One fetched argument, owning any string it names.
///
/// Owned because [`FormatArg::Str`] borrows, and the bytes have to be copied out of guest memory
/// before they can be borrowed from: forming a `&str` into guest memory would be a shared
/// reference to bytes another guest thread may be writing.
enum Owned {
    Int(i64),
    UInt(u64),
    Ptr(u64),
    Double(f64),
    Str(String),
    /// A null `%s` argument. bionic prints `(null)`, which the core does for `Ptr(0)`.
    NullStr,
}

impl Owned {
    fn as_arg(&self) -> FormatArg<'_> {
        match self {
            Owned::Int(n) => FormatArg::Int(*n),
            Owned::UInt(n) => FormatArg::UInt(*n),
            Owned::Ptr(p) => FormatArg::Ptr(*p),
            Owned::Double(d) => FormatArg::Double(*d),
            Owned::Str(s) => FormatArg::Str(s),
            Owned::NullStr => FormatArg::Ptr(0),
        }
    }
}

/// Read a guest C string as one byte per `char`.
fn read_latin1(view: &GuestView<'_>, at: u64, argument: usize) -> AbiResult<String> {
    let address = usize::try_from(at)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
    let bytes = view.mem().cstr(address, Blame::new(view.symbol(), view.address(), argument))?;
    Ok(bytes.iter().map(|&b| b as char).collect())
}

/// Map a formatted result back to bytes, one `char` to one byte.
pub(super) fn to_bytes(view: &GuestView<'_>, text: &str) -> AbiResult<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    for ch in text.chars() {
        let code = ch as u32;
        if code > 0xFF {
            // Unreachable: every byte the core copies comes back as a `char` below 0x100 and
            // every conversion it produces is ASCII. Refused rather than substituted, because a
            // substituted byte is a plausible wrong answer in a string the guest will read.
            return Err(view.refusal(format!(
                "the formatted result contains U+{code:04X}, which is not one guest byte"
            )));
        }
        out.push(code as u8);
    }
    Ok(out)
}

/// Fetch every argument the format string asks for, in order, from `source`.
fn collect(
    view: &GuestView<'_>,
    fmt: &str,
    source: &mut dyn VaSource,
    first_variadic: usize,
) -> AbiResult<Vec<Owned>> {
    let kinds = plan(fmt).map_err(|error| view.refusal(error.to_string()))?;
    let mut owned = Vec::with_capacity(kinds.len());
    for (index, kind) in kinds.iter().enumerate() {
        let argument = first_variadic + index;
        owned.push(match kind {
            // A variadic `int` occupies a whole 64-bit slot and the caller is not required to
            // clear the high half, so it is taken as 64 bits and narrowed here.
            ArgKind::Int => Owned::Int(source.next_u64()? as u32 as i32 as i64),
            ArgKind::UInt => Owned::UInt(source.next_u64()?),
            ArgKind::Ptr => Owned::Ptr(source.next_u64()?),
            ArgKind::Double => Owned::Double(source.next_f64()?),
            ArgKind::Str => {
                let pointer = source.next_u64()?;
                if pointer == 0 {
                    Owned::NullStr
                } else {
                    Owned::Str(read_latin1(view.blaming(argument), pointer, argument)?)
                }
            }
        });
    }
    Ok(owned)
}

/// Read the format string, fetch the arguments, and format.
///
/// `pub(super)` because `liblog` needs it: `__android_log_print` and `syslog` are variadic
/// `printf` calls whose *destination* is a log sink rather than a buffer, and giving them a second
/// copy of the argument walk would be two places for the AAPCS64 variadic bank rules to drift.
pub(super) fn render(
    view: &GuestView<'_>,
    fmt_ptr: u64,
    fmt_argument: usize,
    source: &mut dyn VaSource,
) -> AbiResult<String> {
    if fmt_ptr == 0 {
        // bionic's own `printf` crashes on a null format. A refusal naming the symbol is the
        // only other honest answer; printing the literal "(null)" would be an invention.
        return Err(view.refusal("a null format string"));
    }
    let fmt = read_latin1(view.blaming(fmt_argument), fmt_ptr, fmt_argument)?;
    let owned = collect(view, &fmt, source, fmt_argument + 1)?;
    let args: Vec<FormatArg<'_>> = owned.iter().map(Owned::as_arg).collect();
    let mut out = String::new();
    format_core(&fmt, &args, &mut out).map_err(|error| view.refusal(error.to_string()))?;
    Ok(out)
}

/// Write an `snprintf`-style truncated result, and return what the full length was.
///
/// C's rule, which is the one that surprises people: the return value is what *would* have been
/// written, not what was. A handler returning the truncated length makes every caller that grows
/// its buffer on overflow loop forever.
fn write_truncated(
    view: &GuestView<'_>,
    destination: u64,
    capacity: u64,
    text: &str,
    argument: usize,
) -> AbiResult<i32> {
    let bytes = to_bytes(view, text)?;
    let full = i32::try_from(bytes.len())
        .map_err(|_| view.refusal("a formatted result longer than an int can report"))?;
    if capacity == 0 {
        // `snprintf(NULL, 0, ...)` is the documented way to ask how long the result would be,
        // and it must not touch the destination at all.
        return Ok(full);
    }
    let at = usize::try_from(destination)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
    // `capacity - 1` bytes plus the NUL, which is the whole of snprintf's truncation rule.
    let room = usize::try_from(capacity - 1).unwrap_or(usize::MAX).min(bytes.len());
    let mut write = Vec::with_capacity(room + 1);
    write.extend_from_slice(&bytes[..room]);
    write.push(0);
    view.mem().write_bytes(at, &write, Blame::new(view.symbol(), view.address(), argument))?;
    Ok(full)
}

/// Read the named arguments common to the `v*` forms, and the `va_list` pointer.
struct NamedThenVaList {
    destination: u64,
    capacity: u64,
    fmt: u64,
    va_list: u64,
}

/// `int snprintf(char *s, size_t n, const char *fmt, ...)`
pub(super) fn snprintf(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let state = active(c.symbol(), c.address())?;
    let written = {
        let (destination, capacity, fmt, consumed, overflow) = {
            let mut a = c.args();
            let destination = a.next_u64()?;
            let capacity = a.next_u64()?;
            let fmt = a.next_u64()?;
            // Taken from the cursor that read the named arguments rather than counted by hand,
            // so the split point cannot be got wrong by miscounting parameters.
            (destination, capacity, fmt, a.consumed(), a.overflow())
        };
        let mut source = c.varargs(consumed, overflow, 3);
        let view = enter(c, &state);
        let text = render(&view, fmt, 2, &mut source)?;
        write_truncated(&view, destination, capacity, &text, 0)?
    };
    c.ret().i32(written);
    Ok(())
}

/// `int vsnprintf(char *s, size_t n, const char *fmt, va_list ap)`
pub(super) fn vsnprintf(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let state = active(c.symbol(), c.address())?;
    let named = {
        let mut a = c.args();
        NamedThenVaList {
            destination: a.next_u64()?,
            capacity: a.next_u64()?,
            fmt: a.next_u64()?,
            // `X3` holds a *pointer* to the 32-byte record, not the record.
            va_list: a.next_u64()?,
        }
    };
    let written = {
        let view = enter(c, &state);
        let at = usize::try_from(named.va_list)
            .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
        let mut source =
            GuestVaList::read(view.mem(), at, Blame::new(view.symbol(), view.address(), 3))?;
        let text = render(&view, named.fmt, 2, &mut source)?;
        write_truncated(&view, named.destination, named.capacity, &text, 0)?
    };
    c.ret().i32(written);
    Ok(())
}

/// `int __vsnprintf_chk(char *dest, size_t supplied_size, int flags, size_t dest_len,
/// const char *fmt, va_list ap)`
///
/// The FORTIFY form. `dest_len` is what the compiler could prove about the destination and
/// `supplied_size` is what the caller passed; `supplied_size > dest_len` is a **detected buffer
/// overflow in guest code**, which bionic answers with `__fortify_fatal`. Reported as a refusal
/// naming both sizes, never formatted into the shorter buffer.
pub(super) fn vsnprintf_chk(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (destination, supplied_size, flags, dest_len, fmt, va_list) = {
        let mut a = c.args();
        (
            a.next_u64()?,
            a.next_u64()?,
            a.next_i32()?,
            a.next_u64()?,
            a.next_u64()?,
            a.next_u64()?,
        )
    };
    let _ = flags;
    let state = active(c.symbol(), c.address())?;
    let written = {
        let view = enter(c, &state);
        if supplied_size > dest_len {
            return Err(view.refusal(format!(
                "FORTIFY: the caller passed a size of {supplied_size} for a destination the \
                 compiler proved is {dest_len} bytes -- a buffer overflow in guest code, which \
                 bionic answers with __fortify_fatal"
            )));
        }
        let at = usize::try_from(va_list)
            .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
        let mut source =
            GuestVaList::read(view.mem(), at, Blame::new(view.symbol(), view.address(), 5))?;
        let text = render(&view, fmt, 4, &mut source)?;
        write_truncated(&view, destination, supplied_size, &text, 0)?
    };
    c.ret().i32(written);
    Ok(())
}

/// Refuse a symbol this phase binds but cannot service, naming what is missing.
///
/// Bound rather than left [`Unbound`](crate::Binding::Unbound) because `Unbound` says only "the
/// compatibility layer does not implement this", and for these five the useful thing to say is
/// *which* missing piece and where it will come from. The refusal happens before any argument is
/// read: doing the formatting work and then discarding it would add nothing but a second way to
/// fail.
fn refuse(c: &mut ImportCall<'_, '_>, why: &str) -> AbiResult<()> {
    Err(AbiError::Refused {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: why.to_string(),
    })
}

/// `int fprintf(FILE *stream, const char *fmt, ...)`
pub(super) fn fprintf(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let text = {
        let state = active(c.symbol(), c.address())?;
        let (fmt, consumed, overflow) = {
            let mut a = c.args();
            let _stream = a.next_u64()?;
            let fmt = a.next_u64()?;
            (fmt, a.consumed(), a.overflow())
        };
        let mut source = c.varargs(consumed, overflow, 2);
        let view = enter(c, &state);
        render(&view, fmt, 1, &mut source)?
    };
    let stream = c.args().next_u64()?;
    let written = super::stdio::print_to_stream(c, stream, &text)?;
    c.ret().i32(written);
    Ok(())
}

/// `int vfprintf(FILE *stream, const char *fmt, va_list ap)` — as [`fprintf`].
pub(super) fn vfprintf(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (stream, fmt, va_list) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let text = {
        let state = active(c.symbol(), c.address())?;
        let view = enter(c, &state);
        let at = usize::try_from(va_list)
            .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
        let mut source =
            GuestVaList::read(view.mem(), at, Blame::new(view.symbol(), view.address(), 2))?;
        render(&view, fmt, 1, &mut source)?
    };
    let written = super::stdio::print_to_stream(c, stream, &text)?;
    c.ret().i32(written);
    Ok(())
}

/// `int vasprintf(char **strp, const char *fmt, va_list ap)`
///
/// The result has to be `malloc`ed in the **guest's** heap, and `libroblox.so` imports no
/// allocator at all — it carries its own and reaches the host through guest `mmap`. There is no
/// guest allocator for a host handler to call, and mapping guest memory from inside a handler is
/// what task 2's review F9 warns about: `ImportCall::mem()` reaches the whole `GuestSpace`, and a
/// map performed while generated code is live breaks the pager's own invariant.
pub(super) fn vasprintf(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    refuse(
        c,
        "the result must be allocated in the guest's heap, and libroblox.so imports no \
         allocator: the heap seam is guest mmap through the demand pager, which a host handler \
         has no way to call",
    )
}

/// `int sscanf(const char *s, const char *fmt, ...)`
///
/// There is no scanning engine. `omni-bionic`'s `printf` module formats and does not parse, and
/// the conversions `sscanf` needs are not the ones `strtol` and `strtod` provide.
pub(super) fn sscanf(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    refuse(
        c,
        "there is no scanf conversion engine: omni-bionic's printf module formats and does not \
         parse, and every plausible partial answer would write a wrong value through the \
         guest's output pointers",
    )
}

/// `int fscanf(FILE *stream, const char *fmt, ...)` — no scanning engine *and* no `FILE *`.
pub(super) fn fscanf(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    refuse(
        c,
        "there is no scanf conversion engine, and reading from a guest FILE * needs host file \
         surface that omni-platform does not have yet",
    )
}
