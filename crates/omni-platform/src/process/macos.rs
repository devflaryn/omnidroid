//! macOS backend for the process seam.
//!
//! Implemented and run on macOS 26.5, Apple M1. Each entry is the host's own answer:
//!
//! * [`random_bytes`] is `arc4random_buf(3)`, which cannot fail and cannot return short -- three
//!   lines here where Linux's `getrandom` needs a loop, which is why the two do not share a body.
//! * [`current_cpu`] is `pthread_cpu_number_np(3)`, public since macOS 11. The note this file used
//!   to carry ("no `sched_getcpu` and no supported replacement") predates it. Measured: it answers
//!   0 on every thread, main or not (`rc 0`, the number of the core the thread is on).
//! * [`set_current_thread_nice`] maps a Linux nice value onto a **thread QoS class**, macOS's own
//!   per-thread scheduling level -- `setpriority(PRIO_PROCESS)` is per *process* here and would
//!   reprioritise every guest thread at once. The mapping mirrors the Windows backend's five bands
//!   one for one; see [`qos_for_nice`]. Measured: a freshly created thread reports
//!   `QOS_CLASS_DEFAULT` (0x15), so nice 0 is where a thread starts.
//! * [`current_thread_host_priority`] is `qos_class_self()`, in the host's own numbering.
//! * [`host_manufacturer`] is the `manufacturer` property of the `IOPlatformExpertDevice` registry
//!   entry, which the firmware fills ("Apple Inc." on this machine).
//! * [`cpu_time`] is `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)`: nanosecond resolution, the whole
//!   process.

use std::time::Duration;

use super::{ProcessError, ProcessResult};

type QosClass = libc::c_uint;

const QOS_CLASS_USER_INTERACTIVE: QosClass = 0x21;
const QOS_CLASS_USER_INITIATED: QosClass = 0x19;
const QOS_CLASS_DEFAULT: QosClass = 0x15;
const QOS_CLASS_UTILITY: QosClass = 0x11;
const QOS_CLASS_BACKGROUND: QosClass = 0x09;

extern "C" {
    fn arc4random_buf(buf: *mut libc::c_void, nbytes: libc::size_t);
    fn pthread_cpu_number_np(cpu_number_out: *mut libc::size_t) -> libc::c_int;
    fn pthread_set_qos_class_self_np(qos_class: QosClass, relative_priority: libc::c_int)
        -> libc::c_int;
    fn qos_class_self() -> QosClass;
}

pub(super) fn random_bytes(out: &mut [u8]) -> ProcessResult<()> {
    // SAFETY: `out` is a live, writable buffer of exactly `out.len()` bytes.
    unsafe { arc4random_buf(out.as_mut_ptr().cast(), out.len()) };
    Ok(())
}

pub(super) fn current_cpu() -> ProcessResult<u32> {
    let mut cpu: libc::size_t = 0;
    // SAFETY: `cpu` is a live size_t the call writes.
    let code = unsafe { pthread_cpu_number_np(&mut cpu) };
    if code != 0 {
        return Err(ProcessError::Errno {
            operation: "current_cpu",
            api: "pthread_cpu_number_np",
            code,
        });
    }
    u32::try_from(cpu).map_err(|_| ProcessError::Errno {
        operation: "current_cpu",
        api: "pthread_cpu_number_np",
        code: libc::EOVERFLOW,
    })
}

/// The QoS class a clamped nice value maps to: the Windows backend's five bands, one for one.
///
/// | nice | Windows level | macOS QoS class |
/// |---|---|---|
/// | -20 ..= -11 | `THREAD_PRIORITY_HIGHEST` | `QOS_CLASS_USER_INTERACTIVE` |
/// | -10 ..= -1 | `THREAD_PRIORITY_ABOVE_NORMAL` | `QOS_CLASS_USER_INITIATED` |
/// | 0 | `THREAD_PRIORITY_NORMAL` | `QOS_CLASS_DEFAULT` (where a new thread starts, measured) |
/// | 1 ..= 9 | `THREAD_PRIORITY_BELOW_NORMAL` | `QOS_CLASS_UTILITY` |
/// | 10 ..= 19 | `THREAD_PRIORITY_LOWEST` | `QOS_CLASS_BACKGROUND` |
///
/// On Apple silicon the class also steers which cores a thread may use: `BACKGROUND` is kept on
/// the efficiency cores and its I/O is throttled, which is what Android's `BACKGROUND` nice band
/// (10) asks for on a big.LITTLE device too.
fn qos_for_nice(nice: i32) -> QosClass {
    match nice {
        i32::MIN..=-11 => QOS_CLASS_USER_INTERACTIVE,
        -10..=-1 => QOS_CLASS_USER_INITIATED,
        0 => QOS_CLASS_DEFAULT,
        1..=9 => QOS_CLASS_UTILITY,
        _ => QOS_CLASS_BACKGROUND,
    }
}

pub(super) fn set_current_thread_nice(nice: i32) -> ProcessResult<()> {
    // SAFETY: acts on the calling thread only and touches no memory of ours.
    let code = unsafe { pthread_set_qos_class_self_np(qos_for_nice(nice), 0) };
    if code != 0 {
        return Err(ProcessError::Errno {
            operation: "set_current_thread_nice",
            api: "pthread_set_qos_class_self_np",
            code,
        });
    }
    Ok(())
}

pub(super) fn current_thread_host_priority() -> ProcessResult<i32> {
    // SAFETY: reads the calling thread's QoS class; cannot fail.
    Ok(unsafe { qos_class_self() } as i32)
}

// IOKit and CoreFoundation, for the one registry property `host_manufacturer` reads.
type CfTypeRef = *const libc::c_void;
type IoObject = libc::c_uint;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    static kIOMainPortDefault: libc::c_uint;
    fn IOServiceMatching(name: *const libc::c_char) -> CfTypeRef;
    fn IOServiceGetMatchingService(main_port: libc::c_uint, matching: CfTypeRef) -> IoObject;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CfTypeRef,
        allocator: CfTypeRef,
        options: libc::c_uint,
    ) -> CfTypeRef;
    fn IOObjectRelease(object: IoObject) -> libc::c_int;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(
        allocator: CfTypeRef,
        text: *const libc::c_char,
        encoding: u32,
    ) -> CfTypeRef;
    fn CFGetTypeID(value: CfTypeRef) -> libc::c_ulong;
    fn CFDataGetTypeID() -> libc::c_ulong;
    fn CFDataGetBytePtr(data: CfTypeRef) -> *const u8;
    fn CFDataGetLength(data: CfTypeRef) -> libc::c_long;
    fn CFStringGetTypeID() -> libc::c_ulong;
    fn CFStringGetCString(
        string: CfTypeRef,
        buffer: *mut libc::c_char,
        size: libc::c_long,
        encoding: u32,
    ) -> u8;
    fn CFRelease(value: CfTypeRef);
}

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

pub(super) fn host_manufacturer() -> ProcessResult<String> {
    let failed = |api: &'static str| ProcessError::Errno {
        operation: "host_manufacturer",
        api,
        code: libc::ENOENT,
    };
    // SAFETY: the name is a NUL-terminated literal; IOServiceMatching returns a +1 dictionary that
    // IOServiceGetMatchingService consumes.
    let service = unsafe {
        IOServiceGetMatchingService(
            kIOMainPortDefault,
            IOServiceMatching(c"IOPlatformExpertDevice".as_ptr()),
        )
    };
    if service == 0 {
        return Err(failed("IOServiceGetMatchingService(IOPlatformExpertDevice)"));
    }
    // SAFETY: a literal key; the created string is released below.
    let key = unsafe {
        CFStringCreateWithCString(core::ptr::null(), c"manufacturer".as_ptr(), CF_STRING_ENCODING_UTF8)
    };
    // SAFETY: a live registry entry and key; the property is +1 and released below.
    let property = unsafe { IORegistryEntryCreateCFProperty(service, key, core::ptr::null(), 0) };
    // SAFETY: both were created above and are released exactly once.
    unsafe {
        CFRelease(key);
        IOObjectRelease(service);
    }
    if property.is_null() {
        return Err(failed("IORegistryEntryCreateCFProperty(manufacturer)"));
    }
    // The firmware stores it as bytes (CFData, NUL-terminated) on Apple silicon and as a string on
    // some Intel Macs; both are read, anything else is refused.
    // SAFETY: `property` is a live CF object until the CFRelease at the end.
    let text = unsafe {
        let kind = CFGetTypeID(property);
        let text = if kind == CFDataGetTypeID() {
            let bytes = std::slice::from_raw_parts(
                CFDataGetBytePtr(property),
                CFDataGetLength(property).max(0) as usize,
            );
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
        } else if kind == CFStringGetTypeID() {
            let mut buffer = [0 as libc::c_char; 256];
            (CFStringGetCString(property, buffer.as_mut_ptr(), 256, CF_STRING_ENCODING_UTF8) != 0)
                .then(|| std::ffi::CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned())
        } else {
            None
        };
        CFRelease(property);
        text
    };
    match text.map(|t| t.trim().to_string()) {
        Some(maker) if !maker.is_empty() => Ok(maker),
        _ => Err(failed("the manufacturer property is empty or not text")),
    }
}

pub(super) fn cpu_time() -> ProcessResult<Duration> {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `now` is a live timespec the call writes.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut now) } != 0 {
        return Err(ProcessError::Errno {
            operation: "cpu_time",
            api: "clock_gettime(CLOCK_PROCESS_CPUTIME_ID)",
            code: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        });
    }
    Ok(Duration::new(now.tv_sec as u64, now.tv_nsec as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_bytes_are_not_all_zero_and_differ_between_calls() {
        let (mut a, mut b) = ([0u8; 64], [0u8; 64]);
        random_bytes(&mut a).expect("random");
        random_bytes(&mut b).expect("random");
        assert_ne!(a, [0u8; 64]);
        assert_ne!(a, b);
    }

    #[test]
    fn the_cpu_number_is_one_of_this_machines_cores() {
        let cores = std::thread::available_parallelism().expect("cores").get() as u32;
        for _ in 0..100 {
            let cpu = current_cpu().expect("cpu number");
            assert!(cpu < cores, "cpu {cpu} of {cores}");
        }
    }

    /// A constant would pass the bound above; busy threads spread over the cores must see more
    /// than one number between them.
    #[test]
    fn busy_threads_see_more_than_one_cpu_number() {
        let cores = std::thread::available_parallelism().expect("cores").get();
        let seen: std::collections::BTreeSet<u32> = (0..cores)
            .map(|_| {
                std::thread::spawn(|| {
                    let started = std::time::Instant::now();
                    let mut mine = std::collections::BTreeSet::new();
                    while started.elapsed() < std::time::Duration::from_millis(200) {
                        mine.insert(current_cpu().expect("cpu number"));
                    }
                    mine
                })
            })
            .flat_map(|handle| handle.join().expect("a busy thread"))
            .collect();
        assert!(seen.len() > 1, "{cores} busy threads all reported {seen:?}");
    }

    #[test]
    fn nice_bands_move_the_threads_qos_class_and_nice_zero_is_where_it_started() {
        std::thread::spawn(|| {
            let start = current_thread_host_priority().expect("qos");
            assert_eq!(start, QOS_CLASS_DEFAULT as i32, "a fresh thread starts at DEFAULT");
            for (nice, expected) in [
                (-16, QOS_CLASS_USER_INTERACTIVE),
                (-4, QOS_CLASS_USER_INITIATED),
                (5, QOS_CLASS_UTILITY),
                (19, QOS_CLASS_BACKGROUND),
                (0, QOS_CLASS_DEFAULT),
            ] {
                set_current_thread_nice(nice).expect("apply");
                assert_eq!(current_thread_host_priority().expect("qos"), expected as i32, "nice {nice}");
            }
        })
        .join()
        .expect("the thread ran");
    }

    #[test]
    fn the_manufacturer_is_the_firmwares() {
        assert_eq!(host_manufacturer().expect("manufacturer"), "Apple Inc.");
    }

    #[test]
    fn process_cpu_time_moves_when_this_process_works() {
        let before = cpu_time().expect("cpu time");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut x = 0u64;
        while cpu_time().expect("cpu time") == before && std::time::Instant::now() < deadline {
            for i in 0..100_000u64 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(i);
            }
        }
        std::hint::black_box(x);
        assert!(cpu_time().expect("cpu time") > before);
    }
}
