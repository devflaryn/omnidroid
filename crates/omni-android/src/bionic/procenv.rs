//! Process and environment: fourteen symbols. Four are answers (`getpid`, `sched_getcpu`,
//! `arc4random_buf`, `getauxval`), two are facts about a process that was given nothing (`getenv`,
//! `__system_property_get`), three are terminations (`abort`, `__stack_chk_fail`, `_exit`) with a
//! fourth capturing the message that explains one (`android_set_abort_message`). `sysconf` and
//! `prctl` answer what they can and refuse the rest by name; `sysinfo` and `syscall` refuse
//! entirely. 4 + 2 + 3 + 1 + 4 = 14.
//!
//! # `AT_HWCAP` IS AN OPEN DECISION AND THIS MODULE DOES NOT MAKE IT
//!
//! `getauxval(AT_HWCAP)` is where the LSE question lands, and that question is **unresolved**. Both
//! arms are measured and both are bad:
//!
//! | arm | measured consequence |
//! |---|---|
//! | Advertise `HWCAP_ATOMICS` | **53 hard interpreter halts** — the LSE instructions the guest then emits are ones this pin cannot translate |
//! | Decline it | **106 fallback arms** into a global spinlock that **anti-scales 21x** |
//!
//! Neither number is an estimate and neither arm is a default. So the default here is
//! [`HwcapPolicy::Undecided`], and under it `getauxval(AT_HWCAP)` **refuses by name**, with both
//! measurements in the refusal. A host that has made the decision says so explicitly with
//! [`Bionic::set_hwcap_policy`](super::Bionic::set_hwcap_policy), and the fact that it had to call
//! something is the point: a decision this expensive must not be reachable by an `#[derive(Default)]`.
//!
//! The obvious alternative — default to declining, since declining cannot halt the interpreter —
//! was considered and rejected. It *reads* as the safe arm and it is not: it is the arm that costs
//! 21x on a machine with cores, it would be measured by whoever ran the engine next and reported
//! as "the runtime is slow", and nothing in the report would say a decision had been made. A
//! refusal that names both arms cannot be mistaken for anything.
//!
//! # The environment and the property table are empty **facts**, not stubs
//!
//! `getenv` returns `NULL` for every name and `__system_property_get` returns 0 with an empty
//! string, because this guest process was started with no environment and no Android property
//! service — which is true, and is the same fact `environ` already states as one of the eighteen
//! data objects (it points at a vector of one null). They are host-settable
//! ([`Bionic::set_env`](super::Bionic::set_env),
//! [`Bionic::set_system_property`](super::Bionic::set_system_property)) so that "empty" is a
//! *configuration* rather than an absence, and so that the code path which finds a value is
//! exercised rather than dead.
//!
//! **The host's own environment is deliberately not reachable from here.** Handing the guest the
//! variables this process was started with would be a wrong answer — a desktop shell's environment
//! is not an Android app's — and an information leak of everything in it. `omni-platform`'s
//! process seam has no `host_environment()` for the same reason.
//!
//! # `sysconf` answers four names now, and the evidence came from the guest
//!
//! Phase 3a refused every `sysconf` name, including the two this layer knows — the page size and
//! the processor count — because **bionic's `_SC_*` numbering is bionic's own**, there is no NDK
//! on this machine, and a number derived from memory has the wrong failure mode: if the constant
//! were wrong, the real page-size query would arrive as an unmodelled number and be refused
//! loudly, while *some other* `_SC_` name would silently receive a page size. D22 recorded that
//! the missing thing was four confirmed constants, and that running M3's gate would report which
//! numbers the engine actually passes.
//!
//! It did, and the answer is better than a count: `tools/call_sites.py` decodes every **direct**
//! `sysconf` call site in `libroblox.so` from the guest's own instructions, and the arguments are
//!
//! | value | sites | believed name |
//! |---:|---:|---|
//! | 39 | 11 | `_SC_PAGESIZE` |
//! | 96 | 6 | `_SC_NPROCESSORS_CONF` |
//! | 97 | 4 | `_SC_NPROCESSORS_ONLN` |
//! | 40 | 2 | `_SC_PAGE_SIZE` |
//! | 6, 11, 38, 98 | 1 each | `_SC_CLK_TCK`, `_SC_OPEN_MAX`, `_SC_IOV_MAX`, `_SC_PHYS_PAGES` |
//!
//! (26 direct sites, one of which loads its argument from memory and is therefore unknown.)
//!
//! **That multiset is the corroboration the header would have been.** Bionic is the one libc
//! where `_SC_PAGESIZE` and `_SC_PAGE_SIZE` are two *different* values rather than a macro and its
//! alias — glibc has one value, 30, for both — and this binary asks for **both 39 and 40**,
//! adjacent, in a program that plainly wants a page size. The same shape holds for the processor
//! count: 96, 97 and 98 are asked adjacently, which is `_SC_NPROCESSORS_CONF`, `_SC_NPROCESSORS_ONLN`
//! and `_SC_PHYS_PAGES` in the believed numbering. A numbering that was wrong would not produce
//! adjacent pairs and triples landing exactly where the table says two spellings of one quantity
//! live; it would scatter.
//!
//! So the page size and the processor count are answered, and everything else still refuses by
//! name. **What would falsify this** is a real bionic `<bits/sysconf.h>`, and it is the cheapest
//! check anyone with an NDK can run.
//!
//! `_SC_PHYS_PAGES` is deliberately *not* answered even though a number is available: it is the
//! host's physical memory, which is not the guest's budget, and it is the same wrong answer
//! `sysinfo` refuses for. `_SC_CLK_TCK`, `_SC_OPEN_MAX` and `_SC_IOV_MAX` are refused because each
//! has a believable wrong answer and nothing here measures the right one.
//!
//! # `syscall` refuses; `sysinfo` and `prctl` answer what this runtime actually knows
//!
//! `syscall` is refused because a raw syscall number is a contract this layer does not model.
//! `sysinfo` describes the guest's world rather than a machine — every field is a fact about this
//! instance once the embedding has said how much memory the guest has, and its documentation
//! names the one field that is not. `prctl` answers `PR_SET_VMA` and refuses the rest.

use std::time::Duration;

use omni_bionic::context::GuestContext;
use omni_mem::GuestAddr;

use crate::boundary::ImportCall;
use crate::error::{AbiError, AbiResult};
use crate::mem::Blame;

use super::view::GuestView;
use super::{active, enter};

/// Bytes of a value `__system_property_get` may write, **including** the terminating NUL.
///
/// `PROP_VALUE_MAX` from bionic's `<sys/system_properties.h>`. It is part of the published Android
/// ABI rather than an internal constant — callers declare `char value[PROP_VALUE_MAX]` and pass
/// it, so writing one byte more is a stack overflow **in guest code** caused by this layer.
pub const PROP_VALUE_MAX: usize = 92;

// ------------------------------------------------------------------ the auxiliary vector
//
// `AT_*` are Linux UAPI (`include/uapi/linux/auxvec.h`), stable across every architecture and the
// same source `omni-bionic`'s errno numbers come from. Unlike bionic's private `_SC_*` numbering,
// these can be stated.

/// `AT_PAGESZ`: the page size the kernel is using.
const AT_PAGESZ: u64 = 6;
/// `AT_HWCAP`: the first word of architecture feature flags.
const AT_HWCAP: u64 = 16;
/// `AT_HWCAP2`: the second word.
const AT_HWCAP2: u64 = 26;

/// `HWCAP_ATOMICS`, bit 8 of `AT_HWCAP` on AArch64: the **LSE** atomic instructions.
///
/// This single bit is the whole of the open decision described in this module's documentation.
/// `libroblox.so` has 128 exclusive-monitor sites against 53 LSE ones (29.3% of its atomic
/// read-modify-writes), and which of those two code paths the engine takes is decided by whether
/// this bit is set in the value `getauxval(AT_HWCAP)` returns.
pub const HWCAP_ATOMICS: u64 = 1 << 8;

/// What `getauxval(AT_HWCAP)` and `getauxval(AT_HWCAP2)` should answer.
///
/// **There is no `Default` implementation on purpose.** [`HwcapPolicy::Undecided`] is what
/// [`Bionic`](super::Bionic) starts with and it is spelled out at the construction site, so that
/// the value cannot arrive by way of a derive that nobody read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwcapPolicy {
    /// **No decision has been made, and `getauxval(AT_HWCAP)` refuses.**
    ///
    /// The refusal carries both measured arms. This is what an instance starts with.
    Undecided,

    /// Advertise exactly these two words.
    ///
    /// The host has made the decision and says which. `HWCAP_ATOMICS` set means the engine will
    /// emit LSE; see the module documentation for what that costs on this pin.
    Advertise {
        /// The value for `AT_HWCAP`.
        hwcap: u64,
        /// The value for `AT_HWCAP2`.
        hwcap2: u64,
    },

    /// Advertise nothing: both words are zero.
    ///
    /// A legitimate configuration — it is what a very old ARMv8.0 device reports — and an explicit
    /// choice of the 106-fallback-arms arm. It is **not** the same as [`HwcapPolicy::Undecided`],
    /// and keeping them distinct is the point of the type.
    Decline,
}

/// Narrow a guest pointer to a host address, refusing rather than truncating.
fn guest_address(view: &GuestView<'_>, pointer: u64) -> AbiResult<GuestAddr> {
    GuestAddr::try_from(pointer)
        .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))
}

/// Refuse a call, naming the symbol and the guest address.
fn refuse(c: &ImportCall<'_, '_>, why: String) -> AbiError {
    AbiError::Refused { symbol: c.symbol().to_string(), address: c.address(), why }
}

// ================================================================== the answers

/// `pid_t getpid(void)`
///
/// The **host** process id, truthfully: several guest instances share one host process exactly as
/// several threads of an Android process share one pid. A per-instance invented pid would be a
/// number with nothing behind it.
///
/// A pid that does not fit `pid_t` (an `int`) is refused rather than truncated. Unreachable on
/// Windows in practice; checked because a truncating cast on an identifier produces a *different
/// valid-looking identifier*.
pub(super) fn getpid(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let pid = omni_platform::process::pid();
    let Ok(narrowed) = i32::try_from(pid) else {
        return Err(refuse(
            c,
            format!(
                "the host process id is {pid}, which does not fit the guest's `pid_t` (an int). \
                 Truncating it would hand the guest a different, valid-looking process id"
            ),
        ));
    };
    c.ret().i32(narrowed);
    Ok(())
}

/// `int sched_getcpu(void)`
///
/// Answered on Windows from `GetCurrentProcessorNumber`. On a target whose process backend is
/// structural the platform error is reported as a refusal naming the POSIX call it wanted — never
/// as `-1`, which is a *documented* `sched_getcpu` failure that callers handle by falling back to
/// a hash, and which would therefore hide the gap completely.
pub(super) fn sched_getcpu(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    match omni_platform::process::current_cpu() {
        Ok(cpu) => {
            // The processor number is within the calling thread's group and is bounded by 63
            // there, so this cannot truncate; written as a checked narrowing anyway because the
            // value comes from outside this crate.
            let Ok(narrowed) = i32::try_from(cpu) else {
                return Err(refuse(
                    c,
                    format!("the host reports processor number {cpu}, which does not fit an int"),
                ));
            };
            c.ret().i32(narrowed);
            Ok(())
        }
        Err(error) => Err(refuse(
            c,
            format!(
                "the host could not report which processor this thread is on: {error}. Reporting \
                 -1 would be indistinguishable from the documented failure that callers handle by \
                 hashing the thread id instead, so the gap would never be seen"
            ),
        )),
    }
}

/// `void arc4random_buf(void *buf, size_t n)`
///
/// Real entropy from the platform seam. **No pseudo-random fallback**: the contract of
/// `arc4random_buf` is that the bytes are unpredictable, and a caller using it to seed an address
/// randomisation or a nonce gets no warning that they were not. A host entropy source that fails
/// is a refusal.
///
/// `n == 0` writes nothing and is legal C. A huge `n` is bounded twice: the whole destination is
/// validated against guest memory before a single byte is generated, and the host-side buffer is
/// [`ENTROPY_CHUNK`] rather than `n`, so a guest asking for a terabyte cannot make this layer
/// allocate one.
pub(super) fn arc4random_buf(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    /// Bytes generated host-side at a time. A guest-chosen length must never become a host-side
    /// allocation of that length.
    const ENTROPY_CHUNK: usize = 4096;

    let (buf, n) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    {
        let view = enter(c, &state);
        let len = usize::try_from(n)
            .map_err(|_| view.refusal("a length wider than the host's usize"))?;
        if len > 0 {
            let at = guest_address(view.blaming(0), buf)?;
            let blame = Blame::new(view.symbol(), view.address(), 0);
            // Validate the whole destination first. Without this, a request that is writable for
            // its first page and not its second would leave real entropy in the first page and a
            // reported failure — and the caller would have no way to know which half it got.
            view.mem().checked_ptr(at, len, true, blame)?;
            let mut scratch = [0u8; ENTROPY_CHUNK];
            let mut written = 0usize;
            while written < len {
                let take = ENTROPY_CHUNK.min(len - written);
                omni_platform::process::random_bytes(&mut scratch[..take]).map_err(|error| {
                    view.refusal(format!(
                        "the host entropy source failed after {written} of {len} bytes: {error}. \
                         Filling the rest from a pseudo-random sequence would satisfy every test \
                         that checked the buffer had changed and would not be entropy"
                    ))
                })?;
                view.mem().write_bytes(at + written, &scratch[..take], blame)?;
                written += take;
            }
        }
    }
    c.ret().void();
    Ok(())
}

/// `char *getenv(const char *name)`
///
/// Returns a guest pointer to the **value**, interned in the adapter's pool when the host set it,
/// or `NULL`. The pointer is stable for the life of the instance, which is what `getenv`'s
/// contract requires — a caller may hold it indefinitely.
///
/// A null `name` returns `NULL`. `getenv(NULL)` is undefined behaviour in C and bionic dereferences
/// it; `NULL` is the one answer that cannot be mistaken for a value. A name containing `=`, or an
/// empty name, also returns `NULL`: POSIX leaves both undefined and neither can name a variable.
pub(super) fn getenv(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let name = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let found = {
        let view = enter(c, &state);
        if name == 0 {
            0
        } else {
            let at = guest_address(view.blaming(0), name)?;
            let bytes =
                view.mem().cstr(at, Blame::new(view.symbol(), view.address(), 0))?;
            if bytes.is_empty() || bytes.contains(&b'=') {
                0
            } else {
                state.bionic.lookup_env(&bytes).unwrap_or(0) as u64
            }
        }
    };
    c.ret().u64(found);
    Ok(())
}

/// `int __system_property_get(const char *name, char *value)`
///
/// Writes the property's value plus a NUL into `value` and returns its length, or writes a single
/// NUL and returns 0 when the property is not set — which is what bionic does and what is **true**
/// of a process with no property service behind it.
///
/// `value` is a `char[PROP_VALUE_MAX]` in the caller: bionic's own implementation writes up to
/// [`PROP_VALUE_MAX`] bytes there and callers size the buffer from the same constant, so a value
/// longer than that is rejected when the host *sets* it rather than truncated when the guest reads
/// it. A truncated property value is a believable wrong answer.
pub(super) fn system_property_get(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (name, value) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let length = {
        let view = enter(c, &state);
        let found = if name == 0 {
            // bionic dereferences a null name; an empty answer is the one that cannot crash the
            // guest and cannot be mistaken for a property that exists.
            None
        } else {
            let at = guest_address(view.blaming(0), name)?;
            let bytes = view.mem().cstr(at, Blame::new(view.symbol(), view.address(), 0))?;
            state.bionic.lookup_property(&bytes)
        };
        let text = found.unwrap_or_default();
        let at = guest_address(view.blaming(1), value)?;
        let mut out = Vec::with_capacity(text.len() + 1);
        out.extend_from_slice(text.as_bytes());
        out.push(0);
        view.mem().write_bytes(at, &out, Blame::new(view.symbol(), view.address(), 1))?;
        // The return value is the length **without** the NUL, which is what bionic returns.
        i32::try_from(text.len()).map_err(|_| {
            view.refusal("a property value longer than an int can report")
        })?
    };
    c.ret().i32(length);
    Ok(())
}

/// `unsigned long getauxval(unsigned long type)`
///
/// `AT_PAGESZ` is answered from the guest address space's own page size, which is the granularity
/// its `mmap` really operates at. `AT_HWCAP` and `AT_HWCAP2` follow the instance's
/// [`HwcapPolicy`] and **refuse under the default**; see the module documentation, which is where
/// the open decision is written down. Every other `type` is refused by number.
///
/// `getauxval`'s documented answer for a type it does not have is `0` with `errno = ENOENT`, and
/// that is exactly the answer not given here. `AT_SECURE` answered 0 means "not a privileged
/// binary", `AT_CLKTCK` answered 0 means "zero ticks per second", and a guest cannot tell either
/// apart from "this key is absent" — one of those is a fact and the others are divisions by zero
/// waiting to happen.
pub(super) fn getauxval(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let kind = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let value = match kind {
        AT_PAGESZ => state.bionic.space_page_size() as u64,
        AT_HWCAP | AT_HWCAP2 => match state.bionic.hwcap_policy() {
            HwcapPolicy::Undecided => {
                return Err(refuse(
                    c,
                    format!(
                        "the guest asked for {}, and WHAT TO ANSWER IS AN OPEN DECISION FOR THIS \
                         MILESTONE. Both arms are measured and neither is a default: advertising \
                         HWCAP_ATOMICS ({HWCAP_ATOMICS:#x}, the LSE atomics) makes the engine emit \
                         instructions this dynarmic pin cannot translate, which is 53 hard \
                         interpreter halts; declining it sends 106 call sites down a fallback arm \
                         into a global spinlock that anti-scales 21x. A host that has made the \
                         choice states it with Bionic::set_hwcap_policy, and this refusal exists \
                         so the choice cannot be made by defaulting",
                        if kind == AT_HWCAP { "AT_HWCAP" } else { "AT_HWCAP2" }
                    ),
                ));
            }
            HwcapPolicy::Decline => 0,
            HwcapPolicy::Advertise { hwcap, hwcap2 } => {
                if kind == AT_HWCAP {
                    hwcap
                } else {
                    hwcap2
                }
            }
        },
        other => {
            return Err(refuse(
                c,
                format!(
                    "the guest asked for auxiliary vector entry {other}. This layer supplies \
                     AT_PAGESZ ({AT_PAGESZ}), AT_HWCAP ({AT_HWCAP}) and AT_HWCAP2 ({AT_HWCAP2}) \
                     and nothing else — there is no real auxv here, because there was no kernel \
                     exec to build one. `getauxval` answers an absent key with 0 and ENOENT, and \
                     that is precisely the answer withheld: a guest cannot tell a 0 that means \
                     \"absent\" from a 0 that means \"zero\""
                ),
            ));
        }
    };
    c.ret().u64(value);
    Ok(())
}

// ================================================================== the two terminations

/// `void abort(void)`
///
/// **Reported, never performed.** See
/// [`AbiError::GuestAborted`](crate::AbiError::GuestAborted): a host `abort()` cannot be contained
/// by any caller, and this runtime is required to host several isolated guest instances in one
/// process. The message the guest gave `android_set_abort_message` travels with it, because that
/// is where bionic's crash reporter gets the only human-readable account of why the process died.
pub(super) fn abort(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let state = active(c.symbol(), c.address())?;
    Err(AbiError::GuestAborted {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: "the guest called abort()",
        message: state.bionic.abort_message(),
    })
}

/// `void __stack_chk_fail(void)`
///
/// The stack protector found a corrupted canary, which means **guest code has already overflowed a
/// buffer on its own stack**. D13 programs the same canary into `TPIDR_EL0 + 0x28` for every guest
/// thread and into the `__stack_chk_guard` data object, so both loads see one number and a
/// mismatch is real corruption rather than a disagreement between the two forms.
///
/// Reported as an abort, for the containment reason above, and with its own `why` so the reader is
/// not left thinking the guest chose to exit.
pub(super) fn stack_chk_fail(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let state = active(c.symbol(), c.address())?;
    Err(AbiError::GuestAborted {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: "the stack protector found a corrupted canary, so guest code has overflowed a buffer \
              on its own stack (D13 programs the same value into TPIDR_EL0 + 0x28 and into \
              __stack_chk_guard, so the two forms cannot disagree)",
        message: state.bionic.abort_message(),
    })
}

/// `void _exit(int status)`
///
/// Reported rather than performed, for the same reason as [`abort`]. An exit is an `Err` rather
/// than a successful return because a guest that has called `_exit` must not be resumed, and every
/// caller of `Boundary::run` treats `Ok` as resumable.
pub(super) fn exit(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let status = c.args().next_i32()?;
    Err(AbiError::GuestExited {
        symbol: c.symbol().to_string(),
        address: c.address(),
        status,
    })
}

/// `void android_set_abort_message(const char *msg)`
///
/// Stores the message so the next `abort` can report it. A null message clears it, which is what
/// passing null means.
///
/// The string is read with [`GuestMem::cstr`](crate::mem::GuestMem::cstr) and is therefore already
/// bounded by that function's 64 KiB `STRING_LIMIT` — a guest cannot make this layer allocate an
/// unbounded amount by pointing at a string with no NUL, and an unterminated one is
/// [`AbiError::Unterminated`](crate::AbiError::Unterminated) rather than a scan off the end of the
/// mapping.
pub(super) fn android_set_abort_message(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let msg = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    {
        let view = enter(c, &state);
        if msg == 0 {
            state.bionic.set_abort_message(None);
        } else {
            let at = guest_address(view.blaming(0), msg)?;
            let bytes = view.mem().cstr(at, Blame::new(view.symbol(), view.address(), 0))?;
            // Lossy rather than a refusal: an abort message is a label the guest chose, it is
            // under no obligation to be UTF-8, and refusing to record it would lose the only
            // account of the crash in order to complain about its encoding.
            state.bionic.set_abort_message(Some(String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    c.ret().void();
    Ok(())
}

// ================================================================== the four refusals

/// bionic's `_SC_PAGESIZE`.
const SC_PAGESIZE: i32 = 0x0027;
/// bionic's `_SC_PAGE_SIZE`. **A different constant from [`SC_PAGESIZE`], naming the same
/// quantity** — that is bionic's own arrangement, where glibc has one value and an alias.
const SC_PAGE_SIZE: i32 = 0x0028;
/// bionic's `_SC_NPROCESSORS_CONF`.
const SC_NPROCESSORS_CONF: i32 = 0x0060;
/// bionic's `_SC_NPROCESSORS_ONLN`.
const SC_NPROCESSORS_ONLN: i32 = 0x0061;

/// What a `sysconf` name is believed to be, for the refusal's diagnostic half.
///
/// The four this layer answers are named as constants above. The rest are here so a refusal can
/// say what the number it was given is *believed* to be and give whoever reads it somewhere to
/// start; nothing branches on the text.
fn believed_sysconf_name(name: i32) -> Option<&'static str> {
    Some(match name {
        0x0006 => "_SC_CLK_TCK",
        0x000b => "_SC_OPEN_MAX",
        0x0026 => "_SC_IOV_MAX",
        SC_PAGESIZE => "_SC_PAGESIZE",
        SC_PAGE_SIZE => "_SC_PAGE_SIZE",
        SC_NPROCESSORS_CONF => "_SC_NPROCESSORS_CONF",
        SC_NPROCESSORS_ONLN => "_SC_NPROCESSORS_ONLN",
        0x0062 => "_SC_PHYS_PAGES",
        _ => return None,
    })
}

/// `long sysconf(int name)`
///
/// Answers the page size and the processor count, and refuses everything else by name. See this
/// module's documentation for the evidence that licensed the four numbers, which is the guest's
/// own call sites rather than a header.
///
/// The page size comes from [`Bionic::space_page_size`](super::Bionic::space_page_size), which is
/// the same source `getauxval(AT_PAGESZ)` answers from — so the two cannot disagree, which they
/// could if this had its own constant. bionic implements `sysconf(_SC_PAGESIZE)` as
/// `getauxval(AT_PAGESZ)` for exactly that reason.
pub(super) fn sysconf(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let name = c.args().next_i32()?;
    let state = active(c.symbol(), c.address())?;
    let value: i64 = match name {
        SC_PAGESIZE | SC_PAGE_SIZE => state.bionic.space_page_size() as i64,
        SC_NPROCESSORS_CONF | SC_NPROCESSORS_ONLN => {
            // A failure here is reported, never substituted. `sysconf` answers `-1` without
            // setting `errno` for a name the implementation does not support, which a caller
            // reads as "unlimited or unknown" and carries forward; the phase-3 review's finding
            // M7 was this exact substitution one layer down.
            match omni_platform::process::cpu_count() {
                Ok(count) => {
                    let n = count.get();
                    let Ok(narrowed) = i64::try_from(n) else {
                        return Err(refuse(
                            c,
                            format!("the host reports {n} processors, which does not fit a long"),
                        ));
                    };
                    narrowed
                }
                Err(error) => {
                    return Err(refuse(
                        c,
                        format!(
                            "the guest asked for sysconf({name}), the processor count, and the \
                             host could not determine it: {error}. Answering 1 would be a \
                             believable number with no measurement behind it, and a guest sizing \
                             a thread pool from it would carry that for the life of the run"
                        ),
                    ));
                }
            }
        }
        other => {
            let believed = believed_sysconf_name(other).map_or_else(
                || "no name this layer recognises".to_string(),
                |text| format!("believed to be `{text}`"),
            );
            return Err(refuse(
                c,
                format!(
                    "the guest asked for sysconf({other}), {believed}. This layer answers the \
                     page size (`_SC_PAGESIZE` {SC_PAGESIZE}, `_SC_PAGE_SIZE` {SC_PAGE_SIZE}) and \
                     the processor count (`_SC_NPROCESSORS_CONF` {SC_NPROCESSORS_CONF}, \
                     `_SC_NPROCESSORS_ONLN` {SC_NPROCESSORS_ONLN}) because it knows both. It has \
                     no clock tick, no descriptor ceiling of the guest's own, no `iovec` limit and \
                     no physical memory to report, and every one of those has a believable wrong \
                     answer available"
                ),
            ));
        }
    };
    c.ret().u64(value as u64);
    Ok(())
}

/// Bytes of one guest `struct sysinfo`.
///
/// **VERIFIED from the guest's own instructions**, which is a stronger provenance than the six
/// ASSUMED layouts this project carries. Linux's `asm-generic` definition is
/// `long uptime; unsigned long loads[3]; unsigned long totalram, freeram, sharedram, bufferram,
/// totalswap, freeswap; unsigned short procs, pad; unsigned long totalhigh, freehigh;
/// unsigned int mem_unit; char _f[20 - 2 * sizeof(long) - sizeof(int)]` — on LP64 the trailing
/// array is **zero** bytes, the last field ends at 108, and eight-byte alignment rounds the
/// structure to 112. The engine's only direct call site zeroes its stack buffer with
/// `STP q0, q0, [sp]`, `[sp, #0x20]`, `[sp, #0x40]` and `STR q0, [sp, #0x60]` before the call:
/// exactly **112** bytes.
pub const SYSINFO_BYTES: usize = 112;

/// `int sysinfo(struct sysinfo *info)`
///
/// # Every field is a fact about *this guest's* world, and one is not
///
/// Phase 3a refused this call because "that structure is uptime, load averages, total and free
/// RAM, ... and this layer models none of them". That was true of a *machine*, and the structure
/// does not have to describe one: it describes the environment the guest is running in, and this
/// runtime **is** that environment. So
///
/// | field | what is reported | why it is a fact |
/// |---|---|---|
/// | `uptime` | how long this [`Bionic`](super::Bionic) instance has existed | there was no boot; this is the earliest moment the guest could observe anything |
/// | `totalram` | [`Bionic::set_memory_budget`](super::Bionic::set_memory_budget) | the embedding's budget for this guest, stated by the embedding — there is no default |
/// | `freeram` | that budget less this process's commit charge | D10's commit charge is the one quantity that measures memory actually taken |
/// | `sharedram`, `bufferram`, `totalswap`, `freeswap`, `totalhigh`, `freehigh` | 0 | this guest has no page cache, no swap and no high memory. Zero is what it *has*, not a stand-in |
/// | `procs` | 1 | one guest process per instance (`ARCHITECTURE.md` section 7) |
/// | `mem_unit` | 1 | the other fields are in bytes |
///
/// **`loads[3]` is the exception and it is stated rather than hidden.** A one, five and fifteen
/// minute average of runnable tasks is an accounting this runtime does not do, and there is no
/// value that says "not measured" — the field is an unsigned scaled integer. It is reported as
/// **zero**, and zero is not a measurement. Anything that branches on a load average will get the
/// answer "idle", which is the one believable wrong answer in this structure.
///
/// **What makes that acceptable here, and what would stop it being acceptable.** The engine's
/// only *direct* `sysinfo` call site — `tools/call_sites.py`, one site — fills a 112-byte stack
/// buffer, ignores the return value, and then **reuses the same buffer** as the destination of a
/// 32-byte `read` from `/proc/sys/vm/overcommit_memory`. The structure is dead before anything
/// reads a field of it; what that initializer actually wants is the page size and the overcommit
/// setting. If a later milestone finds a caller that reads `loads`, this becomes a refusal again,
/// and `Bionic::set_memory_budget`'s neighbourhood is where a host-supplied load average would go.
pub(super) fn sysinfo(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let info = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;
    let Some(total) = state.bionic.memory_budget() else {
        return Err(refuse(
            c,
            format!(
                "the guest asked to fill the `struct sysinfo` at {info:#x}, and nothing has told \
                 this instance how much memory the guest has. Every other field is a fact this \
                 runtime knows -- its own uptime, its commit charge, that it has no swap and one \
                 process -- but `totalram` is the embedding's budget, and answering it with the \
                 host's physical memory would be the wrong number in the one field a guest sizes \
                 a cache from. Call Bionic::set_memory_budget"
            ),
        ));
    };
    // Saturating on purpose: the process charge includes this instance's own mappings and
    // anything else in the process, so a host that set a budget smaller than what is already
    // taken gets `freeram = 0` rather than an underflow that wraps to sixteen exabytes.
    let free = omni_mem::process_commit_charge()
        .ok()
        .map_or(total, |charged| total.saturating_sub(charged));
    let uptime = state.bionic.uptime().as_secs();

    let mut bytes = [0u8; SYSINFO_BYTES];
    let mut put = |at: usize, value: u64| bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    put(0, uptime);
    // loads[3] at 8, 16, 24 stay zero. See this function's documentation: the one field with no
    // measurement behind it, stated rather than hidden.
    put(32, total);
    put(40, free);
    // sharedram 48, bufferram 56, totalswap 64, freeswap 72 stay zero: this guest has none.
    bytes[80..82].copy_from_slice(&1u16.to_le_bytes()); // procs
    // pad 82, totalhigh 88, freehigh 96 stay zero.
    bytes[104..108].copy_from_slice(&1u32.to_le_bytes()); // mem_unit: the fields are in bytes

    let result = {
        let view = enter(c, &state);
        let at = guest_address(view.blaming(0), info)?;
        let blame = Blame::new(view.symbol(), view.address(), 0);
        // One write of the whole structure, validated first: a partial fill behind a reported
        // failure is the shape D22 wrote down for `arc4random_buf` and phase 3b did not carry
        // across (review finding M1).
        view.mem().checked_ptr(at, SYSINFO_BYTES, true, blame)?;
        view.mem().write_bytes(at, &bytes, blame)?;
        0
    };
    c.ret().i32(result);
    Ok(())
}

/// The `prctl` options worth naming in a refusal.
///
/// Linux UAPI (`include/uapi/linux/prctl.h`), the same source as the `AT_*` values above, so these
/// **are** verifiable — unlike the `_SC_*` table. `PR_SET_VMA` is the Android-specific one, used to
/// name anonymous mappings so they show up in `/proc/self/maps`; it is the option most likely to
/// arrive first, and it is refused with the rest because this layer has no `/proc`.
fn prctl_option_name(option: i32) -> Option<&'static str> {
    Some(match option {
        1 => "PR_SET_PDEATHSIG",
        4 => "PR_SET_DUMPABLE",
        15 => "PR_SET_NAME",
        16 => "PR_GET_NAME",
        22 => "PR_SET_SECCOMP",
        23 => "PR_CAPBSET_READ",
        38 => "PR_SET_NO_NEW_PRIVS",
        PR_SET_THP_DISABLE => "PR_SET_THP_DISABLE",
        PR_GET_THP_DISABLE => "PR_GET_THP_DISABLE",
        PR_SET_VMA => "PR_SET_VMA (Android: name an anonymous mapping)",
        _ => return None,
    })
}

/// `PR_SET_THP_DISABLE`: ask the kernel not to give this process transparent huge pages.
const PR_SET_THP_DISABLE: i32 = 41;
/// `PR_GET_THP_DISABLE`: read that flag back.
const PR_GET_THP_DISABLE: i32 = 42;
/// `PR_SET_VMA`, Android's option for labelling an anonymous mapping.
const PR_SET_VMA: i32 = 0x5356_4d41;
/// `PR_SET_VMA_ANON_NAME`, the only `PR_SET_VMA` sub-option the kernel defines.
const PR_SET_VMA_ANON_NAME: u64 = 0;
/// The kernel's `ANON_VMA_NAME_MAX_LEN`: how long a label may be, NUL excluded.
///
/// From `mm/madvise.c`. A longer name is `EINVAL` on a device, so it is `EINVAL` here — refusing
/// it would report a guest bug as a gap in this layer.
const ANON_VMA_NAME_MAX: usize = 80;

/// `int prctl(int option, ...)`
///
/// Variadic, and the *option* decides what the remaining arguments even are — so an option this
/// layer does not model is refused before any of them is read, rather than fetched out of the
/// wrong bank.
///
/// # `PR_SET_VMA` is answered, and it is an implementation rather than a stub
///
/// **Found by M3's gate, at `init_array[5]`.** `prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, addr,
/// len, name)` attaches a human-readable label to an anonymous mapping. Its **entire** observable
/// effect on a device is the text that then appears beside that range in `/proc/self/maps`: the
/// kernel copies the string, checks the range is anonymous, and changes no protection, no
/// contents and no behaviour. Nothing a program can do reads it back through `prctl`.
///
/// So this layer can honour the whole contract by keeping the label
/// ([`Bionic::vma_names`](super::Bionic::vma_names)) — which is not a plausible zero but the
/// thing itself, stored where a host debugging a guest can read it. What is *not* modelled is
/// `/proc`, and no guest can tell, because there is no `/proc` for it to look in.
///
/// The guest-visible error cases are kept: a sub-option other than `PR_SET_VMA_ANON_NAME`, a name
/// longer than the kernel's `ANON_VMA_NAME_MAX_LEN`, and an unreadable name pointer are `EINVAL`
/// with `-1`, which is what the kernel answers and what the caller branches on.
pub(super) fn prctl(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (option, sub, addr, len, name) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    if option == PR_SET_THP_DISABLE || option == PR_GET_THP_DISABLE {
        // **`EINVAL` is the correct answer, not a refusal and not a stub.** Both options exist
        // only on a kernel built with `CONFIG_TRANSPARENT_HUGEPAGE`; without it the kernel
        // answers `-EINVAL` for each, because there is no per-process flag to set or read. This
        // runtime has no transparent huge pages at all -- guest memory is `omni-mem`'s 4 KiB
        // pages committed a 64 KiB granule at a time (D10) -- so it is in exactly that
        // configuration, and saying so is a fact about it.
        //
        // The engine's allocator asks `PR_GET_THP_DISABLE` and then `PR_SET_THP_DISABLE`
        // (`tools/call_sites.py`: sites at `0x1d96fa4` and `0x1d96fc8`, inside the same
        // initializer that reads `/proc/sys/vm/overcommit_memory`). Answering 0 would tell it
        // huge pages are available and not disabled, which is the believable wrong answer.
        let state = active(c.symbol(), c.address())?;
        let mut view = enter(c, &state);
        view.set_errno(omni_bionic::errno::consts::EINVAL);
        drop(view);
        c.ret().i32(-1);
        return Ok(());
    }
    if option != PR_SET_VMA {
        let named = prctl_option_name(option)
            .map_or_else(String::new, |name| format!(" ({name})"));
        return Err(refuse(
            c,
            format!(
                "the guest called prctl with option {option}{named}. `prctl` is a switch \
                 statement over process state this layer does not have -- a thread name in /proc, \
                 a dumpable flag, a seccomp filter -- and every option has 0 available as a \
                 believable \"done\". Refusing names which option arrived, which is the \
                 information needed to decide whether it is worth modelling. `PR_SET_VMA` \
                 ({PR_SET_VMA:#x}) is the one option this layer answers"
            ),
        ));
    }
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        if sub != PR_SET_VMA_ANON_NAME {
            view.set_errno(omni_bionic::errno::consts::EINVAL);
            -1
        } else {
            match read_vma_name(&view, name) {
                Ok(text) => {
                    state.bionic.set_vma_name(addr, len, text);
                    0
                }
                Err(errno) => {
                    view.set_errno(errno);
                    -1
                }
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// The label `PR_SET_VMA_ANON_NAME` was given, or the `errno` the kernel would answer.
///
/// A null pointer **clears** the label, which the kernel accepts, so it is not an error.
fn read_vma_name(view: &GuestView<'_>, name: u64) -> Result<Option<String>, i32> {
    if name == 0 {
        return Ok(None);
    }
    // Bounded by the kernel's own limit rather than by the string: a guest that passed an
    // unterminated pointer must not turn into an unbounded host-side walk.
    let mut bytes = Vec::with_capacity(ANON_VMA_NAME_MAX);
    for offset in 0..=ANON_VMA_NAME_MAX as u64 {
        let mut byte = [0u8; 1];
        if omni_bionic::memory::GuestMemory::read(view, name + offset, &mut byte).is_err() {
            return Err(omni_bionic::errno::consts::EINVAL);
        }
        if byte[0] == 0 {
            return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
        }
        if bytes.len() == ANON_VMA_NAME_MAX {
            // Past the kernel's limit without a terminator: `EINVAL`, as a device gives.
            return Err(omni_bionic::errno::consts::EINVAL);
        }
        bytes.push(byte[0]);
    }
    Err(omni_bionic::errno::consts::EINVAL)
}

/// The AArch64 syscall numbers worth naming in a refusal.
///
/// The asm-generic numbering, which is what arm64 Linux uses. Diagnostic only; nothing branches on
/// it. `gettid` (178) is listed because a raw `syscall(SYS_gettid)` is the usual way Android code
/// gets a thread id and is the likeliest of these to arrive first.
fn syscall_name(number: i64) -> Option<&'static str> {
    Some(match number {
        56 => "openat",
        57 => "close",
        63 => "read",
        64 => "write",
        78 => "readlinkat",
        98 => "futex",
        113 => "clock_gettime",
        115 => "clock_nanosleep",
        122 => "sched_setaffinity",
        124 => "sched_yield",
        135 => "rt_sigprocmask",
        167 => "prctl",
        172 => "getpid",
        178 => "gettid",
        214 => "brk",
        215 => "munmap",
        222 => "mmap",
        226 => "mprotect",
        278 => "getrandom",
        293 => "rseq",
        _ => return None,
    })
}

/// `gettid`, in the asm-generic numbering arm64 Linux uses.
const SYS_GETTID: i64 = 178;
/// `rt_sigprocmask`, in the asm-generic numbering arm64 Linux uses.
const SYS_RT_SIGPROCMASK: i64 = 135;
/// `sizeof(sigset_t)` as the kernel's `rt_sigprocmask` requires it, and as D24 records it.
const SIGSET_BYTES: u64 = 8;

/// `int rt_sigprocmask(int how, const sigset_t *set, sigset_t *oldset, size_t sigsetsize)`,
/// reached through `syscall(135, ..)`.
///
/// # The engine uses this as a **pointer-readability probe**, and that is why it is here
///
/// M3's gate stopped at `init_array[3118]` on `syscall(135)`. Decoding the site
/// (`tools/call_sites.py`, `0x2b9fcdc`) shows what it is for:
///
/// ```text
/// e = __errno(); saved = *e;
/// syscall(135, /* how */ -1, /* set */ p, /* oldset */ NULL, /* size */ 8);
/// readable = (*e != EFAULT); *e = saved;
/// ```
///
/// `how = -1` is **invalid on purpose**. The kernel's `rt_sigprocmask` checks `sigsetsize`, then
/// `copy_from_user(set)` — which fails `EFAULT` for an unreadable pointer — and only then rejects
/// `how` with `EINVAL`. So the call can never succeed, and the *errno it fails with* is a precise
/// answer to "can this process read eight bytes at `p`". It is a well-known idiom, and this layer
/// can answer it exactly, because `GuestMem` already decides that question for every other call
/// the boundary services.
///
/// # The mask itself
///
/// D24 excluded `pthread_sigmask` because it "needs the guest's real signal state". A mask set
/// here is still stored and returned exactly, and the reason that is not a contradiction is that
/// **this runtime delivers no signal to the guest at all** — `sigaction` and `raise` both refuse
/// by name (D24), and nothing else raises one. The blocked set therefore has exactly one
/// observable, which is the guest reading back what it wrote, and that is modelled precisely
/// rather than approximated.
///
/// **What would make this wrong** is a milestone that delivers a real signal to guest code. At
/// that point the mask has to gate delivery, and `Bionic::update_signal_mask` is where it would.
fn rt_sigprocmask(c: &mut ImportCall<'_, '_>, args: (i64, u64, u64, u64)) -> AbiResult<()> {
    let (how, set, oldset, sigsetsize) = args;
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        // The kernel's order, and it is the whole point: size, then the pointers, then `how`.
        if sigsetsize != SIGSET_BYTES {
            view.set_errno(omni_bionic::errno::consts::EINVAL);
            -1
        } else {
            let readable = |view: &GuestView<'_>, pointer: u64, write: bool| -> bool {
                let Ok(at) = GuestAddr::try_from(pointer) else { return false };
                view.mem()
                    .checked_ptr(
                        at,
                        SIGSET_BYTES as usize,
                        write,
                        Blame::new(view.symbol(), view.address(), 0),
                    )
                    .is_ok()
            };
            if (set != 0 && !readable(&view, set, false))
                || (oldset != 0 && !readable(&view, oldset, true))
            {
                view.set_errno(EFAULT);
                -1
            } else if set != 0 && !(0..=2).contains(&how) {
                // SIG_BLOCK 0, SIG_UNBLOCK 1, SIG_SETMASK 2. The probe lands here.
                view.set_errno(omni_bionic::errno::consts::EINVAL);
                -1
            } else {
                let bits = if set == 0 {
                    None
                } else {
                    let mut buf = [0u8; SIGSET_BYTES as usize];
                    omni_bionic::memory::GuestMemory::read(&view, set, &mut buf)
                        .map_err(|fault| view.fault(fault))?;
                    Some(u64::from_le_bytes(buf))
                };
                let previous =
                    state.bionic.update_signal_mask(state.thread, how as i32, bits);
                if oldset != 0 {
                    omni_bionic::memory::GuestMemory::write(
                        &mut view,
                        oldset,
                        &previous.to_le_bytes(),
                    )
                    .map_err(|fault| view.fault(fault))?;
                }
                0
            }
        }
    };
    c.ret().i32(result);
    Ok(())
}

/// `EFAULT`, 14 in Linux numbering. Not in `omni-bionic`'s table, which carries the codes its own
/// functions produce; this one is the *kernel's* answer to an unreadable pointer, and it is the
/// value the engine's probe compares against.
const EFAULT: i32 = 14;

// ================================================================== the raw futex
//
// MEASURED by M5's gate: **two of the engine's own worker threads died on `syscall(98, ..)`**,
// reported through `Bionic::guest_thread_failures()` because nobody joins a detached thread. It
// was the largest single obstacle between M5 and M6 (D29), and it is the third raw syscall the
// engine turns out to issue, after `rt_sigprocmask` and `gettid`.
//
// **This is not a stub and it is not a new capability.** `omni-bionic`'s whole synchronisation
// layer already blocks and wakes on guest addresses through `AddressFutex`, which is a futex in
// everything but the name — a parking lot keyed by a guest address, with the queue's bucket lock
// held across registration. What was missing was the *door*: the guest was knocking on the raw
// one. `syscall` already dispatches by number to a real implementation where one exists, and this
// is the same shape.

/// `futex`, in the asm-generic numbering arm64 Linux uses.
const SYS_FUTEX: i64 = 98;

/// `FUTEX_WAIT`: sleep if `*uaddr == val`.
const FUTEX_WAIT: i32 = 0;
/// `FUTEX_WAKE`: wake up to `val` waiters.
const FUTEX_WAKE: i32 = 1;
/// `FUTEX_WAIT_BITSET`: as `FUTEX_WAIT`, with an **absolute** timeout and a bitset.
const FUTEX_WAIT_BITSET: i32 = 9;
/// `FUTEX_WAKE_BITSET`: as `FUTEX_WAKE`, waking only waiters whose bitset intersects.
const FUTEX_WAKE_BITSET: i32 = 10;

/// `FUTEX_PRIVATE_FLAG`: the futex is not shared between processes.
///
/// **Accepted and ignored, and that is a fact rather than a convenience.** The flag tells the
/// kernel it may skip the work of resolving the address to a shared-memory identity, because no
/// other process can be waiting on it. Here there *is* no other process: every guest address
/// belongs to one instance in one host process, so private and shared name the same set of
/// waiters. Ignoring it is the whole of what honouring it means.
const FUTEX_PRIVATE_FLAG: i32 = 128;

/// `FUTEX_CLOCK_REALTIME`: measure the (absolute) timeout against `CLOCK_REALTIME`.
const FUTEX_CLOCK_REALTIME: i32 = 256;

/// `FUTEX_BITSET_MATCH_ANY`: the bitset bionic's own `__futex_wait_ex` passes.
const FUTEX_BITSET_MATCH_ANY: u32 = 0xffff_ffff;

/// The command bits, with the two flags masked off.
fn futex_command(op: i32) -> i32 {
    op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME)
}

/// Name a futex operation for a refusal, so the message says what was asked rather than a number.
fn futex_op_name(command: i32) -> &'static str {
    match command {
        0 => "FUTEX_WAIT",
        1 => "FUTEX_WAKE",
        2 => "FUTEX_FD",
        3 => "FUTEX_REQUEUE",
        4 => "FUTEX_CMP_REQUEUE",
        5 => "FUTEX_WAKE_OP",
        6 => "FUTEX_LOCK_PI",
        7 => "FUTEX_UNLOCK_PI",
        8 => "FUTEX_TRYLOCK_PI",
        9 => "FUTEX_WAIT_BITSET",
        10 => "FUTEX_WAKE_BITSET",
        11 => "FUTEX_WAIT_REQUEUE_PI",
        12 => "FUTEX_CMP_REQUEUE_PI",
        13 => "FUTEX_LOCK_PI2",
        _ => "an operation linux/futex.h does not define",
    }
}

/// The six arguments `futex` takes, named.
///
/// A struct rather than a tuple because they are six machine words of which four are integers,
/// and `(uaddr, op, val, timeout, val3)` passed positionally is five chances to transpose two
/// without a compile error -- which for `val` and `val3` would silently compare against the
/// bitset.
struct FutexArgs {
    /// The word to compare and to park on.
    uaddr: u64,
    /// The operation, with its two flag bits still set.
    op: i32,
    /// `FUTEX_WAIT`'s expected value, or `FUTEX_WAKE`'s count.
    val: u64,
    /// A `struct timespec *`, or null for no timeout.
    timeout: u64,
    /// The bitset, for the `BITSET` forms.
    val3: u64,
}

/// `long futex(uint32_t *uaddr, int op, uint32_t val, const struct timespec *timeout,
///             uint32_t *uaddr2, uint32_t val3)`, reached through `syscall(98, ..)`.
///
/// # What is implemented, and what each answer is
///
/// | operation | answer |
/// |---|---|
/// | `FUTEX_WAIT` | `0` woken, `-1`/`EAGAIN` if `*uaddr != val`, `-1`/`ETIMEDOUT` at the timeout |
/// | `FUTEX_WAIT_BITSET` | the same, with an **absolute** timeout, and only for `FUTEX_BITSET_MATCH_ANY` |
/// | `FUTEX_WAKE` | how many waiters were woken |
/// | `FUTEX_WAKE_BITSET` | the same, and only for `FUTEX_BITSET_MATCH_ANY` |
/// | everything else | **refused by name** |
///
/// # The comparison is the point
///
/// Linux compares `*uaddr` with `val` **atomically with** the decision to block, and that
/// comparison is what makes a futex a futex: a waiter that skipped it would sleep through a wake
/// that had already happened, which is the lost-wake class this project has already measured once
/// at 1.0104 s (`VERIFICATION.md` entry 11). [`AddressFutex::wait_compared`] performs it inside
/// `parking_lot_core`'s `validate` callback, under the queue's bucket lock, which is the same
/// place the kernel performs it.
///
/// This is **not** the `expected` that `Futex::wait` ignores — see `runtime`'s module docs for why
/// that one is ignored and why this one is not the same question.
///
/// # A bitset that is not `MATCH_ANY` refuses, and that is the interesting refusal
///
/// A partial bitset means "wake only waiters interested in these bits", and this futex's queues
/// carry no bits. The believable wrong answer is to treat every bitset as `MATCH_ANY`: it *works*
/// for the common case, and it silently wakes waiters the caller deliberately excluded — which is
/// a correctness bug in the guest's own synchronisation, arriving as a spurious wakeup that its
/// re-check loop will absorb without reporting. bionic's own `__futex_wait_ex` passes
/// `FUTEX_BITSET_MATCH_ANY`, so nothing on the expected path is refused by this.
///
/// # An indefinite wait is allowed here, where `poll` refuses one
///
/// `poll(fds, n, -1)` is refused because **nothing in the descriptor space could ever wake it**.
/// A futex is different in exactly the way that matters: it is woken by another *guest* thread,
/// and this runtime has those — `pthread_mutex_lock` has parked indefinitely through the same
/// mechanism since phase 3c. D16's defence is against a guest that cannot be stopped, and a
/// created guest thread is stopped between run windows whatever it is doing.
fn futex(c: &mut ImportCall<'_, '_>, args: FutexArgs) -> AbiResult<()> {
    let FutexArgs { uaddr, op, val, timeout, val3 } = args;
    let command = futex_command(op);
    let state = active(c.symbol(), c.address())?;
    let absolute = command == FUTEX_WAIT_BITSET;
    let realtime = op & FUTEX_CLOCK_REALTIME != 0;

    // **Recorded before the call, not after.** A `FUTEX_WAIT` with a null timeout never returns
    // until somebody wakes it, so a record written on the way out is written by every call except
    // the ones that matter. MEASURED: with the record after the match, a run with two threads
    // stranded in `FUTEX_WAIT` reported an empty list.
    state.bionic.record_futex_call(crate::bionic::FutexCall {
        thread: state.thread.0,
        op: futex_op_static_name(command),
        address: uaddr,
        value: val as u32,
        outcome: FUTEX_IN_PROGRESS,
    });
    let result = {
        let mut view = enter(c, &state);
        // Linux: `EINVAL` for an unaligned `uaddr`. The word is compared and woken on as a
        // 32-bit quantity, and an unaligned one has no atomic load on any target this runs on.
        if uaddr % 4 != 0 {
            view.set_errno(omni_bionic::errno::consts::EINVAL);
            c.ret().i32(-1);
            return Ok(());
        }
        let Ok(word) = GuestAddr::try_from(uaddr) else {
            view.set_errno(EFAULT);
            c.ret().i32(-1);
            return Ok(());
        };
        match command {
            FUTEX_WAIT | FUTEX_WAIT_BITSET => {
                if command == FUTEX_WAIT_BITSET && val3 as u32 != FUTEX_BITSET_MATCH_ANY {
                    return Err(refuse(
                        c,
                        format!(
                            "the guest issued `futex(FUTEX_WAIT_BITSET)` with bitset                              {:#010x}. This layer's queues carry no bits, so it can honour only                              FUTEX_BITSET_MATCH_ANY ({FUTEX_BITSET_MATCH_ANY:#010x}), which is                              what bionic's own `__futex_wait_ex` passes. Treating a partial                              bitset as MATCH_ANY is the believable wrong answer: it works, and                              it wakes waiters the caller deliberately excluded",
                            val3 as u32
                        ),
                    ));
                }
                // The timeout. `FUTEX_WAIT`'s is **relative**; `FUTEX_WAIT_BITSET`'s is
                // **absolute**, and the two are not interchangeable -- treating an absolute
                // deadline as a duration would sleep for fifty-five years.
                let wait = match read_futex_timeout(&mut view, timeout, absolute, realtime)? {
                    Ok(duration) => duration,
                    Err(errno) => {
                        view.set_errno(errno);
                        c.ret().i32(-1);
                        return Ok(());
                    }
                };
                let expected = val as u32;
                let mem = view.mem();
                let blame = Blame::new(view.symbol(), view.address(), 0);
                // The word must be readable **before** anything parks on it, so that an
                // unreadable one is `EFAULT` rather than a thread asleep on an address nothing
                // will ever wake.
                if mem.read_u32(word, blame).is_err() {
                    view.set_errno(EFAULT);
                    c.ret().i32(-1);
                    return Ok(());
                }
                let outcome = state.bionic.futex().wait_compared(
                    uaddr,
                    || mem.read_u32(word, blame).is_ok_and(|current| current == expected),
                    wait,
                );
                match outcome {
                    omni_bionic::threads::WaitResult::Woken => 0,
                    omni_bionic::threads::WaitResult::TimedOut => {
                        view.set_errno(omni_bionic::errno::consts::ETIMEDOUT);
                        -1
                    }
                    omni_bionic::threads::WaitResult::WouldBlock => {
                        // The word had already changed. `EAGAIN` is Linux's answer and the whole
                        // value of having compared.
                        view.set_errno(omni_bionic::errno::consts::EAGAIN);
                        -1
                    }
                }
            }
            FUTEX_WAKE | FUTEX_WAKE_BITSET => {
                if command == FUTEX_WAKE_BITSET && val3 as u32 != FUTEX_BITSET_MATCH_ANY {
                    return Err(refuse(
                        c,
                        format!(
                            "the guest issued `futex(FUTEX_WAKE_BITSET)` with bitset {:#010x};                              as FUTEX_WAIT_BITSET, only FUTEX_BITSET_MATCH_ANY can be honoured",
                            val3 as u32
                        ),
                    ));
                }
                // `val` is the number to wake. Linux takes it as an `int` and treats a negative
                // one as a very large count; the two spellings of "all" that callers use are
                // `INT_MAX` and `UINT_MAX`, and both land on `u32::MAX` here.
                let count = if (val as i32) < 0 { u32::MAX } else { val as u32 };
                omni_bionic::threads::Futex::wake(state.bionic.futex(), uaddr, count) as i32
            }
            other => {
                return Err(refuse(
                    c,
                    format!(
                        "the guest issued `futex({uaddr:#x}, {}, ..)`. This layer implements \
                         FUTEX_WAIT, FUTEX_WAKE and their BITSET forms over the same parking lot \
                         `omni-bionic`'s mutexes and condition variables already use; \
                         {} asks for something that lot does not have. REQUEUE and WAKE_OP move \
                         or conditionally wake waiters across two addresses, and the PI \
                         operations need priority inheritance from a kernel scheduler -- there is \
                         no kernel here. Answering 0 would tell the guest a requeue happened, and \
                         its waiters would then be on the wrong queue",
                        futex_op_name(other),
                        futex_op_name(other)
                    ),
                ));
            }
        }
    };
    // And again on the way out, so the pair says both what was asked and what it answered. An
    // entry that never gains a partner is a call that never returned.
    state.bionic.record_futex_call(crate::bionic::FutexCall {
        thread: state.thread.0,
        op: futex_op_static_name(command),
        address: uaddr,
        value: val as u32,
        outcome: result,
    });
    c.ret().i32(result);
    Ok(())
}

/// The `outcome` of a [`FutexCall`](crate::bionic::FutexCall) that has been entered and not yet
/// returned.
///
/// A sentinel rather than an `Option`, so the record stays `Copy` and one field carries the whole
/// answer. `i32::MIN` is not a value any futex operation returns: waits answer `0` or `-1`, and
/// wakes answer a count.
const FUTEX_IN_PROGRESS: i32 = i32::MIN;

/// The operation's name as a `'static` string, for the record.
///
/// Separate from [`futex_op_name`], which formats an unknown operation into an owned `String`:
/// this one only ever sees the four commands that got as far as being performed.
const fn futex_op_static_name(command: i32) -> &'static str {
    match command {
        FUTEX_WAIT => "FUTEX_WAIT",
        FUTEX_WAKE => "FUTEX_WAKE",
        FUTEX_WAIT_BITSET => "FUTEX_WAIT_BITSET",
        FUTEX_WAKE_BITSET => "FUTEX_WAKE_BITSET",
        _ => "FUTEX_?",
    }
}

/// Read a `struct timespec` argument for `futex`, and turn it into a duration to wait.
///
/// `Ok(None)` is "wait indefinitely", which a null pointer means. `Err(errno)` is what the guest
/// is told.
///
/// **Absolute and relative are both here because the two operations differ**, and confusing them
/// is not a small error: `FUTEX_WAIT` takes a *relative* timeout and `FUTEX_WAIT_BITSET` takes an
/// *absolute* deadline, so treating one as the other turns a one-millisecond wait into a
/// fifty-five-year one, or a deadline into an immediate timeout.
fn read_futex_timeout(
    view: &mut GuestView<'_>,
    timeout: u64,
    absolute: bool,
    realtime: bool,
) -> AbiResult<Result<Option<Duration>, i32>> {
    if timeout == 0 {
        return Ok(Ok(None));
    }
    let Ok(at) = GuestAddr::try_from(timeout) else {
        return Ok(Err(EFAULT));
    };
    let blame = Blame::new(view.symbol(), view.address(), 3);
    let Ok(bytes) = view.mem().read_bytes(at, TIMESPEC_BYTES, blame) else {
        return Ok(Err(EFAULT));
    };
    let seconds = i64::from_le_bytes(bytes[..8].try_into().expect("eight bytes"));
    let nanos = i64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes"));
    // The kernel's own validation: a `tv_nsec` outside [0, 1e9) is `EINVAL`, and so is a negative
    // `tv_sec` on a relative wait.
    if !(0..1_000_000_000).contains(&nanos) {
        return Ok(Err(omni_bionic::errno::consts::EINVAL));
    }
    if !absolute {
        if seconds < 0 {
            return Ok(Err(omni_bionic::errno::consts::EINVAL));
        }
        return Ok(Ok(Some(
            Duration::from_secs(seconds as u64) + Duration::from_nanos(nanos as u64),
        )));
    }
    // An **absolute** deadline, against `CLOCK_MONOTONIC` unless `FUTEX_CLOCK_REALTIME` says
    // otherwise. Turned into a remaining duration here, against the same clock the guest's own
    // `clock_gettime` reads, so that a deadline the guest computed from that clock means what it
    // meant when it computed it.
    let now_nanos = if realtime {
        omni_platform::clock::realtime_now().as_nanos() as i128
    } else {
        omni_platform::clock::monotonic_now().as_nanos() as i128
    };
    let deadline_nanos = i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
    let remaining = deadline_nanos - now_nanos;
    if remaining <= 0 {
        // A deadline already past is an immediate timeout, not an error and not an indefinite
        // wait. Returning `None` here would park for ever on a call that asked not to.
        return Ok(Ok(Some(Duration::ZERO)));
    }
    Ok(Ok(Some(Duration::from_nanos(
        u64::try_from(remaining).unwrap_or(u64::MAX),
    ))))
}

/// `sizeof(struct timespec)` on LP64: two 64-bit fields.
const TIMESPEC_BYTES: usize = 16;

/// `long syscall(long number, ...)`
///
/// Refused, and refused *by number*, which is the only useful thing this can do. A raw syscall
/// bypasses every symbol this layer binds: the guest is asking the kernel directly, and there is
/// no kernel. Returning `-1`/`ENOSYS` is the believable answer and is wrong for the same reason
/// `mlock` returning 0 was rejected in phase 2 — callers of `syscall` routinely have a fallback
/// path for `ENOSYS`, so the gap would be silently routed around.
pub(super) fn syscall(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (number, a1, a2, a3, a4, _a5, a6) = {
        let mut a = c.args();
        (
            a.next_u64()? as i64,
            a.next_u64()?,
            a.next_u64()?,
            a.next_u64()?,
            a.next_u64()?,
            // `uaddr2`, which only the operations this layer refuses use. Read so that the
            // sixth argument -- `val3`, the bitset -- lands in the right register.
            a.next_u64()?,
            a.next_u64()?,
        )
    };
    if number == SYS_RT_SIGPROCMASK {
        return rt_sigprocmask(c, (a1 as i64, a2, a3, a4));
    }
    if number == SYS_FUTEX {
        return futex(c, FutexArgs { uaddr: a1, op: a2 as i32, val: a3, timeout: a4, val3: a6 });
    }
    if number == SYS_GETTID {
        // **`gettid` is a thread identity, and this runtime has one.**
        //
        // Found by M3's gate, and found the expensive way: a guest thread created during static
        // initialisation called it *while holding a recursive mutex*, the refusal killed that
        // thread, and the main thread then deadlocked on the lock it never released. The whole
        // contract of `gettid` is "a value that identifies this thread within this process, and
        // is not the same as any other live thread's" -- `GuestThreadId` is exactly that, and it
        // is better than the kernel's in one respect: it is never recycled.
        //
        // bionic has no `gettid` symbol to import on an old NDK, which is why the engine issues
        // the raw syscall rather than calling one.
        //
        // **What this does not give the guest** is a value that means anything to the operating
        // system: it names no `/proc/<pid>/task` entry and cannot be passed to `tgkill`. Neither
        // exists here -- there is no `/proc` and `raise` already refuses -- so there is nothing a
        // guest could do with a "real" one that it cannot do with this.
        let state = active(c.symbol(), c.address())?;
        let Some(thread) = state.bionic.current_thread() else {
            return Err(refuse(
                c,
                "the calling thread is not attached to this instance, so it has no identity to                  report. A host that runs guest code holds a `Bionic::activate` guard across it"
                    .to_string(),
            ));
        };
        let Ok(narrowed) = i32::try_from(thread.0) else {
            return Err(refuse(
                c,
                format!(
                    "this guest instance has reached thread identity {}, which does not fit the                      guest's `pid_t` (an int). Truncating it would hand two live threads the                      same id",
                    thread.0
                ),
            ));
        };
        c.ret().i32(narrowed);
        return Ok(());
    }
    let named = syscall_name(number).map_or_else(String::new, |name| format!(" (arm64 `{name}`)"));
    Err(refuse(
        c,
        format!(
            "the guest issued raw syscall {number}{named}. There is no kernel here: a raw syscall \
             bypasses every symbol this layer binds and asks the operating system directly, in the \
             guest's ABI. Answering -1/ENOSYS is the believable wrong answer, because callers of \
             `syscall` routinely carry an ENOSYS fallback and would route around the gap without \
             reporting it"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The auxiliary-vector numbers are the Linux UAPI ones, and `HWCAP_ATOMICS` is bit 8.
    ///
    /// Bit 8 is the entire open decision, so it is pinned as a number rather than left to a shift
    /// expression nobody re-checks.
    #[test]
    fn the_auxv_numbers_and_the_lse_bit_are_the_linux_ones() {
        assert_eq!(AT_PAGESZ, 6);
        assert_eq!(AT_HWCAP, 16);
        assert_eq!(AT_HWCAP2, 26);
        assert_eq!(HWCAP_ATOMICS, 0x100, "HWCAP_ATOMICS is bit 8 of AT_HWCAP on AArch64");
    }

    /// `Decline` and `Undecided` are different values, and neither is reachable by defaulting.
    ///
    /// The failure this catches is the one the module documentation is about: a policy type where
    /// "no decision" and "decided to decline" are the same value cannot refuse, because it cannot
    /// tell that no decision was made.
    #[test]
    fn declining_and_being_undecided_are_not_the_same_value() {
        assert_ne!(HwcapPolicy::Undecided, HwcapPolicy::Decline);
        assert_ne!(
            HwcapPolicy::Decline,
            HwcapPolicy::Advertise { hwcap: 0, hwcap2: 0 },
            "an explicit advertisement of zero is a decision and Decline is the same decision \
             spelled differently -- but neither may equal Undecided"
        );
        assert_ne!(HwcapPolicy::Undecided, HwcapPolicy::Advertise { hwcap: 0, hwcap2: 0 });
    }

    /// `PROP_VALUE_MAX` is the published Android constant, and the `_SC_` table is flagged.
    #[test]
    fn the_property_limit_is_the_android_one() {
        assert_eq!(PROP_VALUE_MAX, 92, "PROP_VALUE_MAX from <sys/system_properties.h>");
    }

    /// The diagnostic tables name what they know and refuse to name what they do not.
    #[test]
    fn the_diagnostic_tables_do_not_invent_names() {
        assert_eq!(believed_sysconf_name(0x0027), Some("_SC_PAGESIZE"));
        assert_eq!(believed_sysconf_name(-1), None);
        assert_eq!(believed_sysconf_name(0x7fff), None);
        assert_eq!(prctl_option_name(15), Some("PR_SET_NAME"));
        assert!(prctl_option_name(0x5356_4d41).is_some_and(|n| n.starts_with("PR_SET_VMA")));
        assert_eq!(prctl_option_name(9999), None);
        assert_eq!(syscall_name(178), Some("gettid"));
        assert_eq!(syscall_name(-1), None);
        assert_eq!(syscall_name(100_000), None);
    }
}
