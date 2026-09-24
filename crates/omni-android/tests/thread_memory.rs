//! **What a guest thread created through `pthread_create` really costs.**
//!
//! M3 task 3 phase 3c is what makes the 16 MiB-per-guest-thread blocker live: until it, nothing
//! in the runtime could create a guest thread, so the figure was about a `GuestCpu` somebody
//! might make rather than about anything the guest could ask for. Now the guest asks.
//!
//! This is a **measurement, not a test**, and it is `#[ignore]`d for the reason
//! `omni-cpu/tests/bench.rs` ignores its own: it reads `process_commit_charge`, which is
//! process-global, so a concurrent benchmark or suite moves it underneath the reading. Run it on
//! its own:
//!
//! ```text
//! cargo test -p omni-android --test thread_memory --release -- --ignored --nocapture
//! ```
//!
//! It is a separate target rather than a case in `tests/bionic.rs` so that it cannot share a
//! process with 112 other tests that map and unmap guest memory.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

mod harness;

use std::sync::Arc;

use harness::a64::*;
use harness::{Asm, Guest, BUDGET};
use omni_android::bionic::{Bionic, ThreadHost};
use omni_android::{Boundary, BoundaryBuilder};
use omni_cpu::ExitReason;

/// How many guest threads the measurement creates. Stated with every figure it produces.
const THREADS: usize = 8;

const MIB: f64 = (1024 * 1024) as f64;

fn mib(bytes: i64) -> f64 {
    bytes as f64 / MIB
}

/// Create [`THREADS`] guest threads through the guest's own `pthread_create`, measure, join.
///
/// Three readings, because they answer different questions and one number answers none of them:
/// what an instance costs before its guest has any threads, what the threads add once they exist,
/// and what comes back when they exit.
///
/// # MEASURED, and the figures this produced
///
/// n = **4 runs of 8 threads**, release, on the D2 development host, each run in its own process:
///
/// | | |
/// |---|---|
/// | per guest thread, while running | **24.76 - 24.84 MiB** |
/// | returned when they were joined | **98.6%** of it: the residual is 2.79 - 3.18 MiB in total, 0.35 - 0.40 MiB per thread |
/// | instance + boundary + the first context | 25.14 - 25.16 MiB, nearly all of which is that one context |
///
/// The per-thread figure agrees with `omni-cpu/tests/bench.rs`'s **24.56 MiB/thread** for a raw
/// context with no adapter and no guest stack, to within 1%, which says the adapter's own
/// per-thread cost is small: a 1 MiB stack that is lazily committed and of which a spinning
/// thread touches one page, and an arena block that was already committed when the instance was
/// built.
///
/// **The reclamation figure is the one that matters for the multi-instance requirement.** "Unused
/// memory genuinely reclaimed" is non-negotiable, and what these runs show is that a guest thread
/// that exits gives back essentially all of what it took — so the cost is of *concurrent* guest
/// threads rather than of threads ever created.
#[test]
#[ignore = "measurement, not a test"]
fn the_commit_charge_of_a_guest_thread_created_by_the_guest() {
    let before_instance = omni_mem::process_commit_charge().expect("commit charge");

    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder: BoundaryBuilder = guest.boundary(256);
    bionic.bind_into(&builder).expect("bind every handler");
    bionic.set_log_to_stderr(false);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&guest.backend) as _;
    bionic
        .set_thread_host(ThreadHost::new(backend).with_limit(THREADS))
        .expect("a thread host");
    let boundary: Arc<Boundary> = builder.finish();

    let out = guest.data + 0x100;
    let gate = guest.data + 0x80;
    let live = guest.data + 0x88;
    guest.write_u64(gate, 0);
    guest.write_u64(live, 0);

    // A guest thread that says it is up, then spins until the gate opens. Spinning rather than
    // sleeping on purpose: a thread that is executing is a thread whose code cache has been
    // written to, which is the half of the cost a fresh context does not yet have.
    let start = {
        let entry = guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.mov(9, live as u64);
        asm.push(ldr_imm(10, 9, 0));
        asm.push(add_imm(10, 10, 1));
        asm.push(str_imm(10, 9, 0));
        asm.mov(9, gate as u64);
        let loop_at = asm.pc();
        asm.push(ldr_imm(10, 9, 0));
        asm.push(subs_imm(10, 10, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.mov(0, 0);
        asm.push(ret(30));
        guest.load(asm.words());
        entry
    };

    // Every program is assembled before any guest thread runs: `Guest::load` reprotects the whole
    // code region, and doing that under a running guest thread faults it.
    let create = {
        let entry = guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        for slot in 0..THREADS {
            asm.mov(0, out as u64 + (slot * 32) as u64);
            asm.mov(1, 0);
            asm.mov(2, start as u64);
            asm.mov(3, 0);
            asm.bl(thunk(&boundary, "pthread_create"));
            asm.mov(22, out as u64 + (slot * 32) as u64);
            asm.push(str_imm(0, 22, 8));
        }
        asm.push(ret(21));
        guest.load(asm.words());
        entry
    };
    let join = {
        let entry = guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        for slot in 0..THREADS {
            asm.mov(22, out as u64 + (slot * 32) as u64);
            asm.push(ldr_imm(0, 22, 0));
            asm.mov(1, 0);
            asm.bl(thunk(&boundary, "pthread_join"));
            asm.push(str_imm(0, 22, 16));
        }
        asm.push(ret(21));
        guest.load(asm.words());
        entry
    };

    let mut cpu = guest.thread(&boundary);
    let after_instance = omni_mem::process_commit_charge().expect("commit charge");

    {
        let _active = bionic.activate().expect("a thread block");
        let exit = boundary.run(&mut cpu, create, BUDGET).expect("the creates complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    }
    // **`SP` and `X30` back where they started before the second program runs on this context.**
    // The first program's `BL`s left `X30` pointing into the middle of itself, and the second
    // program's `MOV X21, X30` would save that and `RET X21` into it -- which is a loop, not a
    // return, and it shows up as the run's whole budget being spent. `tests/bionic.rs` never sees
    // it because every program there gets a fresh context; this one reuses one so that a second
    // context's 24 MiB does not land in the middle of the measurement.
    for slot in 0..THREADS {
        assert_eq!(
            guest.read_u64(out + slot * 32 + 8),
            0,
            "every pthread_create must succeed for the figure to be per {THREADS} threads"
        );
    }
    // Wait until every one of them is really executing, so the reading is of threads that exist
    // rather than of threads that are being created.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while guest.read_u64(live) < THREADS as u64 {
        assert!(std::time::Instant::now() < deadline, "the guest threads never all started");
        std::thread::yield_now();
    }
    // And give them a moment to have translated their loop and grown their code caches; the
    // reading is taken twice so that a still-rising figure is visible rather than averaged away.
    let running = omni_mem::process_commit_charge().expect("commit charge");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let settled = omni_mem::process_commit_charge().expect("commit charge");

    guest.write_u64(gate, 1);
    {
        guest.rearm(&mut cpu, &boundary);
        let _active = bionic.activate().expect("a thread block");
        let exit = boundary.run(&mut cpu, join, BUDGET).expect("the joins complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    }
    for slot in 0..THREADS {
        assert_eq!(guest.read_u64(out + slot * 32 + 16), 0, "every join must succeed");
    }
    assert_eq!(bionic.live_guest_threads(), 0);
    let after_join = omni_mem::process_commit_charge().expect("commit charge");

    println!("\n== commit charge of a guest thread the GUEST created (n = 1 run, N = {THREADS}) ==");
    println!(
        "  method: `omni_mem::process_commit_charge` (Windows `PrivateUsage`) around a real\n\
         \x20         `pthread_create` x{THREADS} from translated ARM64 code, each thread spinning\n\
         \x20         on a guest word until joined. Serialized by being its own test target."
    );
    let row = |what: &str, value: u64, base: u64, per: bool| {
        let delta = value as i64 - base as i64;
        if per {
            println!(
                "  {what:<44}: {:+10} bytes ({:8.3} MiB), {:7.3} MiB/thread",
                delta,
                mib(delta),
                mib(delta) / THREADS as f64
            );
        } else {
            println!("  {what:<44}: {:+10} bytes ({:8.3} MiB)", delta, mib(delta));
        }
    };
    row("instance + boundary + one context", after_instance, before_instance, false);
    row("after the guest created its threads", running, after_instance, true);
    row("250 ms later, still spinning", settled, after_instance, true);
    row("after they were joined", after_join, after_instance, true);
    row("net, against the empty process", after_join, before_instance, false);
    println!(
        "  Against `omni-cpu/tests/bench.rs::the_commit_charge_of_a_guest_thread`, which measures\n\
         \x20 the same thing one layer down (raw contexts, no adapter, no guest stack) at **24.56\n\
         \x20 MiB/thread**, n = 1 run of 8 contexts. What this layer adds per thread is one lazily\n\
         \x20 committed 1 MiB stack -- of which a spinning thread touches one page -- and an arena\n\
         \x20 block that was already committed when the instance was built.\n\
         \x20 16 MiB of the per-thread figure is `A64EmitX64`'s fixed fast-dispatch table, which is\n\
         \x20 allocated and written by the constructor for a feature D16 runs DISABLED. The fork\n\
         \x20 patch is written up in `crates/dynarmic-sys/patches/README.md` item 4 and is NOT\n\
         \x20 applied: D5 pins the vendored tree byte-for-byte unmodified.\n"
    );
}

fn thunk(boundary: &Arc<Boundary>, symbol: &str) -> omni_cpu::GuestAddr {
    boundary
        .slot_named(symbol)
        .unwrap_or_else(|| panic!("`{symbol}` is not bound"))
        .address
}
