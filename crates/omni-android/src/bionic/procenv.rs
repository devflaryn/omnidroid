//! Process and environment: fourteen symbols, of which four are answers, four are facts about a
//! process that was given nothing, two are terminations, and four are refusals by name.
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
//! # `sysconf`, `sysinfo`, `prctl` and `syscall` refuse, and `sysconf` is the interesting one
//!
//! Three of the four are refused because answering means modelling something that is not here: a
//! `struct sysinfo`'s uptime and free memory, a `prctl` option's effect, a raw syscall's contract.
//!
//! `sysconf` is different and the reason is worth reading. Two of its names — the page size and the
//! processor count — are things this layer *does* know. It still refuses, because **bionic's
//! `_SC_*` numbering is bionic's own** (it is not glibc's, and `_SC_PAGESIZE` and `_SC_PAGE_SIZE`
//! are two different numbers there rather than one macro), there is no NDK on this machine, and a
//! number derived from memory has exactly the wrong failure mode: if the constant is wrong, the
//! real page-size query arrives as an unmodelled number and is refused *loudly*, while some other
//! `_SC_` name silently receives a page size. One half of that is safe and the other is the
//! plausible-wrong-answer class this project has been bitten by repeatedly.
//!
//! The refusal names the value it was given and says what that value is *believed* to be, flagged
//! as unverified — which is diagnostic without being load-bearing, because nothing branches on it.
//! Confirming four constants against a real header turns this into four lines.

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

/// What a `sysconf` name is **believed** to be, for the refusal's diagnostic half.
///
/// **Unverified, and nothing branches on it.** Bionic's `_SC_*` numbering is its own and there is
/// no NDK on this machine; this table exists so that a refusal can say "the value 39 is believed
/// to be `_SC_PAGESIZE`" and give whoever reads it somewhere to start. Confirming these four
/// against a real header is what turns the page size and the processor count into answers.
fn believed_sysconf_name(name: i32) -> Option<&'static str> {
    Some(match name {
        0x0006 => "_SC_CLK_TCK",
        0x000b => "_SC_OPEN_MAX",
        0x0027 => "_SC_PAGESIZE",
        0x0028 => "_SC_PAGE_SIZE",
        0x0060 => "_SC_NPROCESSORS_CONF",
        0x0061 => "_SC_NPROCESSORS_ONLN",
        _ => return None,
    })
}

/// `long sysconf(int name)`
///
/// Refused for every name. See the module documentation for why the two names this layer could
/// answer are refused as well: the constant numbering could not be verified, and getting it wrong
/// answers *some other* query with a page size.
pub(super) fn sysconf(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let name = c.args().next_i32()?;
    let believed = believed_sysconf_name(name).map_or_else(
        || "no name this layer recognises".to_string(),
        |text| format!("believed, UNVERIFIED, to be `{text}`"),
    );
    Err(refuse(
        c,
        format!(
            "the guest asked for sysconf({name}), {believed}. Bionic's `_SC_*` numbering is its \
             own -- it is not glibc's, and `_SC_PAGESIZE` and `_SC_PAGE_SIZE` are two different \
             values there rather than one macro -- and there is no NDK on this machine to check it \
             against. The page size and the processor count are both known here; answering them \
             against a constant derived from memory would, if the constant were wrong, refuse the \
             real query loudly and hand some unrelated `_SC_` name a page size silently. \
             Confirming four constants against a real header turns this into four lines"
        ),
    ))
}

/// `int sysinfo(struct sysinfo *info)`
pub(super) fn sysinfo(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let info = c.args().next_u64()?;
    Err(refuse(
        c,
        format!(
            "the guest asked to fill the `struct sysinfo` at {info:#x}. That structure is uptime, \
             one/five/fifteen-minute load averages, total and free RAM, shared and buffered RAM, \
             total and free swap, the process count and the memory unit -- and this layer models \
             none of them. Every field has a believable wrong value available (0 free swap, 1 \
             process, the host's RAM rather than the guest's budget), and a guest sizing a cache \
             from `totalram` would carry the wrong number for the life of the run"
        ),
    ))
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
        0x5356_4d41 => "PR_SET_VMA (Android: name an anonymous mapping)",
        _ => return None,
    })
}

/// `int prctl(int option, ...)`
///
/// Variadic, and refused before any variadic argument is read: the *option* decides what the rest
/// of the arguments even are, so an option this layer does not model makes the remaining arguments
/// untypeable. Nothing is fetched out of the wrong bank.
pub(super) fn prctl(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let option = c.args().next_i32()?;
    let named = prctl_option_name(option)
        .map_or_else(String::new, |name| format!(" ({name})"));
    Err(refuse(
        c,
        format!(
            "the guest called prctl with option {option}{named}. `prctl` is a switch statement \
             over process state this layer does not have -- a thread name in /proc, a dumpable \
             flag, a seccomp filter, a name attached to an anonymous mapping -- and every option \
             has 0 available as a believable \"done\". Refusing names which option arrived, which \
             is the information needed to decide whether it is worth modelling"
        ),
    ))
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
        124 => "sched_yield",
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

/// `long syscall(long number, ...)`
///
/// Refused, and refused *by number*, which is the only useful thing this can do. A raw syscall
/// bypasses every symbol this layer binds: the guest is asking the kernel directly, and there is
/// no kernel. Returning `-1`/`ENOSYS` is the believable answer and is wrong for the same reason
/// `mlock` returning 0 was rejected in phase 2 — callers of `syscall` routinely have a fallback
/// path for `ENOSYS`, so the gap would be silently routed around.
pub(super) fn syscall(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let number = c.args().next_u64()? as i64;
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
