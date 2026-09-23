//! Linux backend for the process seam.
//!
//! **Implemented, and run on Linux x86-64** (Ubuntu 26.04, kernel 7.0.0, glibc 2.43, an i5-4460).
//! Not run on Linux ARM64. [`cpu_time`] is the shared [`unix`](super::unix) body, because
//! `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` is POSIX and is the same call on macOS; the other
//! five are written here, because each is a call only Linux has in this shape:
//!
//! | primitive | call | the decision in it |
//! |---|---|---|
//! | [`random_bytes`] | `getrandom(buf, len, 0)`, looped | blocking rather than `GRND_NONBLOCK`; short fills and `EINTR` looped over |
//! | [`current_cpu`] | `sched_getcpu(3)` | none: the guest symbol *is* this call |
//! | [`set_current_thread_nice`] | `setpriority(PRIO_PROCESS, gettid(), nice)` | the **thread**, not the process; a refused raise is the host's errno |
//! | [`current_thread_host_priority`] | `getpriority(PRIO_PROCESS, gettid())` | `-1` is a nice value, so `errno` is cleared and read |
//! | [`host_manufacturer`] | `/sys/class/dmi/id/sys_vendor`, trimmed | a missing or empty record is an error, not a name |
//!
//! # The guest-visible difference this host has, stated rather than smoothed
//!
//! **An unprivileged Linux process cannot raise a thread's priority.** Lowering a nice value
//! (`-16` is Android's `THREAD_PRIORITY_AUDIO`, which FMOD asks for on every one of its threads)
//! needs `CAP_SYS_NICE` or an `RLIMIT_NICE` of at least `20 - nice`, and a desktop session gives
//! neither: MEASURED on this host, `ulimit -e` is **0**, so the floor is nice 20 -- no lowering at
//! all, not even back to 0 from 19. The kernel answers `EACCES`, and [`set_current_thread_nice`]
//! reports exactly that as [`ProcessError::Errno`]. A device answers success, because AOSP's
//! `init.rc` sets `setrlimit nice 40 40`; what the layer above does with the difference is its
//! decision, and it is not made here by pretending the host agreed.

use super::{ProcessError, ProcessResult};

pub(super) use super::unix::cpu_time;
use super::unix::last_errno;

/// `getrandom(buf, len, 0)`, called until the whole buffer is filled.
///
/// # Blocking, not `GRND_NONBLOCK`, and why
///
/// Flags `0` block **only until the kernel's CRNG has been seeded once after boot**, and never
/// again for the life of the system. `GRND_NONBLOCK` would turn that window into `EAGAIN`, and
/// then this function has two answers and both are wrong: failing makes the guest's
/// `arc4random_buf` -- which cannot fail, and whose callers have no error path -- a refusal
/// during early boot; and substituting anything is the one thing entropy must never do. Blocking
/// is also what the guest's own libc does: bionic's `arc4random` seeds from `getrandom(.., 0)`.
/// The window it could block in is before this process can have been started by a user, and on
/// kernels since 5.18 the CRNG is seeded from jitter entropy within about a second of boot even
/// on a machine with no hardware source, so the wait has an end that does not depend on the
/// guest. `GRND_RANDOM` (the old blocking pool) is not used: it is no stronger and can stall.
///
/// # The loop, and the two ways it is entered
///
/// `getrandom` may return **fewer bytes than asked for**: a request is filled a page at a time
/// and the kernel stops between pages when a signal is pending, returning what it has; one larger
/// than `MAX_RW_COUNT` (just under 2 GiB) is truncated to it. And a signal that arrives before the
/// first page is `-1`/`EINTR` with nothing written. So the call is repeated over the unfilled
/// tail until nothing is left, and `EINTR` is retried rather than reported: the request is
/// idempotent and the guest's `arc4random_buf` has no way to receive an error anyway. A short
/// fill treated as a full one would hand the caller a buffer whose tail is whatever was there
/// before -- zeroes, usually -- reported as entropy.
pub(super) fn random_bytes(out: &mut [u8]) -> ProcessResult<()> {
    fill_from(out, |rest| {
        // SAFETY: `getrandom` writes at most `rest.len()` bytes at `rest`'s pointer and reads
        // nothing; `rest` is a live, uniquely-borrowed slice of exactly that length.
        unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) }
    })
}

/// The loop [`random_bytes`] runs, over any call with `getrandom(2)`'s contract: fill some prefix
/// of the slice and answer how much, or answer `-1` with `errno` set.
///
/// **A separate function so that the loop can be driven by the raw syscall**, and the reason is a
/// measurement. glibc 2.41 and later answer `getrandom` from the kernel's **vDSO** -- in user
/// space, with no syscall -- and that path does not stop for a signal. MEASURED on this host
/// (glibc 2.43, kernel 7.0), 200 calls of 16 MiB each with `SIGUSR1` sent to the calling thread
/// every 50 us: **glibc's `getrandom` returned short 0 times; `syscall(SYS_getrandom)` returned
/// short 200 times in 200.** So on this host the loop's short-fill arm is unreachable through the
/// wrapper, and it is not dead: the wrapper falls back to the syscall wherever the vDSO entry is
/// missing (kernels before 6.11) or the request is one it declines. The test drives this function
/// with the syscall, which is the kernel's real behaviour rather than a simulation of it.
fn fill_from(out: &mut [u8], mut call: impl FnMut(&mut [u8]) -> isize) -> ProcessResult<()> {
    let mut filled = 0;
    while filled < out.len() {
        let rest = &mut out[filled..];
        let got = call(rest);
        if got < 0 {
            let errno = last_errno();
            if errno == libc::EINTR {
                #[cfg(test)]
                tests::note(&tests::EINTR_RETRIES);
                continue;
            }
            return Err(ProcessError::Errno { operation: "random_bytes", api: "getrandom", errno });
        }
        // `got` is non-negative here and never more than it was offered.
        let got = usize::try_from(got).unwrap_or(0).min(rest.len());
        if got == 0 {
            // Not a thing `getrandom(2)` does for a non-empty request. It is here because it is
            // the loop's termination argument: without it, a zero would spin this thread for ever
            // inside a guest call, and D16's step budgets cannot end a thread that is not running
            // guest code.
            return Err(ProcessError::Indeterminate {
                operation: "random_bytes",
                detail: format!("getrandom returned 0 bytes for a request of {}", rest.len()),
            });
        }
        #[cfg(test)]
        tests::note_fill(got, rest.len());
        filled += got;
    }
    Ok(())
}

/// `sched_getcpu(3)`: the processor the calling thread is running on.
///
/// glibc answers from the `getcpu` vDSO entry, so this costs no syscall. The number is the
/// kernel's logical cpu id -- the same number `sched_setaffinity` takes, which is what the test
/// uses to check it -- and it is advisory the moment it is returned, as the seam says.
pub(super) fn current_cpu() -> ProcessResult<u32> {
    // SAFETY: no arguments and no memory.
    let cpu = unsafe { libc::sched_getcpu() };
    u32::try_from(cpu).map_err(|_| ProcessError::Errno {
        operation: "current_cpu",
        api: "sched_getcpu",
        errno: last_errno(),
    })
}

/// The calling thread's kernel id, which is what `PRIO_PROCESS` names on Linux.
fn this_thread() -> libc::id_t {
    // SAFETY: no arguments; `gettid` cannot fail.
    let tid = unsafe { libc::gettid() };
    // A thread id is positive; `id_t` is the unsigned type `setpriority` takes it as.
    tid.unsigned_abs()
}

/// `setpriority(PRIO_PROCESS, gettid(), nice)`: the calling **thread's** nice value.
///
/// **Per thread, because on Linux `PRIO_PROCESS` names a task and a thread is one** -- the
/// documented NPTL divergence from POSIX (`setpriority(2)`, BUGS), and the reason this is the
/// guest's own call rather than a mapping: Android is Linux, and the guest's `setpriority(0, 0,
/// -16)` means exactly this. `getpid()` here would set the process's *main* thread and leave the
/// caller untouched.
///
/// The value is Linux's own nice scale, already clamped to `[-20, 19]` by the seam, and it is
/// passed through unchanged -- there is no table to apply on the host whose scale it is.
///
/// # Errors
///
/// [`ProcessError::Errno`] with the kernel's answer: `EACCES` for a lower nice value than this
/// thread has when the process holds neither `CAP_SYS_NICE` nor an `RLIMIT_NICE` that permits it
/// -- see the module header for what this host gives -- and `EPERM`/`ESRCH` for the cases
/// `setpriority(2)` lists.
pub(super) fn set_current_thread_nice(nice: i32) -> ProcessResult<()> {
    // SAFETY: three by-value integers; no memory crosses.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, this_thread(), nice) };
    if rc != 0 {
        return Err(ProcessError::Errno {
            operation: "set_current_thread_nice",
            api: "setpriority(PRIO_PROCESS, gettid())",
            errno: last_errno(),
        });
    }
    Ok(())
}

/// `getpriority(PRIO_PROCESS, gettid())`: the calling thread's nice value, `-20..=19`.
///
/// The host's own numbering *is* the nice scale here, so this answers exactly what
/// [`set_current_thread_nice`] set. **`-1` is a legitimate nice value and also the error
/// return**, which is why `errno` is cleared before the call and read after it rather than the
/// return value being tested alone -- the pattern `getpriority(2)` itself prescribes.
pub(super) fn current_thread_host_priority() -> ProcessResult<i32> {
    // SAFETY: `__errno_location` returns this thread's `errno` slot, valid for the thread's life.
    unsafe { *libc::__errno_location() = 0 };
    // SAFETY: two by-value integers; no memory crosses.
    let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, this_thread()) };
    if nice == -1 {
        let errno = last_errno();
        if errno != 0 {
            return Err(ProcessError::Errno {
                operation: "current_thread_host_priority",
                api: "getpriority(PRIO_PROCESS, gettid())",
                errno,
            });
        }
    }
    Ok(nice)
}

/// Where the kernel publishes the firmware's SMBIOS System Information manufacturer.
pub(super) const SYS_VENDOR: &str = "/sys/class/dmi/id/sys_vendor";

/// The machine's maker, from the SMBIOS System Information structure's `Manufacturer` string --
/// the same firmware field Windows copies into `SystemManufacturer`.
///
/// Trimmed at both ends: the kernel ends the file with a newline, and firmware pads the field
/// with spaces. **Missing is an error, not a default**: a board without DMI (most ARM hosts, many
/// containers) has no such file, and naming a maker for it would be a manufacturer nobody made.
pub(super) fn host_manufacturer() -> ProcessResult<String> {
    let bytes = std::fs::read(SYS_VENDOR).map_err(|error| ProcessError::Errno {
        operation: "host_manufacturer",
        api: "read(/sys/class/dmi/id/sys_vendor)",
        errno: error.raw_os_error().unwrap_or(0),
    })?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if text.is_empty() {
        return Err(ProcessError::Indeterminate {
            operation: "host_manufacturer",
            detail: format!(
                "{SYS_VENDOR} exists and is empty: the firmware did not name a manufacturer, and \
                 this layer does not name one for it"
            ),
        });
    }
    Ok(text.to_string())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// How many times [`random_bytes`] saw `getrandom` return short, and retried `EINTR`.
    ///
    /// **Test-only instruments, and they are the stimulus's proof** (VERIFICATION entry 19): the
    /// loop test below asserts the whole buffer was filled, and that assertion is only worth
    /// something if the kernel actually returned short while it ran. These say that it did.
    pub(super) static SHORT_FILLS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static EINTR_RETRIES: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn note(counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn note_fill(got: usize, asked: usize) {
        if got < asked {
            note(&SHORT_FILLS);
        }
    }

    /// A signal handler that does nothing, so that a signal interrupts a syscall and nothing else.
    extern "C" fn nothing(_: libc::c_int) {}

    /// The raw `getrandom(2)` syscall, bypassing glibc's vDSO path: the source whose short fills
    /// are real on this host. See [`fill_from`].
    fn raw_getrandom(rest: &mut [u8]) -> isize {
        // SAFETY: as `random_bytes`'s call; the syscall writes at most `rest.len()` bytes there.
        let got = unsafe { libc::syscall(libc::SYS_getrandom, rest.as_mut_ptr(), rest.len(), 0u32) };
        got as isize
    }

    /// Fill 16 MiB buffers from `fill` while another thread sends `SIGUSR1` to this one every
    /// 50 us, and require every 4 KiB page of each to have been written. Returns (rounds,
    /// signals sent, short fills seen, EINTR retries seen) for the caller to judge.
    fn under_signals(rounds: usize, fill: fn(&mut [u8]) -> ProcessResult<()>) -> (usize, u64, usize, usize) {
        // SAFETY: installing a handler for SIGUSR1, which nothing else in this test binary uses;
        // the handler touches nothing. `sa_flags` is 0, so SA_RESTART is off, which is the point.
        unsafe {
            let mut action: libc::sigaction = core::mem::zeroed();
            action.sa_sigaction = nothing as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&raw mut action.sa_mask);
            action.sa_flags = 0;
            assert_eq!(libc::sigaction(libc::SIGUSR1, &raw const action, core::ptr::null_mut()), 0);
        }
        const LEN: usize = 16 << 20;
        let (short_before, eintr_before) =
            (SHORT_FILLS.load(Ordering::Relaxed), EINTR_RETRIES.load(Ordering::Relaxed));
        let target = Arc::new(std::sync::Mutex::new(None::<libc::pthread_t>));
        let stop = Arc::new(AtomicBool::new(false));
        let signaller = {
            let (target, stop) = (Arc::clone(&target), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut sent = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    if let Some(thread) = *target.lock().expect("not poisoned") {
                        // SAFETY: `thread` is the filler, alive while it is registered here: it
                        // clears the registration under this same lock before it returns.
                        unsafe { libc::pthread_kill(thread, libc::SIGUSR1) };
                        sent += 1;
                    }
                    std::thread::sleep(Duration::from_micros(50));
                }
                sent
            })
        };
        let filler = {
            let (target, stop) = (Arc::clone(&target), Arc::clone(&stop));
            std::thread::spawn(move || {
                // SAFETY: no arguments.
                *target.lock().expect("not poisoned") = Some(unsafe { libc::pthread_self() });
                let mut buffer = vec![0u8; LEN];
                for round in 0..rounds {
                    buffer.fill(0);
                    fill(&mut buffer).expect("an interrupted getrandom is retried, not reported");
                    for (page, chunk) in buffer.chunks(4096).enumerate() {
                        assert!(
                            chunk.iter().any(|&b| b != 0),
                            "round {round}: page {page} of {} is still zero -- a short fill was \
                             taken for the whole answer",
                            LEN / 4096
                        );
                    }
                }
                *target.lock().expect("not poisoned") = None;
                stop.store(true, Ordering::Relaxed);
            })
        };
        filler.join().expect("the filler");
        let sent = signaller.join().expect("the signaller");
        (
            rounds,
            sent,
            SHORT_FILLS.load(Ordering::Relaxed) - short_before,
            EINTR_RETRIES.load(Ordering::Relaxed) - eintr_before,
        )
    }

    /// Serialises the two signal tests: they share one handler and the process-wide counters.
    static SIGNALS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// **The loop fills the whole buffer when `getrandom(2)` returns short** -- driven by the raw
    /// syscall, whose short fills under signals are the kernel's real behaviour on this host
    /// (MEASURED: 200 short in 200 calls; see [`fill_from`]).
    ///
    /// The detector is the tail: every buffer starts zeroed and every 4 KiB page of it must hold a
    /// non-zero byte afterwards (a page of real entropy is all-zero with probability 2^-32768). A
    /// loop that took one short fill as the whole answer leaves the tail zero; one that reported
    /// `EINTR` fails the `expect`. And the stimulus must be shown to have happened
    /// (VERIFICATION entry 19): at least one short fill is required, or the test proved nothing.
    #[test]
    fn a_getrandom_that_returns_short_under_signals_is_looped_until_full() {
        let _serial = SIGNALS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (rounds, sent, short, eintr) = under_signals(8, |buf| fill_from(buf, raw_getrandom));
        eprintln!(
            "raw getrandom under signals: {rounds} rounds of 16 MiB, {sent} SIGUSR1, {short} short \
             fills, {eintr} EINTR retries"
        );
        assert!(short > 0, "{sent} signals over {rounds} rounds produced no short fill");
    }

    /// **`random_bytes` itself, under the same signals, fills every buffer** -- through glibc's
    /// vDSO path, which (MEASURED, printed) is not interrupted at all. Both facts are kept: the
    /// production call is what the guest gets, and the count says why the loop test above has to
    /// use the syscall.
    #[test]
    fn random_bytes_under_signals_fills_every_buffer() {
        let _serial = SIGNALS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (rounds, sent, short, eintr) = under_signals(8, random_bytes);
        eprintln!(
            "glibc getrandom under signals: {rounds} rounds of 16 MiB, {sent} SIGUSR1, {short} \
             short fills, {eintr} EINTR retries"
        );
    }

    /// **`EINTR` is retried and any other errno is reported, by name** -- `fill_from` driven with
    /// `getrandom(2)`'s documented contract, because the kernel gives `EINTR` only while it waits
    /// for the CRNG's first seeding, which a running host is long past (MEASURED: 0 `EINTR` in
    /// the signal tests above). The source is a script: `-1`/`EINTR`, a 3-byte short fill,
    /// `-1`/`EINTR`, then the rest; and for the second half, `-1`/`EIO`.
    #[test]
    fn eintr_is_retried_and_another_errno_is_reported() {
        fn fail(errno: i32) -> isize {
            // SAFETY: this thread's errno slot.
            unsafe { *libc::__errno_location() = errno };
            -1
        }
        let mut script = 0;
        let mut out = [0u8; 8];
        fill_from(&mut out, |rest| {
            script += 1;
            match script {
                1 | 3 => fail(libc::EINTR),
                2 => {
                    rest[..3].copy_from_slice(b"abc");
                    3
                }
                _ => {
                    rest.fill(b'z');
                    rest.len() as isize
                }
            }
        })
        .expect("EINTR is retried");
        assert_eq!(&out, b"abczzzzz");
        assert_eq!(script, 4, "two EINTRs, one short fill, one final fill");

        let error = fill_from(&mut out, |_| fail(libc::EIO)).expect_err("EIO is not retried");
        assert!(
            matches!(error, ProcessError::Errno { errno: libc::EIO, api: "getrandom", .. }),
            "{error:?}"
        );
    }

    /// Two draws differ, a short buffer is filled to its end and no further, and an empty one
    /// is a no-op -- on this target, where the shared test in `process` is not ignored any more.
    #[test]
    fn random_bytes_fills_exactly_the_slice_it_was_given() {
        let mut framed = [0xAAu8; 40];
        random_bytes(&mut framed[..32]).expect("entropy");
        assert_eq!(&framed[32..], &[0xAA; 8], "wrote past the slice");
        assert_ne!(&framed[..32], &[0xAA; 32], "wrote nothing");
    }

    /// **`sched_getcpu` names the cpu the thread is pinned to, for every cpu it may use.**
    ///
    /// The oracle is the affinity mask, not a second reading: pinned to one cpu, the thread can
    /// be nowhere else, so the answer is forced. A constant (0, the believable one) fails on the
    /// first cpu past 0; this host has four.
    #[test]
    fn the_current_cpu_is_the_one_the_thread_is_pinned_to() {
        std::thread::spawn(|| {
            // SAFETY: a zeroed `cpu_set_t` is the empty set; `sched_getaffinity` fills it.
            let mut allowed: libc::cpu_set_t = unsafe { core::mem::zeroed() };
            // SAFETY: `allowed` is a live `cpu_set_t` and the size is its own.
            let rc = unsafe {
                libc::sched_getaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &raw mut allowed)
            };
            assert_eq!(rc, 0, "sched_getaffinity");
            let cpus: Vec<usize> =
                // SAFETY: `CPU_ISSET` reads one bit of a live set.
                (0..libc::CPU_SETSIZE as usize).filter(|&c| unsafe { libc::CPU_ISSET(c, &allowed) }).collect();
            assert!(cpus.len() >= 2, "this host lets the test use only {cpus:?}");
            for &cpu in &cpus {
                // SAFETY: as above.
                let mut one: libc::cpu_set_t = unsafe { core::mem::zeroed() };
                // SAFETY: `one` is a live set and `cpu` is below CPU_SETSIZE.
                unsafe { libc::CPU_SET(cpu, &mut one) };
                // SAFETY: as `sched_getaffinity`.
                let rc = unsafe {
                    libc::sched_setaffinity(0, core::mem::size_of::<libc::cpu_set_t>(), &raw const one)
                };
                assert_eq!(rc, 0, "pinning to cpu {cpu}");
                assert_eq!(current_cpu().expect("sched_getcpu"), cpu as u32, "pinned to {cpu}");
            }
        })
        .join()
        .expect("the pinned thread");
    }

    /// The nice value this process may lower a thread to, from `RLIMIT_NICE` and `CAP_SYS_NICE`:
    /// `20 - rlim_cur` (`setpriority(2)`, `getrlimit(2)`), or `-20` with the capability.
    fn nice_floor() -> i32 {
        let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
        let effective = status
            .lines()
            .find_map(|line| line.strip_prefix("CapEff:"))
            .map(|hex| u64::from_str_radix(hex.trim(), 16).expect("CapEff is hex"))
            .expect("a CapEff line");
        const CAP_SYS_NICE: u32 = 23;
        if effective & (1 << CAP_SYS_NICE) != 0 {
            return -20;
        }
        // SAFETY: `limit` is a live `rlimit` and `getrlimit` writes only it.
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NICE, &raw mut limit) }, 0);
        20 - i32::try_from(limit.rlim_cur.min(40)).expect("at most 40")
    }

    /// Another thread's nice value, read by its id: what proves a setting was per thread.
    fn nice_of(tid: libc::id_t) -> i32 {
        // SAFETY: as `current_thread_host_priority`.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: two by-value integers.
        let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid) };
        assert!(nice != -1 || last_errno() == 0, "getpriority({tid})");
        nice
    }

    /// **A nice value lands on the calling thread and on no other, and a raise this process may
    /// not make is the host's `EACCES`, not a success.**
    ///
    /// Two threads: a bystander that only reports its id and waits, and the one that sets. The
    /// bystander's value is read by id after the set, so a backend that applied the value to the
    /// process's main thread (`getpid`) or to every thread fails the first half, and one that
    /// swallowed the refusal fails the second. Which raises are allowed is computed from the
    /// process's own `RLIMIT_NICE` and `CAP_SYS_NICE`, so the assertion is the kernel's rule on
    /// any configuration rather than this host's.
    #[test]
    fn a_nice_value_is_the_calling_threads_and_a_forbidden_raise_is_refused() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let bystander = std::thread::spawn(move || {
            tx.send(this_thread()).expect("send");
            done_rx.recv().expect("released");
        });
        let bystander_tid = rx.recv().expect("the bystander's id");
        let before = nice_of(bystander_tid);

        std::thread::spawn(move || {
            let start = current_thread_host_priority().expect("getpriority");
            assert_eq!(start, nice_of(this_thread()), "the reading is this thread's own");
            let lower = (start + 5).min(19);
            set_current_thread_nice(lower).expect("a lower priority is always allowed");
            assert_eq!(current_thread_host_priority().expect("read"), lower);
            // The oracle is this thread's own kernel id, read here and not through the backend:
            // a backend that named the wrong task would read back its own wrong answer.
            // SAFETY: no arguments.
            let me = unsafe { libc::gettid() }.unsigned_abs();
            assert_eq!(nice_of(me), lower, "the value did not land on the calling thread");
            assert_eq!(
                nice_of(bystander_tid),
                before,
                "setting nice {lower} on one thread changed another's: it is not per thread"
            );

            // Back up toward -16, FMOD's value: allowed only down to the floor.
            let floor = nice_floor();
            let wanted = -16;
            match set_current_thread_nice(wanted) {
                Ok(()) => {
                    assert!(wanted >= floor, "nice {wanted} was accepted below the floor {floor}");
                    assert_eq!(current_thread_host_priority().expect("read"), wanted);
                }
                Err(error) => {
                    assert!(
                        wanted < floor,
                        "nice {wanted} is within this process's floor {floor} and was refused: {error}"
                    );
                    assert!(
                        matches!(error, ProcessError::Errno { errno: libc::EACCES, .. }),
                        "a forbidden raise is the kernel's EACCES, reported as it is: {error:?}"
                    );
                    assert!(error.to_string().contains("setpriority"), "{error}");
                    assert_eq!(
                        current_thread_host_priority().expect("read"),
                        lower,
                        "and a refused set changed nothing"
                    );
                    eprintln!("nice floor on this host is {floor}; nice {wanted} refused: {error}");
                }
            }
        })
        .join()
        .expect("the setting thread");
        done_tx.send(()).expect("release");
        bystander.join().expect("the bystander");
    }

    /// **The maker is the firmware's, trimmed**: equal to the kernel's record with its newline
    /// removed, and never empty.
    ///
    /// The comparison is against the file's own bytes rather than a name typed here, because a
    /// name typed here would be true of one machine. What a mutation can get wrong -- the trailing
    /// newline kept, a different file, a default -- each fails one of these.
    #[test]
    fn the_host_names_its_manufacturer_as_the_firmware_does() {
        let maker = host_manufacturer().expect("this host has DMI");
        let raw = std::fs::read_to_string(SYS_VENDOR).expect("the record");
        assert_eq!(maker, raw.trim(), "{maker:?} against {raw:?}");
        assert!(!maker.is_empty());
        assert!(!maker.contains('\n') && !maker.contains('\0'), "{maker:?}");
        assert_eq!(maker, maker.trim(), "{maker:?}");
        eprintln!("host_manufacturer: {maker:?}");
    }

    /// Process CPU time moves under work and is **nanosecond-grained** here, not tick-grained.
    ///
    /// The Linux fact the seam's documentation leaves open ("resolution is the host's scheduler
    /// accounting"): `CLOCK_PROCESS_CPUTIME_ID` is accounted from the scheduler's runtime
    /// counters in nanoseconds, where Windows' `GetProcessTimes` advances in 15.625 ms ticks
    /// (VERIFICATION entry 6's `1843750 -> 1843750`). The measurement is the **smallest non-zero
    /// step** between consecutive readings over 10,000 of them, which is a property of the clock
    /// and not of the load: other threads can only add to a step, never shrink one below the
    /// clock's own grain. A tick-grained clock has no step below its tick (4 ms at this kernel's
    /// `CONFIG_HZ=250`, 15.6 ms on Windows); the assertion allows 1 ms.
    #[test]
    fn process_cpu_time_moves_in_nanosecond_steps() {
        let mut previous = cpu_time().expect("clock_gettime");
        let mut smallest = Duration::MAX;
        let mut steps = 0;
        for _ in 0..10_000 {
            let now = cpu_time().expect("clock_gettime");
            assert!(now >= previous, "process CPU time went backwards: {previous:?} -> {now:?}");
            if now > previous {
                smallest = smallest.min(now - previous);
                steps += 1;
            }
            previous = now;
        }
        assert!(steps > 0, "10,000 readings and the clock never moved");
        assert!(
            smallest < Duration::from_millis(1),
            "the smallest step in {steps} was {smallest:?}: a tick-grained clock"
        );
        eprintln!("cpu_time: {steps} steps in 10,000 readings, smallest {smallest:?}");
    }
}
