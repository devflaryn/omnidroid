//! The host's heap, measured: what an allocation costs (one thread, many threads, the kernel's
//! checked copy that allocates) and what the heap keeps committed -- after a churn of mixed sizes
//! across threads, and for many host threads that each hold a little (the system host process runs
//! ~950). Built twice for an A/B: as is (the platform's allocator) and with `--features mimalloc`
//! (the allocator `omni-linux-run` then uses too).
//! `cargo test --release -p omni-linux --test alloc_cost [--features mimalloc] -- --nocapture`.
use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};
use std::sync::{Arc, Barrier};
use std::time::Instant;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static HEAP: mimalloc::MiMalloc = mimalloc::MiMalloc;

const HEAP_NAME: &str = if cfg!(feature = "mimalloc") { "mimalloc" } else { "system" };

fn commit_mib() -> f64 {
    omni_platform::vm::process_memory().map_or(f64::NAN, |m| m.commit_charge as f64 / 1048576.0)
}

/// A cheap deterministic sequence (xorshift).
fn next(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

#[test]
fn the_heap_costs_this_much() {
    let start = commit_mib();
    let mut rows = Vec::new();

    // 1. One thread: a small and a page-sized allocation, made and freed.
    for size in [32usize, 4096] {
        const N: u32 = 2_000_000;
        let t0 = Instant::now();
        for _ in 0..N {
            std::hint::black_box(Vec::<u8>::with_capacity(size));
        }
        rows.push(format!("alloc+free {size} B {:.1} ns", t0.elapsed().as_nanos() as f64 / f64::from(N)));
    }

    // 2. The kernel's checked copy that allocates (`GuestMem::read`), 32 bytes.
    {
        let m = manifest::parse("d\t755\t/\n").unwrap();
        let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
        let p = Process::for_tests(vfs, Output::Capture(Default::default()));
        let s = p.scratch();
        p.mem.write(s, &[1u8; 4096]).unwrap();
        const N: u32 = 1_000_000;
        let t0 = Instant::now();
        for _ in 0..N {
            std::hint::black_box(p.mem.read(s, 32).unwrap());
        }
        rows.push(format!("mem.read 32 B {:.1} ns", t0.elapsed().as_nanos() as f64 / f64::from(N)));
    }

    // 3. Eight threads churning mixed sizes (each keeps 256 alive, replacing one per step), with a
    //    quarter of the frees made by another thread (what a channel between threads does).
    {
        const THREADS: usize = 8;
        const STEPS: u64 = 500_000;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1024);
        let rx = Arc::new(parking_lot::Mutex::new(rx));
        let go = Arc::new(Barrier::new(THREADS + 1));
        let mut handles = Vec::new();
        for i in 0..THREADS {
            let (tx, rx, go) = (tx.clone(), Arc::clone(&rx), Arc::clone(&go));
            handles.push(std::thread::spawn(move || {
                let mut x = 0x9e37_79b9_7f4a_7c15u64 ^ (i as u64 + 1);
                let mut live: Vec<Vec<u8>> = (0..256).map(|_| Vec::new()).collect();
                go.wait();
                for step in 0..STEPS {
                    let r = next(&mut x);
                    let size = [16usize, 48, 128, 512, 2048, 8192][(r % 6) as usize];
                    let mut v = Vec::with_capacity(size);
                    v.push(r as u8);
                    let old = std::mem::replace(&mut live[(r >> 8) as usize % 256], v);
                    if step % 4 == 0 {
                        let _ = tx.try_send(old);
                        let got = rx.lock().try_recv();
                        drop(got);
                    }
                }
            }));
        }
        drop(tx);
        go.wait();
        let t0 = Instant::now();
        for h in handles {
            h.join().unwrap();
        }
        let ns = t0.elapsed().as_nanos() as f64 / (THREADS as f64 * STEPS as f64);
        rows.push(format!("8-thread churn {ns:.1} ns/step (wall per step per thread)"));
    }
    let after_churn = commit_mib();

    // 4. 256 host threads that each hold ~2 KB of small objects and wait: what an idle host
    //    process's threads keep committed in the heap.
    let before_threads = commit_mib();
    {
        const THREADS: usize = 256;
        let held = Arc::new(Barrier::new(THREADS + 1));
        let release = Arc::new(Barrier::new(THREADS + 1));
        let mut handles = Vec::new();
        for i in 0..THREADS {
            let (held, release) = (Arc::clone(&held), Arc::clone(&release));
            handles.push(std::thread::Builder::new().stack_size(64 * 1024).spawn(move || {
                let keep: Vec<Vec<u8>> = (0..32).map(|k| vec![(i + k) as u8; 16 + (k % 8) * 16]).collect();
                held.wait();
                release.wait();
                drop(keep);
            }).unwrap());
        }
        held.wait();
        let with_threads = commit_mib();
        rows.push(format!(
            "256 threads holding ~2 KB each: +{:.1} MiB committed ({:.1} KiB a thread, stacks included)",
            with_threads - before_threads,
            (with_threads - before_threads) * 1024.0 / THREADS as f64
        ));
        release.wait();
        for h in handles {
            h.join().unwrap();
        }
    }
    rows.push(format!("committed after the churn +{:.1} MiB, at the end +{:.1} MiB", after_churn - start, commit_mib() - start));
    eprintln!("[alloc-cost] heap {HEAP_NAME}: {}", rows.join("; "));
}
