//! One handler per bound symbol, and the two tables [`Bionic::bind_into`] walks.
//!
//! # The shape of a handler, and the three ways to get it silently wrong
//!
//! Each handler reads its arguments through [`Args`] in AAPCS64 order, calls the `omni-bionic`
//! function, and writes the result through [`Ret`]. The interesting part is the types, because
//! the guest's are **not** the host's and none of the differences is a compile error:
//!
//! * **`long` is 64-bit on the guest and 32-bit on host Windows.** `strtol` returns a `long`, so
//!   it returns 64 bits; a handler that wrote 32 would truncate every large value and every
//!   negative one would still look right, which is the worst possible test coverage.
//! * **An `int` return must be sign-extended.** `Ret::i32` does it; `Ret::u64` does not. Every
//!   libc function that reports failure as `-1` becomes a success if this is wrong, and
//!   `strcmp`'s negative results become huge positive ones.
//! * **`size_t` is 64-bit.** Every length argument is `next_u64`, never `next_i32`.
//!
//! # `strcmp` and `memcmp` return a byte difference, not a sign
//!
//! `omni-bionic` returns `first_differing_a - first_differing_b` (so `'a' - 'z'` is `-25`), which
//! is bionic's documented behaviour where the C standard fixes only the sign. That value is
//! passed to the guest **unchanged**. Nothing here compares a comparison result against `1` or
//! `-1`, and nothing normalises one: guest code is entitled to the magnitude, and a handler that
//! clamped to `-1/0/1` would be changing an observable.
//!
//! # What is deliberately not here
//!
//! **Sockets and polling, and thread lifecycle.** Each needs host surface `omni-platform` still
//! does not have — sockets are phase 3c, thread lifecycle 3d — and each keeps the boundary's own
//! [`Binding::Unbound`](crate::Binding::Unbound), whose call names the symbol and the guest
//! address. That is exactly the failure that is wanted, rather than a plausible zero.
//!
//! Files and directories **are** here as of phase 3b, with bionic's `FILE *` layer on top of
//! them: eighteen descriptor symbols in `files` and eleven stream symbols in `stdio`. They are
//! the first group where a guest argument names something *outside* this process — a path — and
//! the whole of what stops that being a host file is `omni-platform`'s rooted filesystem, which
//! an instance does not have until the embedding supplies one.
//!
//! Clocks, process information and logging **are** here as of phase 3a, and with them the first
//! growth of `omni-platform` past `vm` and `fault`. Four of those symbols are bound and **refuse
//! by name** — `sysconf`, `sysinfo`, `prctl`, `syscall` — which is a different statement from
//! `Unbound`: `Unbound` says nothing implements this, and a refusal says *which* missing piece,
//! with the guest's own argument in it. See `procenv`'s module documentation, and in particular
//! why `sysconf` refuses two names this layer could otherwise answer.

use std::sync::Arc;

use omni_bionic::context::GuestContext;
use omni_bionic::error::BionicError;
use omni_bionic::memory::Fault;
use omni_mem::GuestAddr;

use crate::abi::Args;
use crate::boundary::{ImportCall, ImportFn, ReentrantCall, ReentrantFn};
use crate::error::{AbiError, AbiResult};

use super::view::GuestView;
use super::{
    active, clocks, dl, enter, files, format, guestmem, logging, net, procenv,
    runtime::CallThreads, signals, stdio, threads,
};

// ------------------------------------------------------------------ result lifting

/// Turn whatever an `omni-bionic` function returns into the boundary's result.
///
/// Three vocabularies meet here — [`Fault`], [`BionicError`] and a plain value — and each one
/// has to keep naming the symbol and the guest address on the way out.
trait Lift<T> {
    fn lift(self, view: &GuestView<'_>) -> AbiResult<T>;
}

impl<T> Lift<T> for Result<T, Fault> {
    fn lift(self, view: &GuestView<'_>) -> AbiResult<T> {
        self.map_err(|fault| view.fault(fault))
    }
}

impl<T> Lift<T> for Result<T, BionicError> {
    fn lift(self, view: &GuestView<'_>) -> AbiResult<T> {
        self.map_err(|error| match error {
            // A memory failure keeps the boundary's rich refusal, which names the argument and
            // which of `admit`'s rules said no.
            BionicError::Memory(fault) => view.fault(fault),
            // The rest are refusals with a reason, and the reason is the whole value of them:
            // a FORTIFY check that fired is a *detected buffer overflow in guest code*, and
            // reporting it as "bad pointer" would lose that.
            other => view.refusal(other.to_string()),
        })
    }
}

impl<T> Lift<T> for AbiResult<T> {
    fn lift(self, _view: &GuestView<'_>) -> AbiResult<T> {
        self
    }
}

macro_rules! lift_infallible {
    ($($t:ty),* $(,)?) => {
        $(
            impl Lift<$t> for $t {
                fn lift(self, _view: &GuestView<'_>) -> AbiResult<$t> {
                    Ok(self)
                }
            }
        )*
    };
}
lift_infallible!(i32, u64, f32, f64, ());

// ------------------------------------------------------------------ the handler macro

/// Take one argument in AAPCS64 order.
///
/// `ptr` and `u64` read the same 64 bits; they are spelled differently so a handler's signature
/// says which of its parameters are guest pointers, and so a reader can check the count of
/// pointers against the hostile-argument tests.
macro_rules! take {
    ($a:ident, u64) => {
        $a.next_u64()
    };
    ($a:ident, ptr) => {
        $a.next_u64()
    };
    ($a:ident, i32) => {
        $a.next_i32()
    };
    ($a:ident, f32) => {
        $a.next_f32()
    };
    ($a:ident, f64) => {
        $a.next_f64()
    };
}

/// Write the return value in the register AAPCS64 puts it in.
macro_rules! put {
    ($c:ident, u64, $v:expr) => {
        $c.ret().u64($v)
    };
    ($c:ident, i32, $v:expr) => {
        $c.ret().i32($v)
    };
    ($c:ident, f32, $v:expr) => {
        $c.ret().f32($v)
    };
    ($c:ident, f64, $v:expr) => {
        $c.ret().f64($v)
    };
    ($c:ident, void, $v:expr) => {{
        let () = $v;
        $c.ret().void()
    }};
}

/// Define inline handlers: arguments in, one `omni-bionic` call, result out.
///
/// The body is a closure over the guest view so that the view's borrow of the call ends before
/// the return value is written — [`ImportCall::args`] borrows shared and [`ImportCall::ret`]
/// borrows unique, and the two cannot overlap.
macro_rules! handlers {
    ($(
        $(#[$meta:meta])*
        fn $name:ident ( $($arg:ident : $kind:ident),* $(,)? ) -> $ret:ident = |$view:ident| $body:expr;
    )*) => {
        $(
            $(#[$meta])*
            #[allow(unused_variables)]
            pub(super) fn $name(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
                #[allow(unused_mut)]
                let mut a: Args<'_> = c.args();
                $( let $arg = take!(a, $kind)?; )*
                let state = active(c.symbol(), c.address())?;
                let value = {
                    #[allow(unused_mut)]
                    let mut $view = enter(c, &state);
                    let produced = $body;
                    Lift::lift(produced, &$view)?
                };
                put!(c, $ret, value);
                Ok(())
            }
        )*
    };
}

// ------------------------------------------------------------------ memory and strings

handlers! {
    /// `void *memcpy(void *dst, const void *src, size_t n)`
    fn memcpy(dst: ptr, src: ptr, n: u64) -> u64 =
        |v| omni_bionic::mem::memcpy(&mut v, dst, src, n);

    /// `void *memmove(void *dst, const void *src, size_t n)`
    fn memmove(dst: ptr, src: ptr, n: u64) -> u64 =
        |v| omni_bionic::mem::memmove(&mut v, dst, src, n);

    /// `void *memset(void *s, int c, size_t n)`
    fn memset(s: ptr, c: i32, n: u64) -> u64 = |v| omni_bionic::mem::memset(&mut v, s, c, n);

    /// `int memcmp(const void *a, const void *b, size_t n)` — bionic's byte difference, passed
    /// through as it is.
    fn memcmp(a: ptr, b: ptr, n: u64) -> i32 = |v| omni_bionic::mem::memcmp(&v, a, b, n);

    /// `void *memchr(const void *s, int c, size_t n)`
    fn memchr(s: ptr, c: i32, n: u64) -> u64 = |v| omni_bionic::mem::memchr(&v, s, c, n);

    /// `void *__memcpy_chk(void *dst, const void *src, size_t n, size_t dst_len)` — FORTIFY.
    fn memcpy_chk(dst: ptr, src: ptr, n: u64, dst_len: u64) -> u64 =
        |v| omni_bionic::mem::memcpy_chk(&mut v, dst, src, n, dst_len);

    /// `void *__memset_chk(void *dst, int c, size_t n, size_t dst_len)` — FORTIFY.
    fn memset_chk(dst: ptr, c: i32, n: u64, dst_len: u64) -> u64 =
        |v| omni_bionic::mem::memset_chk(&mut v, dst, c, n, dst_len);

    /// `size_t strlen(const char *s)`
    fn strlen(s: ptr) -> u64 = |v| omni_bionic::string::strlen(&v, s);

    /// `size_t __strlen_chk(const char *s, size_t s_len)` — FORTIFY.
    fn strlen_chk(s: ptr, s_len: u64) -> u64 = |v| omni_bionic::string::strlen_chk(&v, s, s_len);

    /// `size_t strnlen(const char *s, size_t n)`
    ///
    /// **Not among the 188 statically-reachable imports**, and M4's gate is what found it: the
    /// engine reaches it from `JNI_OnLoad`'s registration helpers, and D17 says in as many
    /// words that 188 is a *lower* bound with 17,698 unfollowable indirect call sites behind it.
    /// The implementation was already in `omni-bionic`; only the binding was missing.
    fn strnlen(s: ptr, n: u64) -> u64 = |v| omni_bionic::string::strnlen(&v, s, n);

    /// `int strcmp(const char *a, const char *b)` — byte difference, unchanged.
    fn strcmp(a: ptr, b: ptr) -> i32 = |v| omni_bionic::string::strcmp(&v, a, b);

    /// `int strncmp(const char *a, const char *b, size_t n)` — byte difference, unchanged.
    fn strncmp(a: ptr, b: ptr, n: u64) -> i32 = |v| omni_bionic::string::strncmp(&v, a, b, n);

    /// `int strcasecmp(const char *a, const char *b)`
    fn strcasecmp(a: ptr, b: ptr) -> i32 = |v| omni_bionic::string::strcasecmp(&v, a, b);

    /// `int strncasecmp(const char *a, const char *b, size_t n)`
    fn strncasecmp(a: ptr, b: ptr, n: u64) -> i32 =
        |v| omni_bionic::string::strncasecmp(&v, a, b, n);

    /// `char *strcpy(char *dst, const char *src)`
    fn strcpy(dst: ptr, src: ptr) -> u64 = |v| omni_bionic::string::strcpy(&mut v, dst, src);

    /// `char *strncpy(char *dst, const char *src, size_t n)`
    fn strncpy(dst: ptr, src: ptr, n: u64) -> u64 =
        |v| omni_bionic::string::strncpy(&mut v, dst, src, n);

    /// `char *__strncpy_chk2(char *dst, const char *src, size_t n, size_t dst_len, size_t src_len)`
    fn strncpy_chk2(dst: ptr, src: ptr, n: u64, dst_len: u64, src_len: u64) -> u64 =
        |v| omni_bionic::string::strncpy_chk2(&mut v, dst, src, n, dst_len, src_len);

    /// `char *strcat(char *dst, const char *src)`
    fn strcat(dst: ptr, src: ptr) -> u64 = |v| omni_bionic::string::strcat(&mut v, dst, src);

    /// `char *strchr(const char *s, int c)`
    fn strchr(s: ptr, c: i32) -> u64 = |v| omni_bionic::string::strchr(&v, s, c);

    /// `char *strrchr(const char *s, int c)`
    fn strrchr(s: ptr, c: i32) -> u64 = |v| omni_bionic::string::strrchr(&v, s, c);

    /// `char *strstr(const char *haystack, const char *needle)`
    fn strstr(haystack: ptr, needle: ptr) -> u64 =
        |v| omni_bionic::string::strstr(&v, haystack, needle);

    /// `int strerror_r(int errnum, char *buf, size_t buflen)` — the **POSIX** form, which
    /// returns `0` or `ERANGE`.
    ///
    /// **Found by M3's gate, at `init_array[3118]`.** `libroblox.so` imports both spellings;
    /// only the GNU one was in Task 1's 188.
    fn strerror_r(errnum: i32, buf: ptr, buflen: u64) -> i32 =
        |v| omni_bionic::string::strerror_r(&mut v, errnum, buf, buflen);

    /// `char *__gnu_strerror_r(int errnum, char *buf, size_t buflen)` — the GNU form, which
    /// returns a pointer and may or may not have used `buf`.
    fn gnu_strerror_r(errnum: i32, buf: ptr, buflen: u64) -> u64 =
        |v| omni_bionic::string::gnu_strerror_r(&mut v, errnum, buf, buflen);

    // ---------------------------------------------------------- numbers

    /// `int atoi(const char *s)`
    fn atoi(s: ptr) -> i32 = |v| omni_bionic::numerics::atoi(&mut v, s);

    /// `long long atoll(const char *s)`
    fn atoll(s: ptr) -> u64 = |v| omni_bionic::numerics::atoll(&mut v, s).map(|n| n as u64);

    /// `long strtol(const char *nptr, char **endptr, int base)` — **`long` is 64 bits** on the
    /// guest, whatever it is on the host.
    fn strtol(nptr: ptr, endptr: ptr, base: i32) -> u64 =
        |v| omni_bionic::numerics::strtol(&mut v, nptr, endptr, base).map(|n| n as u64);

    /// `long long strtoll(const char *nptr, char **endptr, int base)`
    fn strtoll(nptr: ptr, endptr: ptr, base: i32) -> u64 =
        |v| omni_bionic::numerics::strtoll(&mut v, nptr, endptr, base).map(|n| n as u64);

    /// `unsigned long strtoul(const char *nptr, char **endptr, int base)`
    fn strtoul(nptr: ptr, endptr: ptr, base: i32) -> u64 =
        |v| omni_bionic::numerics::strtoul(&mut v, nptr, endptr, base);

    /// `unsigned long long strtoull(const char *nptr, char **endptr, int base)`
    fn strtoull(nptr: ptr, endptr: ptr, base: i32) -> u64 =
        |v| omni_bionic::numerics::strtoull(&mut v, nptr, endptr, base);

    /// `double strtod(const char *nptr, char **endptr)`
    fn strtod(nptr: ptr, endptr: ptr) -> f64 =
        |v| omni_bionic::numerics::strtod(&mut v, nptr, endptr);

    /// `float strtof(const char *nptr, char **endptr)`
    fn strtof(nptr: ptr, endptr: ptr) -> f32 =
        |v| omni_bionic::numerics::strtof(&mut v, nptr, endptr);

    /// `int rand(void)`
    ///
    /// The sequence is `omni-bionic`'s LCG and is **not** bit-exact with bionic's. See the
    /// module-level note on the table below for why that is safe here.
    fn rand() -> i32 = |v| omni_bionic::numerics::rand(&mut v);

    // ---------------------------------------------------------- libm

    /// `double exp(double x)`
    fn exp(x: f64) -> f64 = |v| omni_bionic::libm::exp(&mut v, x);
    /// `float expf(float x)`
    fn expf(x: f32) -> f32 = |v| omni_bionic::libm::expf(&mut v, x);
    /// `double log(double x)`
    fn log(x: f64) -> f64 = |v| omni_bionic::libm::log(&mut v, x);
    /// `float logf(float x)`
    fn logf(x: f32) -> f32 = |v| omni_bionic::libm::logf(&mut v, x);
    /// `double pow(double x, double y)`
    fn pow(x: f64, y: f64) -> f64 = |v| omni_bionic::libm::pow(&mut v, x, y);
    /// `float powf(float x, float y)`
    fn powf(x: f32, y: f32) -> f32 = |v| omni_bionic::libm::powf(&mut v, x, y);
    /// `float sinf(float x)`
    fn sinf(x: f32) -> f32 = |v| omni_bionic::libm::sinf(&mut v, x);
    /// `float cosf(float x)`
    fn cosf(x: f32) -> f32 = |v| omni_bionic::libm::cosf(&mut v, x);
    /// `float tanf(float x)`
    fn tanf(x: f32) -> f32 = |v| omni_bionic::libm::tanf(&mut v, x);
    /// `float tanhf(float x)`
    fn tanhf(x: f32) -> f32 = |v| omni_bionic::libm::tanhf(&mut v, x);
    /// `float acosf(float x)`
    fn acosf(x: f32) -> f32 = |v| omni_bionic::libm::acosf(&mut v, x);
    /// `double nan(const char *tagp)`
    fn nan(tagp: ptr) -> f64 = |v| omni_bionic::libm::nan(tagp);
    /// `double frexp(double x, int *exp)`
    fn frexp(x: f64, exp_ptr: ptr) -> f64 = |v| omni_bionic::libm::frexp(&mut v, x, exp_ptr);
    /// `void sincosf(float x, float *sin, float *cos)`
    fn sincosf(x: f32, sin_ptr: ptr, cos_ptr: ptr) -> void =
        |v| omni_bionic::libm::sincosf(&mut v, x, sin_ptr, cos_ptr);

    // ---------------------------------------------------------- wide characters

    /// `size_t __ctype_get_mb_cur_max(void)` — what the `MB_CUR_MAX` macro expands to.
    ///
    /// **Found by M3's gate, at `init_array[2]`.** Not in Task 1's 188 and not in its Tier C
    /// section either.
    fn ctype_get_mb_cur_max() -> u64 = |v| omni_bionic::locale::ctype_get_mb_cur_max();

    /// `int mbtowc(wchar_t *pwc, const char *s, size_t n)`
    ///
    /// **Found by M3's gate, at `init_array[2]`, and it is the sharpest thing the gate found
    /// about the static prediction**: `mbtowc` is not among Task 1's 188 *and is not in the Tier
    /// C address-taken section either* -- `init-reachable-imports.txt` files it under "never
    /// referenced from the Tier C closure at all". `tools/call_sites.py` finds exactly one direct
    /// call site, at `0x2b772f4`, passing `n = 4`, which is `MB_CUR_MAX` for UTF-8.
    fn mbtowc(pwc: ptr, s: ptr, n: u64) -> i32 =
        |v| omni_bionic::wide::mbtowc(&mut v, pwc, s, n);

    // ---------------------------------------------------------- locale

    /// `locale_t newlocale(int category_mask, const char *locale, locale_t base)`
    fn newlocale(category_mask: i32, locale: ptr, base: u64) -> u64 =
        |v| omni_bionic::locale::newlocale(&mut v, category_mask, locale, base);

    /// `locale_t uselocale(locale_t newloc)` — install `newloc` for the calling thread and
    /// return what was there.
    ///
    /// **Found by the M3 gate, at `init_array[2]`.** It is not among Task 1's 188: the static
    /// closure put it in the Tier C section, reached only through an address-taken edge — and
    /// the engine calls it directly, third initializer in. The slot it reads and writes is the
    /// one in this thread's arena block ([`GuestView::locale_address`]), because `omni-bionic`
    /// refuses to invent per-thread storage and answers `Unimplemented` for a zero slot instead.
    fn uselocale(newloc: u64) -> u64 = |v| {
        let slot = v.locale_address() as u64;
        omni_bionic::locale::uselocale(&mut v, slot, newloc)
    };

    /// `void freelocale(locale_t locobj)` — releases nothing, because `newlocale` allocates
    /// nothing: the only handle this layer produces is the static C-locale sentinel.
    fn freelocale(locobj: u64) -> void = |v| omni_bionic::locale::freelocale(locobj);

    // ---------------------------------------------------------- pthread: identity

    /// `pthread_t pthread_self(void)` — 64 bits, never a host thread id.
    fn pthread_self() -> u64 = |v| v.active.thread.0;

    /// `int pthread_equal(pthread_t a, pthread_t b)`
    fn pthread_equal(a: u64, b: u64) -> i32 = |v| omni_bionic::metadata::equal(
        omni_bionic::threads::GuestThreadId(a),
        omni_bionic::threads::GuestThreadId(b),
    );

    /// `int sched_yield(void)`
    fn sched_yield() -> i32 = |v| omni_bionic::metadata::sched_yield(&v.active.bionic.yielder);

    /// `int sched_get_priority_max(int policy)` -- Linux's constant per policy.
    fn sched_get_priority_max(policy: i32) -> i32 =
        |v| omni_bionic::metadata::sched_get_priority_max(policy);

    /// `int sched_get_priority_min(int policy)` -- the mirror of the above.
    fn sched_get_priority_min(policy: i32) -> i32 =
        |v| omni_bionic::metadata::sched_get_priority_min(policy);

    /// `int sched_setscheduler(pid_t pid, int policy, const struct sched_param *param)`
    ///
    /// **`-1`/`EPERM`, which is what a device answers**, not a refusal and not a lie. At guest
    /// `0x022077fc` the engine asks for `SCHED_FIFO` on itself, three instructions after taking
    /// `sched_get_priority_max(SCHED_FIFO)`; an ordinary Android application has no
    /// `CAP_SYS_NICE`, so the kernel refuses that call and the app runs at its normal policy.
    /// That is the answer on every non-rooted device, so returning it is modelling the platform
    /// rather than papering over a gap -- and the call site **ignores the result**, which is what
    /// a caller written for a request it expects to be denied looks like.
    ///
    /// Answering `0` would be the believable wrong answer: the engine would then believe its
    /// render or audio thread runs at real-time priority, and every latency decision downstream
    /// of that belief would be made on it.
    fn sched_setscheduler(pid: i32, policy: i32, param: ptr) -> i32 = |v| {
        let _ = (pid, policy, param);
        v.set_errno(omni_bionic::errno::consts::EPERM);
        -1
    };

    /// `int *__errno(void)` — the address of **this** guest thread's `errno`.
    fn errno_location() -> u64 = |v| v.errno_address() as u64;

    // ---------------------------------------------------------- pthread: attributes

    /// `int pthread_attr_init(pthread_attr_t *attr)`
    fn pthread_attr_init(attr: ptr) -> i32 = |v| omni_bionic::metadata::attr_init(&mut v, attr);

    /// `int pthread_attr_destroy(pthread_attr_t *attr)`
    fn pthread_attr_destroy(attr: ptr) -> i32 =
        |v| omni_bionic::metadata::attr_destroy(&mut v, attr);

    /// `int pthread_attr_setstacksize(pthread_attr_t *attr, size_t size)`
    fn pthread_attr_setstacksize(attr: ptr, size: u64) -> i32 =
        |v| omni_bionic::metadata::attr_setstacksize(&mut v, attr, size);

    /// `int pthread_attr_setdetachstate(pthread_attr_t *attr, int state)`
    ///
    /// **Bound in M5, and it was a pure binding gap**: `omni_bionic::metadata::attr_setdetachstate`
    /// has existed since phase 3c and nothing called it. M5's gate found it —
    /// `GameActivity_onCreate` step 3 sets `PTHREAD_CREATE_DETACHED` on the game thread's
    /// attributes before `pthread_create`, and the run stopped there with `Unbound` naming the
    /// symbol, which is exactly the failure that shape is supposed to produce.
    ///
    /// `pthread_create` already reads the detach state out of the attribute object at
    /// `threads::ATTR_DETACH_STATE`, so binding this is the whole of what was missing: without it
    /// the glue's game thread would have been created **joinable**, and nothing joins it.
    fn pthread_attr_setdetachstate(attr: ptr, state: i32) -> i32 =
        |v| omni_bionic::metadata::attr_setdetachstate(&mut v, attr, state);

    // ---------------------------------------------------------- pthread: mutex

    /// `int pthread_mutexattr_init(pthread_mutexattr_t *attr)`
    fn pthread_mutexattr_init(attr: ptr) -> i32 =
        |v| omni_bionic::mutex::attr_init(&mut v, attr).map(|()| 0);

    /// `int pthread_mutexattr_destroy(pthread_mutexattr_t *attr)`
    fn pthread_mutexattr_destroy(attr: ptr) -> i32 =
        |v| omni_bionic::mutex::attr_destroy(&mut v, attr).map(|()| 0);

    /// `int pthread_mutexattr_settype(pthread_mutexattr_t *attr, int type)`
    fn pthread_mutexattr_settype(attr: ptr, kind: i32) -> i32 =
        |v| omni_bionic::mutex::attr_settype(&mut v, attr, kind);

    /// `int pthread_mutex_init(pthread_mutex_t *m, const pthread_mutexattr_t *attr)`
    fn pthread_mutex_init(m: ptr, attr: ptr) -> i32 =
        |v| omni_bionic::mutex::init(&mut v, m, attr);

    // ---------------------------------------------------------- pthread: rwlock

    /// `int pthread_rwlock_init(pthread_rwlock_t *rw, const pthread_rwlockattr_t *attr)`
    fn pthread_rwlock_init(rw: ptr, attr: ptr) -> i32 =
        |v| omni_bionic::rwlock::init(&mut v, rw, attr);

    /// `int pthread_rwlock_destroy(pthread_rwlock_t *rw)`
    fn pthread_rwlock_destroy(rw: ptr) -> i32 = |v| omni_bionic::rwlock::destroy(&mut v, rw);

    // ---------------------------------------------------------- pthread: cond

    /// `int pthread_cond_init(pthread_cond_t *c, const pthread_condattr_t *attr)`
    fn pthread_cond_init(c: ptr, attr: ptr) -> i32 = |v| omni_bionic::cond::init(&mut v, c, attr);

    // ---------------------------------------------------------- C++ runtime

    /// `int __cxa_atexit(void (*func)(void *), void *arg, void *dso_handle)`
    ///
    /// Registration only. Running the list is a shutdown action the host performs through
    /// [`Bionic::atexit`](super::Bionic::atexit), because each entry is a **guest** call and an
    /// inline handler structurally cannot make one.
    fn cxa_atexit(func: ptr, arg: ptr, dso: ptr) -> i32 =
        |v| v.active.bionic.atexit.register(func as u64, arg as u64, dso as u64);

    /// `int __cxa_thread_atexit_impl(void (*func)(void *), void *arg, void *dso_handle)`
    fn cxa_thread_atexit(func: ptr, arg: ptr, dso: ptr) -> i32 =
        |v| v.active.bionic.tls.thread_atexit(v.active.thread, func as u64, arg as u64, dso as u64);
}

// ------------------------------------------------------------------ handlers that need more

/// `char *strerror(int errnum)`
///
/// Returns a pointer to **this thread's** scratch buffer, which is what bionic does: the message
/// outlives the call and two threads must not share one buffer. A message that does not fit is a
/// named refusal, never a truncation.
pub(super) fn strerror(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let errnum = c.args().next_i32()?;
    let state = active(c.symbol(), c.address())?;
    let at = {
        let view = enter(c, &state);
        let message = omni_bionic::string::strerror_message(errnum);
        view.put_scratch(message.as_bytes())?
    };
    c.ret().u64(at as u64);
    Ok(())
}

/// `int pthread_setname_np(pthread_t thread, const char *name)`
pub(super) fn pthread_setname_np(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (thread, name_ptr) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let code = {
        let view = enter(c, &state);
        if name_ptr == 0 {
            omni_bionic::errno::consts::EINVAL
        } else {
            let at = usize::try_from(name_ptr)
                .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
            let bytes = view.mem().cstr(at, crate::mem::Blame::new(view.symbol(), view.address(), 1))?;
            // Lossy rather than a refusal: a thread name is a label, the guest is under no
            // obligation to make it UTF-8, and refusing a `pthread_setname_np` because of its
            // encoding would fail a correct program.
            let name = String::from_utf8_lossy(&bytes).into_owned();
            omni_bionic::metadata::setname(
                &state.bionic.names,
                omni_bionic::threads::GuestThreadId(thread),
                Some(&name),
            )
        }
    };
    c.ret().i32(code);
    Ok(())
}

/// Handlers whose `omni-bionic` function needs the thread registry, the futex and the owner
/// table together. Written out rather than macro-generated because the capability set differs
/// per function and spelling it makes the dependency visible.
macro_rules! sync_handler {
    ($(
        $(#[$meta:meta])*
        fn $name:ident ( $($arg:ident),* $(,)? ) = |$view:ident, $threads:ident, $futex:ident, $owners:ident, $conds:ident| $body:expr;
    )*) => {
        $(
            $(#[$meta])*
            #[allow(unused_variables)]
            pub(super) fn $name(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
                #[allow(unused_mut)]
                let mut a: Args<'_> = c.args();
                $( let $arg = a.next_u64()?; )*
                let state = active(c.symbol(), c.address())?;
                let code = {
                    #[allow(unused_mut)]
                    let mut $view = enter(c, &state);
                    let $threads = CallThreads {
                        table: &state.bionic.threads,
                        me: state.thread,
                    };
                    let $futex = &state.bionic.futex;
                    let $owners: &omni_bionic::mutex::OwnerTable = &state.bionic.owners;
                    let $conds: &omni_bionic::cond::CondWaiters = &state.bionic.conds;
                    let produced = $body;
                    Lift::lift(produced, &$view)?
                };
                c.ret().i32(code);
                Ok(())
            }
        )*
    };
}

sync_handler! {
    /// `int pthread_mutex_destroy(pthread_mutex_t *m)`
    fn pthread_mutex_destroy(m) = |v, threads, futex, owners, conds|
        omni_bionic::mutex::destroy(&mut v, owners, m);

    /// `int pthread_mutex_lock(pthread_mutex_t *m)` — blocks the calling guest thread.
    fn pthread_mutex_lock(m) = |v, threads, futex, owners, conds|
        omni_bionic::mutex::lock(&mut v, futex, owners, &threads, m);

    /// `int pthread_mutex_unlock(pthread_mutex_t *m)`
    fn pthread_mutex_unlock(m) = |v, threads, futex, owners, conds|
        omni_bionic::mutex::unlock(&mut v, futex, owners, &threads, m);

    /// `int pthread_rwlock_rdlock(pthread_rwlock_t *rw)`
    fn pthread_rwlock_rdlock(rw) = |v, threads, futex, owners, conds|
        omni_bionic::rwlock::rdlock(&mut v, futex, rw);

    /// `int pthread_rwlock_wrlock(pthread_rwlock_t *rw)`
    fn pthread_rwlock_wrlock(rw) = |v, threads, futex, owners, conds|
        omni_bionic::rwlock::wrlock(&mut v, futex, rw);

    /// `int pthread_rwlock_unlock(pthread_rwlock_t *rw)`
    fn pthread_rwlock_unlock(rw) = |v, threads, futex, owners, conds|
        omni_bionic::rwlock::unlock(&mut v, futex, rw);

    /// `int pthread_cond_destroy(pthread_cond_t *c)`
    fn pthread_cond_destroy(cond) = |v, threads, futex, owners, conds|
        omni_bionic::cond::destroy(&mut v, conds, cond);

    /// `int pthread_cond_signal(pthread_cond_t *c)`
    fn pthread_cond_signal(cond) = |v, threads, futex, owners, conds|
        omni_bionic::cond::signal(conds, cond);

    /// `int pthread_cond_broadcast(pthread_cond_t *c)`
    fn pthread_cond_broadcast(cond) = |v, threads, futex, owners, conds|
        omni_bionic::cond::broadcast(conds, cond);
}

/// `int pthread_cond_wait(pthread_cond_t *c, pthread_mutex_t *m)`
///
/// Two phases, and the order is the atomicity: the calling thread registers on the cond's waiter
/// list *before* the mutex is released, so a signal delivered after registration is already
/// addressed to it. The mutex is reacquired on every exit path, including the error ones.
///
/// `with_owners` and `with_registry` publish the owner table and this thread's identity as
/// ambient state, because the relock on the way out happens inside `wait_end`, below the layer
/// that was handed them.
pub(super) fn pthread_cond_wait(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (cond, mutex) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    // **§8.1's fifth failure mode, recorded while it is happening.** §8 row 14 blocks here until
    // the game thread signals `app->running`, and a deadlock there looks exactly like a hang from
    // outside. The guard removes the entry on **every** exit including the failing ones, which a
    // matched pair of calls around a `?` would not.
    let _parked = state.bionic.park("pthread_cond_wait", state.thread, cond, mutex);
    let code = {
        let mut view = enter(c, &state);
        let threads = CallThreads { table: &state.bionic.threads, me: state.thread };
        let owners = Arc::clone(&state.bionic.owners);
        let conds = &state.bionic.conds;
        let futex = &state.bionic.futex;
        let produced = omni_bionic::cond::with_owners(Arc::clone(&owners), || {
            omni_bionic::cond::with_registry(&threads, || {
                omni_bionic::cond::wait_begin(
                    &mut view, &owners, &threads, conds, cond, mutex,
                )?;
                omni_bionic::cond::wait_end(
                    &threads, conds, cond, mutex, &mut view, futex, None,
                )
            })
        });
        Lift::lift(produced, &view)?
    };
    c.ret().i32(code);
    Ok(())
}

/// `int pthread_cond_timedwait(pthread_cond_t *c, pthread_mutex_t *m, const struct timespec *abs)`
///
/// [`pthread_cond_wait`] with a deadline, and everything that makes that call correct — the
/// registration before the release, the relock on every exit path, the park record — is the same
/// here because it is the same two calls into `omni-bionic`. The only thing this adds is turning
/// an **absolute** deadline into the relative duration `cond::wait_end` takes.
///
/// # What the engine actually calls this for, MEASURED
///
/// `jni-surface.md` §8 rows 19-20 go through the GameActivity glue's
/// `android_app_set_activity_state`, at guest `0x0285f760`, and this was decoded from the binary
/// rather than from a header:
///
/// ```text
/// 0x285f794: bl  pthread_mutex_lock        ; app + 0xc8
/// 0x285f7a8: bl  write                     ; app->msgwrite, &cmd, 1
/// 0x285f7bc: bl  clock_gettime             ; w0 = 0  -> CLOCK_REALTIME
/// 0x285f7cc: add x8, x8, #2                ; ts.tv_sec += 2
/// 0x285f7ec: bl  pthread_cond_timedwait    ; (app + 0xf0, app + 0xc8, &ts)
/// 0x285f7f0: cmp w0, #0x6e                 ; ETIMEDOUT == 110
/// ```
///
/// So the deadline is absolute, two seconds out, and on `CLOCK_REALTIME` — and the `cmp w0,
/// #0x6e` is independent confirmation of `omni_bionic::errno::ETIMEDOUT`, from the guest that has
/// to agree with it.
///
/// **The clock is read back from the cond rather than assumed to be that one**, because the
/// binary also imports `pthread_condattr_setclock` and uses it once, off this path. `cond::init`
/// records the selector in a field `omni-bionic` defined, so [`omni_bionic::cond::clock_of`] is
/// reading this layer's own convention — not a guess at bionic's internal bit layout, which
/// there is no bionic source on this host to check.
///
/// # A deadline already past is not an error
///
/// POSIX: the call still releases the mutex, and still reacquires it, before returning
/// `ETIMEDOUT`. A zero duration through `wait_end` does exactly that, so the past-deadline case
/// needs no arm of its own — and an early return that skipped the two-phase wait would skip the
/// release the caller's own loop depends on.
pub(super) fn pthread_cond_timedwait(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (cond, mutex, abstime) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let (clock, deadline) = {
        let mut view = enter(c, &state);
        let clock = match Lift::lift(omni_bionic::cond::clock_of(&mut view, cond), &view)? {
            Ok(clock) => clock,
            Err(code) => {
                // The selector is one `cond::init` never writes, so the struct is not a
                // `pthread_cond_t` this layer produced. Reported as the code bionic reports
                // rather than refused: `EINVAL` is exactly what a caller passing a bad cond
                // gets, and it has an arm for it.
                c.ret().i32(code);
                return Ok(());
            }
        };
        let blame = crate::mem::Blame::new(view.symbol(), view.address(), 2);
        let at = usize::try_from(abstime)
            .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
        // `struct timespec` on LP64 bionic: `time_t tv_sec` then `long tv_nsec`, both 8 bytes.
        // The same layout `clock_gettime` writes one call earlier at the measured call site, so
        // the two agree by construction rather than by two separate transcriptions.
        let seconds = view.mem().read_u64(at, blame)? as i64;
        let nanos = view.mem().read_u64(at + 8, blame)? as i64;
        (clock, (seconds, nanos))
    };
    let (seconds, nanos) = deadline;
    if !(0..1_000_000_000).contains(&nanos) {
        // bionic's own validation, and the reason it is not a refusal: `EINVAL` is what the
        // caller is told on a device, and inventing a wait for an unrepresentable time would be
        // the believable wrong answer.
        c.ret().i32(omni_bionic::errno::consts::EINVAL);
        return Ok(());
    }
    let now = match clock {
        omni_bionic::cond::clock_id::CLOCK_MONOTONIC => omni_platform::clock::monotonic_now(),
        _ => omni_platform::clock::realtime_now(),
    };
    let absolute = std::time::Duration::new(
        u64::try_from(seconds).unwrap_or(0),
        u32::try_from(nanos).unwrap_or(0),
    );
    // **Saturating rather than checked**, and that is the whole of the past-deadline case: a
    // deadline already gone is a zero wait, which `wait_end` turns into a release, a relock and
    // ETIMEDOUT. `seconds` below zero lands here too, through the `unwrap_or(0)` above.
    let budget = absolute.saturating_sub(now);
    if budget.as_secs() > super::MAX_SLEEP_SECONDS {
        return Err(AbiError::Refused {
            symbol: c.symbol().to_string(),
            address: c.address(),
            why: format!(
                "the guest asked `pthread_cond_timedwait` to wait {budget:?} -- an absolute \
                 deadline of {seconds}.{nanos:09} on {} -- and this layer caps a guest-chosen \
                 wait at {} seconds, the same cap `nanosleep`, `poll`, `select` \
                 and `ALooper_pollOnce` name. Clamping was rejected for their reason: returning \
                 ETIMEDOUT at the cap reports a deadline that has not passed",
                match clock {
                    omni_bionic::cond::clock_id::CLOCK_MONOTONIC => "CLOCK_MONOTONIC",
                    _ => "CLOCK_REALTIME",
                },
                super::MAX_SLEEP_SECONDS
            ),
        });
    }

    let _parked = state.bionic.park("pthread_cond_timedwait", state.thread, cond, mutex);
    let code = {
        let mut view = enter(c, &state);
        let threads = CallThreads { table: &state.bionic.threads, me: state.thread };
        let owners = Arc::clone(&state.bionic.owners);
        let conds = &state.bionic.conds;
        let futex = &state.bionic.futex;
        let produced = omni_bionic::cond::with_owners(Arc::clone(&owners), || {
            omni_bionic::cond::with_registry(&threads, || {
                omni_bionic::cond::wait_begin(
                    &mut view, &owners, &threads, conds, cond, mutex,
                )?;
                omni_bionic::cond::wait_end(
                    &threads, conds, cond, mutex, &mut view, futex, Some(budget),
                )
            })
        });
        Lift::lift(produced, &view)?
    };
    c.ret().i32(code);
    Ok(())
}

/// `int pthread_key_create(pthread_key_t *key, void (*destructor)(void *))`
///
/// `pthread_key_t` is a 32-bit `int` on bionic, so the key is written as four bytes. The
/// destructor is recorded and **not** called here: running it is a guest call at thread exit,
/// which belongs to the lifecycle phase.
pub(super) fn pthread_key_create(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (key_ptr, destructor) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let code = {
        let view = enter(c, &state);
        match state.bionic.tls.key_create(destructor) {
            Err(code) => code,
            Ok(key) => {
                let at = usize::try_from(key_ptr)
                    .map_err(|_| view.refusal("a guest pointer wider than the host's usize"))?;
                view.mem().write_u32(
                    at,
                    key,
                    crate::mem::Blame::new(view.symbol(), view.address(), 0),
                )?;
                0
            }
        }
    };
    c.ret().i32(code);
    Ok(())
}

/// `void *pthread_getspecific(pthread_key_t key)`
pub(super) fn pthread_getspecific(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let key = c.args().next_i32()?;
    let state = active(c.symbol(), c.address())?;
    // A negative key is not a key: `pthread_key_t` is unsigned in use, and `getspecific` on an
    // invalid key is defined to return NULL rather than to fail.
    let value = if key < 0 { 0 } else { state.bionic.tls.getspecific(state.thread, key as u32) };
    c.ret().u64(value);
    Ok(())
}

/// `int pthread_setspecific(pthread_key_t key, const void *value)`
pub(super) fn pthread_setspecific(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (key, value) = {
        let mut a = c.args();
        (a.next_i32()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let code = if key < 0 {
        omni_bionic::errno::consts::EINVAL
    } else {
        state.bionic.tls.setspecific(state.thread, key as u32, value)
    };
    c.ret().i32(code);
    Ok(())
}

// ------------------------------------------------------------------ the two re-entrant ones

/// `int pthread_once(pthread_once_t *once_control, void (*init_routine)(void))`
///
/// Re-entrant because the initialiser is **guest code**. An inline handler structurally cannot
/// call it: `ImportCall` holds no CPU, which is not a convention but the thing that stops a
/// second `&mut CpuCtx` existing while the first is live.
///
/// A failure inside the initialiser is reported with the *inner* symbol named, and the control
/// word is left as `omni-bionic` left it — which is `DONE`, because that crate publishes the
/// state after `run_init` returns and has no error channel to be told the routine did not
/// finish. Stated rather than worked around: the failure propagates out of `Boundary::run`, so
/// no guest code observes the `DONE` it would otherwise be misled by.
pub(super) fn pthread_once(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (once_addr, init) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let symbol = c.symbol().to_string();
    let address = c.address();
    let state = active(&symbol, address)?;
    // Cloned rather than borrowed: `ReentrantCall::mem` borrows the call shared and
    // `call_guest` borrows it unique, and the view has to outlive the callback. `GuestMem` is an
    // `Arc` and nothing else, so the clone is one increment.
    let mem = c.mem().clone();
    let mut init_failure: Option<AbiError> = None;
    let outcome = {
        let mut view = GuestView::new(&mem, &symbol, address, &state);
        let futex = &state.bionic.futex;
        let produced = omni_bionic::once::once(&mut view, futex, once_addr, || {
            if init_failure.is_some() {
                return;
            }
            // AAPCS64: `void (*)(void)` takes no arguments. The budget is the caller's, which is
            // what stops an initialiser that never returns.
            if let Err(error) = c.call_guest(init as GuestAddr, &[], omni_cpu::RunLimit::Unlimited)
            {
                init_failure = Some(error);
            }
        });
        Lift::lift(produced, &view)?
    };
    if let Some(error) = init_failure {
        return Err(error);
    }
    let _ = outcome;
    c.ret(|mut r| r.i32(0));
    Ok(())
}

/// The guest's `int (*compar)(const void *, const void *)`, called through the boundary.
struct GuestComparator<'a, 'b> {
    call: &'a mut ReentrantCall<'b>,
    target: GuestAddr,
    /// The first failure, kept because [`omni_bionic::guestcmp::GuestCompare`] can only report a
    /// [`Fault`] and this one has a symbol, an address and a reason.
    failure: Option<AbiError>,
}

impl omni_bionic::guestcmp::GuestCompare for GuestComparator<'_, '_> {
    fn compare(
        &mut self,
        _mem: &impl omni_bionic::memory::GuestMemory,
        a: u64,
        b: u64,
    ) -> Result<i32, Fault> {
        if self.failure.is_some() {
            return Err(Fault(self.target as u64));
        }
        let args = [
            crate::boundary::GuestArg::Pointer(a as GuestAddr),
            crate::boundary::GuestArg::Pointer(b as GuestAddr),
        ];
        match self.call.call_guest(self.target, &args, omni_cpu::RunLimit::Unlimited) {
            // `as_i32` sign-extends from the low 32 bits of `X0`, which is the whole content of
            // a comparator's answer. The magnitude is passed through: `qsort` branches on the
            // sign and nothing here normalises to -1/0/1.
            Ok(result) => Ok(result.as_i32()),
            Err(error) => {
                self.failure = Some(error);
                Err(Fault(self.target as u64))
            }
        }
    }
}

/// `void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *))`
///
/// Re-entrant: the comparator is guest code. Every comparison is a full host-to-guest crossing,
/// which is the 80-102 ns path rather than the 26.7-31.0 ns one — correct rather than fast, and
/// `qsort` is not on any hot path the initializers take.
pub(super) fn qsort(c: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let (base, nmemb, size, compar) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?, a.next_u64()?, a.next_u64()?)
    };
    let symbol = c.symbol().to_string();
    let address = c.address();
    let state = active(&symbol, address)?;
    let mem = c.mem().clone();
    let target = usize::try_from(compar).map_err(|_| AbiError::Refused {
        symbol: symbol.clone(),
        address,
        why: "a comparator pointer wider than the host's usize".to_string(),
    })?;
    let mut view = GuestView::new(&mem, &symbol, address, &state);
    let mut comparator = GuestComparator { call: c, target, failure: None };
    let result = omni_bionic::sort::qsort(&mut view, &mut comparator, base, nmemb, size);
    if let Some(error) = comparator.failure.take() {
        return Err(error);
    }
    Lift::lift(result, &view)?;
    drop(view);
    c.ret(|r| r.void());
    Ok(())
}

// ------------------------------------------------------------------ the tables

/// Serviced **inside** the run loop: pure host work, no guest code, ≈27-31 ns per call.
pub(super) static INLINE: &[(&str, ImportFn)] = &[
    // memory
    ("memcpy", memcpy),
    ("memmove", memmove),
    ("memset", memset),
    ("memcmp", memcmp),
    ("memchr", memchr),
    ("__memcpy_chk", memcpy_chk),
    ("__memset_chk", memset_chk),
    // strings
    ("strlen", strlen),
    ("strnlen", strnlen),
    ("__strlen_chk", strlen_chk),
    ("strcmp", strcmp),
    ("strncmp", strncmp),
    ("strcasecmp", strcasecmp),
    ("strncasecmp", strncasecmp),
    ("strcpy", strcpy),
    ("strncpy", strncpy),
    ("__strncpy_chk2", strncpy_chk2),
    ("strcat", strcat),
    ("strchr", strchr),
    ("strrchr", strrchr),
    ("strstr", strstr),
    ("strerror", strerror),
    ("__gnu_strerror_r", gnu_strerror_r),
    ("strerror_r", strerror_r),
    // numbers
    ("atoi", atoi),
    ("atoll", atoll),
    ("strtol", strtol),
    ("strtoll", strtoll),
    ("strtoul", strtoul),
    ("strtoull", strtoull),
    ("strtod", strtod),
    ("strtof", strtof),
    ("rand", rand),
    // libm
    ("exp", exp),
    ("expf", expf),
    ("log", log),
    ("logf", logf),
    ("pow", pow),
    ("powf", powf),
    ("sinf", sinf),
    ("cosf", cosf),
    ("tanf", tanf),
    ("tanhf", tanhf),
    ("acosf", acosf),
    ("nan", nan),
    ("frexp", frexp),
    ("sincosf", sincosf),
    // wide characters
    ("__ctype_get_mb_cur_max", ctype_get_mb_cur_max),
    ("mbtowc", mbtowc),
    // locale
    ("newlocale", newlocale),
    ("uselocale", uselocale),
    ("freelocale", freelocale),
    // pthread identity and scheduling
    ("pthread_self", pthread_self),
    ("pthread_equal", pthread_equal),
    ("pthread_setname_np", pthread_setname_np),
    ("sched_yield", sched_yield),
    ("sched_get_priority_max", sched_get_priority_max),
    ("sched_get_priority_min", sched_get_priority_min),
    ("sched_setscheduler", sched_setscheduler),
    ("__errno", errno_location),
    // pthread attributes
    ("pthread_attr_init", pthread_attr_init),
    ("pthread_attr_destroy", pthread_attr_destroy),
    ("pthread_attr_setstacksize", pthread_attr_setstacksize),
    ("pthread_attr_setdetachstate", pthread_attr_setdetachstate),
    // pthread mutex
    ("pthread_mutexattr_init", pthread_mutexattr_init),
    ("pthread_mutexattr_destroy", pthread_mutexattr_destroy),
    ("pthread_mutexattr_settype", pthread_mutexattr_settype),
    ("pthread_mutex_init", pthread_mutex_init),
    ("pthread_mutex_destroy", pthread_mutex_destroy),
    ("pthread_mutex_lock", pthread_mutex_lock),
    ("pthread_mutex_unlock", pthread_mutex_unlock),
    // pthread rwlock
    ("pthread_rwlock_init", pthread_rwlock_init),
    ("pthread_rwlock_destroy", pthread_rwlock_destroy),
    ("pthread_rwlock_rdlock", pthread_rwlock_rdlock),
    ("pthread_rwlock_wrlock", pthread_rwlock_wrlock),
    ("pthread_rwlock_unlock", pthread_rwlock_unlock),
    // pthread cond
    ("pthread_cond_init", pthread_cond_init),
    ("pthread_cond_destroy", pthread_cond_destroy),
    ("pthread_cond_signal", pthread_cond_signal),
    ("pthread_cond_broadcast", pthread_cond_broadcast),
    ("pthread_cond_wait", pthread_cond_wait),
    ("pthread_cond_timedwait", pthread_cond_timedwait),
    // pthread TLS
    ("pthread_key_create", pthread_key_create),
    ("pthread_getspecific", pthread_getspecific),
    ("pthread_setspecific", pthread_setspecific),
    // C++ runtime
    ("__cxa_atexit", cxa_atexit),
    ("__cxa_thread_atexit_impl", cxa_thread_atexit),
    // libdl: four that refuse by name rather than issue a handle they cannot honour.
    // `dl_iterate_phdr` is the fifth and is on the exit path, because it calls a guest callback.
    ("dlerror", dl::dlerror),
    // the printf family
    ("snprintf", format::snprintf),
    ("vsnprintf", format::vsnprintf),
    ("__vsnprintf_chk", format::vsnprintf_chk),
    ("fprintf", format::fprintf),
    ("vfprintf", format::vfprintf),
    ("vasprintf", format::vasprintf),
    ("sscanf", format::sscanf),
    ("fscanf", format::fscanf),
    // ---- phase 3a: clocks. Answers from `omni-platform`'s clock seam; `gmtime_r` from
    // `omni-bionic`'s calendar arithmetic, which needs no clock at all.
    ("clock_gettime", clocks::clock_gettime),
    ("gettimeofday", clocks::gettimeofday),
    ("gmtime", clocks::gmtime),
    ("strftime", clocks::strftime),
    ("gmtime_r", clocks::gmtime_r),
    ("nanosleep", clocks::nanosleep),
    ("usleep", clocks::usleep),
    // ---- phase 3a: process and environment. Four answers, two facts about a process that was
    // given nothing, two terminations reported rather than performed, and four refusals by name.
    ("getpid", procenv::getpid),
    ("sched_getcpu", procenv::sched_getcpu),
    ("arc4random_buf", procenv::arc4random_buf),
    ("getauxval", procenv::getauxval),
    ("getenv", procenv::getenv),
    ("__system_property_get", procenv::system_property_get),
    ("abort", procenv::abort),
    ("__stack_chk_fail", procenv::stack_chk_fail),
    ("_exit", procenv::exit),
    ("android_set_abort_message", procenv::android_set_abort_message),
    ("sysconf", procenv::sysconf),
    ("sysinfo", procenv::sysinfo),
    ("prctl", procenv::prctl),
    ("syscall", procenv::syscall),
    // ---- phase 3a: the log sink. These four are serviced rather than refused because a log call
    // has no return value the guest acts on, so there is no believable wrong answer available —
    // and because refusing would halt the run at the first thing the engine wanted to report.
    ("__android_log_print", logging::android_log_print),
    ("syslog", logging::syslog),
    ("openlog", logging::openlog),
    ("closelog", logging::closelog),
    // ---- phase 3b: files and directories. Eighteen descriptor-level symbols over
    // `omni-platform`'s rooted filesystem seam. They are inline rather than re-entrant because
    // none of them calls guest code and none reaches `GuestSpace`: they read and write guest
    // memory, which `memcpy` already does from the fast path, and the arena a `FILE` or a
    // `dirent` lands in was mapped in `Bionic::new` (F9).
    ("open", files::open),
    ("__open_2", files::open_2),
    ("close", files::close),
    ("read", files::read),
    ("pread", files::pread),
    ("__write_chk", files::write_chk),
    ("access", files::access),
    ("getcwd", files::getcwd),
    ("stat", files::stat),
    ("fstat", files::fstat),
    ("lstat", files::lstat),
    ("statvfs", files::statvfs),
    ("rename", files::rename),
    ("unlink", files::unlink),
    ("mkdir", files::mkdir),
    ("rmdir", files::rmdir),
    ("opendir", files::opendir),
    ("readdir", files::readdir),
    ("closedir", files::closedir),
    // ---- M5: the two §5.2 needs before `initializeNativeCode` can return. Neither is among the
    // 188 the initializers reach, and binding `pipe` is what ended `net`'s closed-descriptor-space
    // argument -- see that module for what replaced it.
    ("pipe", files::pipe),
    ("fcntl", files::fcntl),
    ("write", files::write),
    // ---- phase 3b: bionic's `FILE *` layer, over those descriptors. The stream logic is in
    // `omni-bionic` (D19) and what is here is the binding from a guest `FILE *` to a stream.
    ("fopen", stdio::fopen),
    ("fdopen", stdio::fdopen),
    ("fclose", stdio::fclose),
    ("feof", stdio::feof),
    ("fflush", stdio::fflush),
    ("fgets", stdio::fgets),
    ("fileno", stdio::fileno),
    ("fputc", stdio::fputc),
    ("fputs", stdio::fputs),
    ("fread", stdio::fread),
    ("fwrite", stdio::fwrite),
    // ---- phase 3c: signals. One is answered because it is pure computation over the guest's
    // own `sigset_t`; three are bound and **refuse by name**, because there is no guest signal
    // delivery here and each of them has a believable wrong answer that would not be observable
    // until much later. `signals`' module documentation has the table.
    ("sigfillset", signals::sigfillset),
    ("sigaction", signals::sigaction),
    ("raise", signals::raise),
    ("pthread_sigmask", signals::pthread_sigmask),
    // ---- phase 3c: the one thread symbol that runs no guest code and touches no mapping, so
    // the exit path would cost it 3x per call for nothing.
    ("pthread_getschedparam", threads::pthread_getschedparam),
    // ---- phase 3d: the network group. Two are answered out of `omni-bionic` (pure computation),
    // two are answered here over the descriptor table `files` already has -- with **no new
    // `omni-platform` surface at all**, which is the third phase running whose five-target
    // prediction over-estimated the OS -- and four refuse by name. `net`'s module documentation
    // has the closed argument for why `poll` and `select` need no operating system, and the
    // believable wrong answer each refusal declines to give.
    ("inet_ntop", net::inet_ntop),
    ("gai_strerror", net::gai_strerror),
    ("poll", net::poll),
    ("select", net::select),
    ("socket", net::socket),
    ("eventfd", net::eventfd),
    ("getaddrinfo", net::getaddrinfo),
    ("freeaddrinfo", net::freeaddrinfo),
    // ---- phase 3e: the six nothing else claimed. Two are answered -- `time` over the same wall
    // clock `gettimeofday` reads, and `clock` over the one new `omni-platform` primitive this
    // combined phase needed -- and two refuse by name. The other two of the six are
    // `__gcov_dump` and `__gcov_flush`, which are **not here at all**: they are declared
    // ABSENT, so a weak reference to either resolves to null and the guest's own null test
    // skips the call. `bionic::absent` has the decoded guest instructions.
    ("time", clocks::time),
    ("clock", clocks::clock),
    ("mallinfo", guestmem::mallinfo),
    ("longjmp", signals::longjmp),
];

/// Serviced on the **exit** path: 80-102 ns per call.
///
/// Two reasons a symbol is here, and only one of them is about guest code.
///
/// * **It calls a guest callback.** `pthread_once`'s initialiser, `qsort`'s comparator and
///   `dl_iterate_phdr`'s per-object callback are all guest functions, and D18 makes "may run guest
///   code" a property of the type rather than a rule.
/// * **It changes the guest's address space** — task 2 review finding **F9**. `mmap`, `munmap`,
///   `mprotect`, `madvise` and `mlock` reach `GuestSpace`, and an inline handler runs inside one
///   of the translating backend's own callbacks with generated code live. Nothing in the types
///   says so; see `guestmem`'s module documentation and
///   `dispatch_paths_are_what_f9_requires` in `tests/bionic.rs`.
pub(super) static REENTRANT: &[(&str, ReentrantFn)] = &[
    ("pthread_once", pthread_once),
    ("qsort", qsort),
    ("dl_iterate_phdr", dl::dl_iterate_phdr),
    // `dlopen`, `dlsym` and `dlclose` are on the exit path because each needs the boundary's
    // symbol table and `ImportCall` deliberately cannot reach it. None of them calls guest code.
    ("dlopen", dl::dlopen),
    ("dlsym", dl::dlsym),
    ("dlclose", dl::dlclose),
    ("mmap", guestmem::mmap),
    ("munmap", guestmem::munmap),
    ("mprotect", guestmem::mprotect),
    ("madvise", guestmem::madvise),
    ("mlock", guestmem::mlock),
    // ---- phase 3c: thread lifecycle. Re-entrant for both of F9's reasons at once: the start
    // routine is **guest code**, and `pthread_create` maps the new thread's stack, which reaches
    // `GuestSpace`. It is also the only handler that needs the boundary itself, to install the
    // thunk table on a context it has just created; `threads`' module documentation has the
    // three constraints that meet in it and why none of them is traded against another.
    ("pthread_create", threads::pthread_create),
    ("pthread_join", threads::pthread_join),
    ("pthread_detach", threads::pthread_detach),
];
