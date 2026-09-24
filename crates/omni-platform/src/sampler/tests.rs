//! The sampler seam, shown seeing things whose answer is known (`docs/VERIFICATION.md` entry 19:
//! an instrument that returns a default when it cannot see is indistinguishable from one that saw
//! the default). The same tests on every target with a backend; what differs is only how a test
//! makes executable memory and which library a blocked thread is expected in.

#![cfg(any(
    target_os = "windows",
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::*;

/// A thread spinning in this function, so a sample of it has a known answer.
#[inline(never)]
fn spin_until(stop: &AtomicBool, counter: &AtomicU64) {
    while !stop.load(Ordering::Relaxed) {
        counter.fetch_add(1, Ordering::Relaxed);
        std::hint::spin_loop();
    }
}

fn spinning_thread() -> (Arc<AtomicBool>, Arc<AtomicU64>, HostThread, std::thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let counter = Arc::new(AtomicU64::new(0));
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = {
        let stop = Arc::clone(&stop);
        let counter = Arc::clone(&counter);
        std::thread::spawn(move || {
            tx.send(HostThread::current().expect("a handle to this thread")).expect("sent");
            spin_until(&stop, &counter);
        })
    };
    let thread = rx.recv().expect("the thread's handle");
    while counter.load(Ordering::Relaxed) == 0 {
        std::hint::spin_loop();
    }
    (stop, counter, thread, handle)
}

/// A thread parked in the kernel until `unpark`ed, and a handle to it.
fn parked_thread() -> (HostThread, std::thread::JoinHandle<()>, Arc<AtomicBool>) {
    let done = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            tx.send(HostThread::current().expect("a handle to this thread")).expect("sent");
            while !done.load(Ordering::Acquire) {
                std::thread::park();
            }
        })
    };
    let thread = rx.recv().expect("the thread's handle");
    // Long enough to be in the kernel, and for macOS to have charged its last run.
    std::thread::sleep(Duration::from_millis(50));
    (thread, handle, done)
}

#[test]
fn a_spinning_thread_is_sampled_inside_this_image_and_keeps_running() {
    let _serial = serial();
    let (stop, counter, thread, handle) = spinning_thread();
    let exe = modules()
        .expect("the module list")
        .into_iter()
        .find(|m| {
            let f = spin_until as *const () as usize;
            f >= m.base && f < m.base + m.size
        })
        .expect("the module holding this test's code");
    let mut code = [0u8; CODE_BEFORE + CODE_AFTER];
    let mut inside = 0;
    for _ in 0..50 {
        let sample = thread.sample(&mut code).expect("a sample");
        assert_ne!(sample.ip, 0);
        if sample.ip >= exe.base && sample.ip < exe.base + exe.size {
            inside += 1;
            assert_eq!(memory_kind(sample.ip).expect("a kind"), MemoryKind::Image { base: exe.base });
            // The bytes read are the bytes that are there.
            let len = sample.code_end - sample.code_start;
            assert!(len >= CODE_AFTER, "at least the bytes from ip onwards are read");
            let from = sample.ip - (CODE_BEFORE - sample.code_start);
            // SAFETY: `from..from+len` was just read successfully from this image, which stays
            // mapped for the life of the process.
            let actual = unsafe { core::slice::from_raw_parts(from as *const u8, len) };
            assert_eq!(&code[sample.code_start..sample.code_end], actual);
        }
    }
    assert!(inside >= 40, "only {inside} of 50 samples of a thread spinning in this image landed in it");
    // Sampling resumed it every time: it is still counting.
    let before = counter.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(20));
    assert!(counter.load(Ordering::Relaxed) > before, "the sampled thread was left suspended");
    assert!(thread.cpu_time().expect("its CPU time") > Duration::ZERO);
    let first = thread.cycles().expect("its cycles");
    std::thread::sleep(Duration::from_millis(5));
    assert!(thread.cycles().expect("its cycles") > first, "a running thread's cycles did not move");
    stop.store(true, Ordering::Relaxed);
    handle.join().expect("joined");
}

/// "Did it run since the last tick" is the sampler's first question, asked at 100 Hz: a thread that
/// is running must move between any two looks a couple of milliseconds apart, and a thread asleep
/// must not move at all. (On macOS the charged time alone fails the first half -- MEASURED, it
/// moved in 152 of 1000 reads 1.5 ms apart -- which is why `cycles` there also counts the reads
/// that find the thread running.)
#[test]
fn a_running_thread_moves_between_ticks_and_a_parked_one_does_not() {
    let _serial = serial();
    let (stop, _counter, spinning, handle) = spinning_thread();
    let mut moved = 0;
    let mut last = spinning.cycles().expect("cycles");
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(2));
        let now = spinning.cycles().expect("cycles");
        assert!(now >= last, "cycles went backwards: {last} -> {now}");
        moved += u32::from(now > last);
        last = now;
    }
    stop.store(true, Ordering::Relaxed);
    handle.join().expect("joined");
    assert!(moved >= 18, "a spinning thread's cycles moved in only {moved} of 20 ticks");

    let (parked, handle, done) = parked_thread();
    let first = parked.cycles().expect("cycles");
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(4));
        assert_eq!(parked.cycles().expect("cycles"), first, "a parked thread's cycles moved");
    }
    done.store(true, Ordering::Release);
    handle.thread().unpark();
    handle.join().expect("joined");
}

/// A thread waiting in the kernel is sampled at the system-call stub it entered from, in the
/// library `crate::perf` counts as the operating system's.
#[test]
fn a_thread_blocked_in_the_kernel_is_sampled_in_the_system_library() {
    let _serial = serial();
    let expected: &[&str] = if cfg!(target_os = "windows") {
        &["ntdll.dll"]
    } else if cfg!(target_os = "linux") {
        &["libc.so.6"]
    } else {
        &["libsystem_kernel.dylib"]
    };
    let (thread, handle, done) = parked_thread();
    let modules = modules().expect("the module list");
    let mut code = [0u8; CODE_BEFORE + CODE_AFTER];
    for _ in 0..5 {
        let sample = thread.sample(&mut code).expect("a sample of a parked thread");
        let module = modules.iter().find(|m| sample.ip >= m.base && sample.ip < m.base + m.size);
        assert!(
            module.is_some_and(|m| expected.contains(&m.name.as_str())),
            "a parked thread was sampled at {:#x}, in {:?}; expected one of {expected:?}",
            sample.ip,
            module.map(|m| &m.name)
        );
    }
    done.store(true, Ordering::Release);
    handle.thread().unpark();
    handle.join().expect("the parked thread survived being sampled");
}

#[test]
fn a_thread_cannot_sample_itself() {
    let me = HostThread::current().expect("a handle to this thread");
    let mut code = [0u8; CODE_BEFORE + CODE_AFTER];
    assert_eq!(me.sample(&mut code), Err(SamplerError::SampledItself));
}

#[cfg(unix)]
#[test]
fn a_thread_that_has_exited_is_an_error_and_not_a_sample() {
    let _serial = serial();
    let thread = std::thread::spawn(|| HostThread::current().expect("a handle")).join().expect("joined");
    let mut code = [0u8; CODE_BEFORE + CODE_AFTER];
    let result = thread.sample(&mut code);
    assert!(
        matches!(result, Err(SamplerError::Errno { errno: libc::ESRCH, .. } | SamplerError::Kern { .. })),
        "{result:?}"
    );
    assert!(thread.cycles().is_err(), "an exited thread's clock was read");
}

#[test]
fn modules_are_named_and_do_not_overlap() {
    let mut all = modules().expect("the module list");
    assert!(all.len() >= 2, "{all:?}");
    all.sort_by_key(|m| m.base);
    for pair in all.windows(2) {
        assert!(pair[0].size > 0 && !pair[0].name.is_empty(), "{:?}", pair[0]);
        assert!(pair[0].base + pair[0].size <= pair[1].base, "{:?} overlaps {:?}", pair[0], pair[1]);
    }
}

/// Executable memory of this process's own, and plain data, of `size` bytes each.
#[cfg(target_os = "windows")]
fn executable_and_data(size: usize) -> (usize, usize) {
    use windows_sys::Win32::System::Memory::{
        VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_READWRITE,
    };
    // SAFETY: fresh allocations, released by `release`.
    let rwx = unsafe { VirtualAlloc(core::ptr::null(), size, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE) };
    // SAFETY: as above.
    let rw = unsafe { VirtualAlloc(core::ptr::null(), size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
    assert!(!rwx.is_null() && !rw.is_null());
    (rwx as usize, rw as usize)
}

#[cfg(target_os = "windows")]
fn release(address: usize, _size: usize) {
    use windows_sys::Win32::System::Memory::{VirtualFree, MEM_RELEASE};
    // SAFETY: came from `VirtualAlloc` in `executable_and_data`, released once.
    unsafe { VirtualFree(address as *mut core::ffi::c_void, 0, MEM_RELEASE) };
}

#[cfg(unix)]
fn executable_and_data(size: usize) -> (usize, usize) {
    let jit = if cfg!(target_os = "macos") { 0x800 } else { 0 }; // MAP_JIT: rwx on Apple silicon
    let map = |prot: libc::c_int, extra: libc::c_int| {
        // SAFETY: a fresh anonymous mapping, unmapped by `release`.
        let p = unsafe {
            libc::mmap(core::ptr::null_mut(), size, prot, libc::MAP_PRIVATE | libc::MAP_ANON | extra, -1, 0)
        };
        assert_ne!(p, libc::MAP_FAILED);
        p as usize
    };
    (map(libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC, jit), map(libc::PROT_READ | libc::PROT_WRITE, 0))
}

#[cfg(unix)]
fn release(address: usize, size: usize) {
    // SAFETY: came from `mmap` in `executable_and_data`, unmapped once.
    unsafe { libc::munmap(address as *mut libc::c_void, size) };
}

#[test]
fn writable_executable_private_memory_is_told_apart_from_data_and_images() {
    let _serial = serial();
    const SIZE: usize = 0x10000;
    let (rwx, rw) = executable_and_data(SIZE);
    assert_eq!(
        memory_kind(rwx + 0x100).expect("a kind"),
        MemoryKind::PrivateWritableExecutable { base: rwx }
    );
    assert_eq!(memory_kind(rw).expect("a kind"), MemoryKind::Other);
    release(rwx, SIZE);
    release(rw, SIZE);
    // Linux answers a known mapping from maps read up to a second ago.
    #[cfg(target_os = "linux")]
    std::thread::sleep(super::linux::CACHE_FRESH);
    assert_eq!(memory_kind(rwx).expect("a kind"), MemoryKind::Other, "released");
}

#[test]
fn the_machine_reports_an_efficiency_class_for_every_processor_this_thread_can_run_on() {
    let classes = efficiency_classes().expect("the processor sets");
    let cpus = std::thread::available_parallelism().expect("a count").get();
    assert!(classes.len() >= cpus.min(64), "{} classes for {cpus} processors", classes.len());
    let here = crate::process::current_cpu().expect("the current processor") as usize;
    assert!(here < classes.len());
}

/// The classes assume the efficiency cores are numbered first. The machine's own record of each
/// processor's cluster is the device tree's `cpuN` nodes (`logical-cpu-id`, `cluster-type` `E` or
/// `P`), so the numbering is checked against that rather than taken from a table. (A
/// `QOS_CLASS_BACKGROUND` thread is *not* a usable check: MEASURED on the M1, one ran 480 of
/// 500 ms on processor 4, a performance core -- the scheduler spills it when it likes.)
#[cfg(target_os = "macos")]
#[test]
fn the_classes_match_the_device_trees_cluster_types() {
    let classes = efficiency_classes().expect("the performance levels");
    let output = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-l", "-p", "IODeviceTree", "-r", "-n", "cpus"])
        .output()
        .expect("ioreg runs");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut clusters = Vec::new();
    for node in text.split("+-o ") {
        let id = node.lines().find_map(|l| {
            l.split_once("\"logical-cpu-id\" = ").and_then(|(_, v)| v.trim().parse::<usize>().ok())
        });
        let kind = node.lines().find_map(|l| l.split_once("\"cluster-type\" = <\"").map(|(_, v)| v.starts_with('E')));
        if let (Some(id), Some(efficient)) = (id, kind) {
            clusters.push((id, efficient));
        }
    }
    assert_eq!(clusters.len(), classes.len(), "{clusters:?} against {classes:?}");
    let fastest = classes.iter().copied().max().expect("classes");
    for (id, efficient) in clusters {
        assert_eq!(classes[id] < fastest, efficient, "processor {id}: classes {classes:?}");
    }
}

#[test]
fn the_process_counters_move_when_memory_is_touched() {
    let before = process_counters().expect("counters");
    let block = std::hint::black_box(vec![1u8; 8 << 20]);
    let after = process_counters().expect("counters");
    assert!(after.page_faults > before.page_faults, "{before:?} -> {after:?}");
    assert!(after.private_bytes > 0 && after.working_set > 0);
    drop(block);
}
