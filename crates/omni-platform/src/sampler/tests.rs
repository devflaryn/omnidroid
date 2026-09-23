//! The sampler seam, shown seeing things whose answer is known (`docs/VERIFICATION.md` entry 19:
//! an instrument that returns a default when it cannot see is indistinguishable from one that saw
//! the default).

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

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

#[test]
fn a_spinning_thread_is_sampled_inside_this_image_and_keeps_running() {
    let (stop, counter, thread, handle) = spinning_thread();
    let exe = modules()
        .expect("the module list")
        .into_iter()
        .find(|m| {
            let f = spin_until as usize;
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
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(counter.load(Ordering::Relaxed) > before, "the sampled thread was left suspended");
    assert!(thread.cpu_time().expect("its CPU time") > std::time::Duration::ZERO);
    let first = thread.cycles().expect("its cycles");
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(thread.cycles().expect("its cycles") > first, "a running thread's cycles did not move");
    stop.store(true, Ordering::Relaxed);
    handle.join().expect("joined");
}

#[test]
fn a_thread_cannot_sample_itself() {
    let me = HostThread::current().expect("a handle to this thread");
    let mut code = [0u8; CODE_BEFORE + CODE_AFTER];
    assert_eq!(me.sample(&mut code), Err(SamplerError::SampledItself));
}

#[test]
fn writable_executable_private_memory_is_told_apart_from_data_and_images() {
    use windows_sys::Win32::System::Memory::{
        VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
        PAGE_READWRITE,
    };
    // SAFETY: fresh allocations, released below.
    let rwx = unsafe {
        VirtualAlloc(core::ptr::null(), 0x10000, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE)
    };
    // SAFETY: as above.
    let rw = unsafe { VirtualAlloc(core::ptr::null(), 0x10000, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
    assert!(!rwx.is_null() && !rw.is_null());
    assert_eq!(
        memory_kind(rwx as usize + 0x100).expect("a kind"),
        MemoryKind::PrivateWritableExecutable { base: rwx as usize }
    );
    assert_eq!(memory_kind(rw as usize).expect("a kind"), MemoryKind::Other);
    // SAFETY: both came from `VirtualAlloc` above and are released once.
    unsafe {
        VirtualFree(rwx, 0, MEM_RELEASE);
        VirtualFree(rw, 0, MEM_RELEASE);
    }
    assert_eq!(memory_kind(rwx as usize).expect("a kind"), MemoryKind::Other, "released");
}

#[test]
fn the_machine_reports_an_efficiency_class_for_every_processor_this_thread_can_run_on() {
    let classes = efficiency_classes().expect("the processor sets");
    let cpus = std::thread::available_parallelism().expect("a count").get();
    assert!(classes.len() >= cpus.min(64), "{} classes for {cpus} processors", classes.len());
    let here = crate::process::current_cpu().expect("the current processor") as usize;
    assert!(here < classes.len());
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
