//! Process and machine facts: the platform seam for "what process am I, and what is under me".
//!
//! Serves the guest's `getpid`, `sysconf`, `sched_getcpu` and `arc4random_buf`. What it
//! deliberately does **not** serve is `getenv` and `__system_property_get`: see "The environment
//! is not on this seam" below.
//!
//! # Two kinds of primitive, and only one of them needs a backend
//!
//! This is the first module in the crate where that distinction has to be made explicitly, and the
//! pattern later phases copy is set here:
//!
//! | primitive | how |
//! |---|---|
//! | [`pid`] | `std::process::id()` — portable standard library, one implementation |
//! | [`cpu_count`] | `std::thread::available_parallelism()` — portable standard library |
//! | [`random_bytes`] | **backend**: `BCryptGenRandom` on Windows; `getrandom(2)` / `arc4random_buf(3)` intended elsewhere |
//! | [`current_cpu`] | **backend**: `GetCurrentProcessorNumber` on Windows; `sched_getcpu(3)` intended on Linux |
//! | [`cpu_time`] | **backend**: `GetProcessTimes` on Windows; `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` intended elsewhere |
//!
//! The two backend entries carry the five-target rule in full: `linux.rs` and `macos.rs` exist
//! **now**, they name the POSIX call they intend to make, and they return
//! [`ProcessError::Unsupported`] rather than a plausible body. The two portable entries do not get
//! a fabricated `Unsupported` arm, because inventing one would be a false claim in the other
//! direction — see [`crate::clock`], which makes the same argument at length.
//!
//! The list a backend must provide is exactly:
//!
//! ```text
//! random_bytes(&mut [u8]) -> ProcessResult<()>
//! current_cpu() -> ProcessResult<u32>
//! cpu_time() -> ProcessResult<Duration>
//! ```
//!
//! A backend that is missing one, or whose signature has drifted, does not build for that target —
//! the same compile-time substitutability [`crate::vm`] relies on, and for the same reason.
//!
//! # The environment is not on this seam, and that is a decision
//!
//! `getenv` and `__system_property_get` are process-environment symbols, and there is no
//! `host_environment()` here. Handing the guest the **host's** environment would be both a wrong
//! answer — an Android app's environment is not a desktop shell's — and an information leak of
//! every variable this process was started with, including ones holding credentials. The guest's
//! environment is state the *host embedding* decides and the adapter stores, exactly as `environ`
//! already is (it is one of the eighteen data objects, and it points at an empty vector). Reading
//! the real host environment, if anything ever needs to, is `std::env` and needs no seam at all.
//!
//! # Process CPU time is the seam's third backend primitive, and it is genuinely not `std`
//!
//! [`cpu_time`] serves the guest's `clock()` and `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)`. The
//! sharper five-target test D23 proposed — *is there one `std` call that serves all five
//! targets?* — answers **no** here, and that is the first time in three phases it has. `Instant`
//! is wall time; nothing in the standard library reports how much processor time this process has
//! consumed. So this one gets a Windows backend and a structural unix half, and the two other
//! entries in this module that already had one gain a third sibling rather than an exception.
//!
//! # `sysinfo` is not here either
//!
//! There is no `physical_memory()`, because the guest symbol that would use it — `sysinfo` — is
//! **refused by name** in the adapter rather than answered. Adding a primitive whose only caller
//! refuses would be surface built for a call that is not made.

mod error;

use std::time::Duration;

pub use error::{ProcessError, ProcessResult};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

/// This process's identifier, as the guest's `getpid` reports it.
///
/// The **host** process id, and truthfully so: several guest instances share one host process, so
/// they share one pid, which is exactly what `getpid` would report for several threads of one
/// Android process. Inventing a per-instance pid would be a number with nothing behind it — no
/// `/proc` entry, no signal target, no relationship to anything the OS knows.
#[must_use]
pub fn pid() -> u32 {
    std::process::id()
}

/// How many CPUs this process may run on, as `sysconf(_SC_NPROCESSORS_ONLN)` reports it.
///
/// `available_parallelism` rather than a raw core count: it honours affinity masks and container
/// limits, which is what "processors online *for this process*" means. It never returns zero, so
/// neither does this — a zero would divide by zero in any guest thread-pool sizing that used it.
#[must_use]
pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// Fill `out` with cryptographically strong random bytes.
///
/// Serves the guest's `arc4random_buf`, whose contract is that the bytes are unpredictable — so a
/// pseudo-random fallback is not an acceptable degradation and a failure is reported rather than
/// substituted. An empty slice is a no-op and succeeds.
///
/// # Errors
///
/// [`ProcessError::Unsupported`] on Linux and macOS, naming the intended POSIX call.
/// [`ProcessError::Status`] if the OS entropy source itself fails.
pub fn random_bytes(out: &mut [u8]) -> ProcessResult<()> {
    if out.is_empty() {
        return Ok(());
    }
    backend::random_bytes(out)
}

/// The processor number the calling thread is running on, as `sched_getcpu` reports it.
///
/// **Advisory the instant it is returned**: the thread may migrate before the caller reads it.
/// That is true of `sched_getcpu` on Linux too, and it is why the value's only legitimate use is
/// as a shard index.
///
/// # Errors
///
/// [`ProcessError::Unsupported`] on Linux and macOS, naming the intended POSIX call.
pub fn current_cpu() -> ProcessResult<u32> {
    backend::current_cpu()
}

/// Processor time this **process** has consumed, across every thread it has ever had.
///
/// What `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` reports on Linux, and therefore what the
/// guest's `clock()` is scaled from. User **and** kernel time together, which is what both of
/// those report and what `clock(3)` is specified to return ("processor time used"): counting only
/// user time would make a guest that spends its startup inside the kernel look idle.
///
/// **It is the process's, not the thread's, and not this guest instance's.** Several guest
/// instances share one host process, so they share this number, exactly as they share
/// [`pid`]. That is the same honest answer `getpid` gives and for the same reason — there is no
/// per-instance process for a per-instance figure to describe.
///
/// **Resolution is the host's scheduler accounting, not a clock.** On Windows `GetProcessTimes`
/// is accumulated per scheduler tick (~15.6 ms by default, the same quantum
/// [`crate::clock::sleep`] records), so a short call may report **zero** elapsed CPU time and a
/// sequence of readings advances in steps rather than smoothly. A caller that needs to measure a
/// microsecond of work does not have a tool here; a caller asking "how much CPU has this process
/// burned" does.
///
/// # Errors
///
/// [`ProcessError::Unsupported`] on Linux and macOS, naming the intended POSIX call.
/// [`ProcessError::LastError`] if the OS refuses to report it.
pub fn cpu_time() -> ProcessResult<Duration> {
    backend::cpu_time()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The portable pair answer, and answer something usable.
    #[test]
    fn the_portable_facts_are_answered_on_every_target() {
        assert_ne!(pid(), 0, "no process has pid 0");
        assert!(cpu_count() >= 1, "a zero cpu count divides by zero in guest code that uses it");
    }

    /// Entropy: the buffer is filled, and two draws differ.
    ///
    /// **Structural, not statistical.** 64 bytes left at zero is the failure mode of a backend
    /// that reports success without writing, and two identical 64-byte draws is the failure mode
    /// of a backend that fills from a constant. The probability of a true source failing either
    /// assertion is 2^-512, which is not a flake risk — this is not a randomness *quality* test
    /// and does not pretend to be one.
    #[test]
    #[cfg_attr(not(target_os = "windows"), ignore = "no entropy backend on this target")]
    fn random_bytes_fills_the_whole_buffer_and_does_not_repeat() {
        let mut first = [0u8; 64];
        let mut second = [0u8; 64];
        random_bytes(&mut first).expect("the host entropy source");
        random_bytes(&mut second).expect("the host entropy source");
        assert_ne!(first, [0u8; 64], "the buffer was reported filled and is all zero");
        assert_ne!(first, second, "two draws from a real entropy source cannot be equal");
        // A short buffer must be filled to its end and no further: the tail sentinel is the half a
        // length bug would step on.
        let mut framed = [0xAAu8; 8];
        random_bytes(&mut framed[..4]).expect("the host entropy source");
        assert_eq!(&framed[4..], &[0xAA; 4], "random_bytes wrote past the slice it was given");
        // An empty request succeeds and touches nothing.
        let mut nothing: [u8; 0] = [];
        random_bytes(&mut nothing).expect("an empty request is a no-op");
    }

    /// Process CPU time advances under work, never goes backwards, and **is not wall time**.
    ///
    /// **The obvious version of the third assertion is wrong here, and the first attempt at it
    /// failed for exactly that reason.** "A sleep charges no CPU" is true of a *thread* clock and
    /// false of a process one: this figure covers every thread of the process, and in the
    /// whole-workspace run libtest has several other tests executing while this one sleeps.
    /// MEASURED, on the run that caught it: a 50 ms sleep was charged **93.75 ms** of process CPU
    /// time, which is not a defect in the primitive but the primitive working as documented.
    ///
    /// So the discrimination is made the other way, and it is one a wall clock cannot fake:
    /// **several threads burning CPU for one interval of wall time advance a process CPU clock by
    /// MORE than that interval.** Interference from other threads only makes the assertion easier,
    /// which is the direction a shared-process measurement has to be robust in.
    ///
    /// n = one 50 ms busy loop for the advance, plus `min(4, available_parallelism)` threads
    /// burning one 100 ms wall interval for the discrimination. The busy-loop bound is the OS's
    /// accounting quantum rather than the elapsed time, because the charge arrives in ticks.
    #[test]
    #[cfg_attr(not(target_os = "windows"), ignore = "no process-cpu-time backend on this target")]
    fn process_cpu_time_advances_with_work_and_outruns_the_wall_clock() {
        use std::time::Instant;

        let first = cpu_time().expect("the host's process times");
        assert!(
            first < Duration::from_secs(60 * 60 * 24 * 365),
            "this process has not used {first:?} of CPU: that is an absolute FILETIME read as a \
             duration"
        );

        // Real work the optimiser cannot remove: the accumulator is read afterwards.
        let started = Instant::now();
        let mut acc: u64 = 0;
        let mut i: u64 = 0;
        while started.elapsed() < Duration::from_millis(50) {
            i = i.wrapping_add(1);
            acc = acc.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(i);
        }
        assert_ne!(acc, 0, "the busy loop must not be optimised away");
        let after_work = cpu_time().expect("the host's process times");
        assert!(
            after_work > first,
            "50 ms of arithmetic moved the process CPU clock not at all: {first:?} -> \
             {after_work:?}"
        );
        assert!(after_work >= first, "process CPU time went backwards");

        // The discrimination. A wall clock advances by the interval no matter how many threads
        // are running; a process CPU clock advances by the interval times the number of threads
        // that were on a core.
        let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        if threads < 2 {
            // Stated rather than skipped silently: on a single-core host the two clocks cannot be
            // separated this way, and inventing a weaker assertion would be worse than saying so.
            eprintln!("one logical processor: the CPU-versus-wall discrimination cannot be made");
            return;
        }
        let busy = threads.min(4);
        // The wall interval must **contain** the CPU interval rather than the other way round: a
        // CPU reading taken before the clock starts would span more wall time than `elapsed`
        // measures, and a wall-clock impostor would then satisfy the assertion below by accident.
        let wall = Instant::now();
        let before = cpu_time().expect("the host's process times");
        let handles: Vec<_> = (0..busy)
            .map(|_| {
                std::thread::spawn(|| {
                    let started = Instant::now();
                    let mut acc: u64 = 1;
                    while started.elapsed() < Duration::from_millis(100) {
                        acc = acc.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    }
                    acc
                })
            })
            .collect();
        for handle in handles {
            assert_ne!(handle.join().expect("a busy thread"), 0);
        }
        let charged = cpu_time().expect("the host's process times") - before;
        let elapsed = wall.elapsed();
        // Half again, rather than merely more: `busy` is at least two, so a real CPU clock
        // advances by about `busy` times the interval, and the margin is what stops a scheduling
        // hiccup in either direction deciding the test.
        assert!(
            charged > elapsed + elapsed / 2,
            "{busy} threads burned {elapsed:?} of wall time and the process CPU clock advanced by \
             only {charged:?}: a clock that cannot outrun wall time is a wall clock"
        );
    }

    /// Process CPU time is answered on Windows and refused by name elsewhere.
    #[test]
    fn process_cpu_time_is_answered_or_refused_by_name() {
        match cpu_time() {
            Ok(_) => assert!(cfg!(target_os = "windows"), "only Windows has a cpu-time backend"),
            Err(error) => {
                assert!(error.is_unsupported(), "{error}");
                let text = error.to_string();
                assert!(
                    text.contains("CLOCK_PROCESS_CPUTIME_ID"),
                    "the refusal must name its POSIX call: {text}"
                );
            }
        }
    }

    /// The current cpu is answered on Windows and is a refusal that names its POSIX call elsewhere.
    #[test]
    fn the_current_cpu_is_answered_or_refused_by_name() {
        match current_cpu() {
            Ok(_) => assert!(cfg!(target_os = "windows"), "only Windows has a cpu-id backend"),
            Err(error) => {
                assert!(error.is_unsupported(), "{error}");
                let text = error.to_string();
                assert!(text.contains("sched_getcpu"), "the refusal must name its POSIX call: {text}");
            }
        }
    }
}
