//! **`ALooper`, driven by real translated ARM64 code.**
//!
//! ```text
//! cargo test -p omni-android --test ndk --release
//! ```
//!
//! `jni-surface.md` §8.1 ranks `ALooper_forThread()` returning null — which makes
//! `initializeNativeCode` return `0` and Java-side startup fail **silently** — fourth among the
//! failure modes to expect. Everything here is about making that condition, and the rest of the
//! looper's contract, observable before step 13 is first called rather than afterwards.
//!
//! Every test writes A64 instructions and asserts on the **value the guest got back**. A test that
//! only checked `run` returned `Ok` would pass against a handler that answered zero for
//! everything, which is what Global Constraint 1 is about — and here zero is the very value the
//! milestone's worst failure mode is made of.

#![cfg(target_arch = "x86_64")]

mod harness;

use std::sync::Arc;

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::{Bionic, ThreadHost};
use omni_android::ndk::{
    Ndk, ALOOPER_EVENT_HANGUP, ALOOPER_EVENT_INPUT, ALOOPER_EVENT_OUTPUT, ALOOPER_POLL_CALLBACK,
    ALOOPER_POLL_ERROR, ALOOPER_POLL_TIMEOUT, ALOOPER_PREPARE_ALLOW_NON_CALLBACKS, MAX_LOOPERS,
};
use omni_android::{AbiError, Boundary};
use omni_cpu::ExitReason;

/// A guest, a bionic instance, an NDK instance, and the boundary with everything bound.
struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    ndk: Arc<Ndk>,
    boundary: Arc<Boundary>,
    _root: Scratch,
}

/// A host directory that removes itself, so the instance has a filesystem root — which a looper
/// needs, because the descriptors it watches are the filesystem seam's.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-ndk-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        Scratch(at)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(tag: &str) -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let ndk = Ndk::new(Arc::clone(&guest.space)).expect("an NDK instance");
    let builder = guest.boundary(320);
    bionic.bind_into(&builder).expect("bind every bionic handler");
    ndk.bind_into(&builder).expect("bind every NDK handler");
    bionic.set_log_to_stderr(false);
    let root = Scratch::new(tag);
    bionic.set_filesystem_root(&root.0).expect("a filesystem root");
    let host: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&guest.backend) as _;
    bionic.set_thread_host(ThreadHost::new(host)).expect("a thread host");
    let boundary = builder.finish();
    Fixture { guest, bionic, ndk, boundary, _root: root }
}

impl Fixture {
    fn thunk(&self, symbol: &str) -> omni_cpu::GuestAddr {
        self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address
    }

    /// Run a program with **all three** instances published to this thread.
    fn run(&self, entry: omni_cpu::GuestAddr) -> Result<ExitReason, AbiError> {
        let _bionic = self.bionic.activate().expect("publish the bionic instance");
        let _ndk = self.ndk.activate();
        let mut cpu = self.guest.thread(&self.boundary);
        self.boundary.run(&mut cpu, entry, BUDGET)
    }

    /// Assemble a program that ends by storing `X0` at `data`, run it, and return the value.
    fn value_of(&self, symbol: &str, setup: impl FnOnce(&mut Asm)) -> u64 {
        let entry = self.program_calling(symbol, setup);
        let exit = self.run(entry).expect("the run must complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        self.guest.read_u64(self.guest.data)
    }

    /// The refusal a one-call program produced.
    fn refusal_of(&self, symbol: &str, setup: impl FnOnce(&mut Asm)) -> AbiError {
        let entry = self.program_calling(symbol, setup);
        match self.run(entry) {
            Err(error) => error,
            Ok(exit) => panic!("`{symbol}` completed with {exit:?} where a refusal was required"),
        }
    }

    fn program_calling(&self, symbol: &str, setup: impl FnOnce(&mut Asm)) -> omni_cpu::GuestAddr {
        let thunk = self.thunk(symbol);
        let entry = self.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        setup(&mut asm);
        asm.bl(thunk);
        asm.mov(22, self.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        self.guest.load(asm.words());
        entry
    }

    /// Build a pipe through the guest's own `pipe`, returning `(read end, write end)`.
    fn pipe(&self) -> (i32, i32) {
        let at = self.guest.data + 0x40;
        self.guest.write_u64(at, 0x5A5A_5A5A_5A5A_5A5A);
        let returned = self.value_of("pipe", |asm| {
            asm.mov(0, at as u64);
        });
        assert_eq!(returned as i64, 0, "pipe() failed");
        let raw = self.guest.read_u64(at);
        ((raw & 0xffff_ffff) as i32, (raw >> 32) as i32)
    }

    /// `ALooper_prepare(ALOOPER_PREPARE_ALLOW_NON_CALLBACKS)` through the guest.
    fn prepare(&self) -> u64 {
        self.value_of("ALooper_prepare", |asm| {
            asm.mov(0, ALOOPER_PREPARE_ALLOW_NON_CALLBACKS as u64);
        })
    }

    /// `ALooper_addFd`, returning what the guest got.
    fn add_fd(&self, looper: u64, fd: i32, ident: i32, events: i32, callback: u64, data: u64) -> i64 {
        self.value_of("ALooper_addFd", |asm| {
            asm.mov(0, looper);
            asm.mov(1, i64::from(fd) as u64);
            asm.mov(2, i64::from(ident) as u64);
            asm.mov(3, i64::from(events) as u64);
            asm.mov(4, callback);
            asm.mov(5, data);
        }) as i64
    }
}

// =================================================================== the fourth failure mode

/// **`ALooper_forThread` answers null before a looper is prepared, and the looper afterwards.**
///
/// This is §8.1's fourth failure mode as a measurement rather than a diagnosis: the null is a real
/// answer the engine branches on, and the *host's* job is to have prepared one. Both halves are
/// asserted, because a layer that always answered a looper would pass the second on its own.
#[test]
fn for_thread_is_null_until_a_looper_is_prepared_and_the_looper_afterwards() {
    let _guard = serialized();
    let f = fixture("for-thread");

    let before = f.value_of("ALooper_forThread", |_asm| {});
    assert_eq!(
        before, 0,
        "a thread with no looper must answer NULL: jni-surface.md §5.2 has initializeNativeCode \
         logging \"Unable to retrieve native ALooper\" and returning 0 on exactly this"
    );

    let prepared = f.prepare();
    assert_ne!(prepared, 0, "ALooper_prepare must produce one");
    let after = f.value_of("ALooper_forThread", |_asm| {});
    assert_eq!(after, prepared, "and the same thread gets the same looper back");

    // The host's own spelling agrees with the guest's, which is what a gate asserts *before*
    // calling step 13 rather than reading a zero back afterwards.
    assert_eq!(
        f.ndk.looper_for_current_thread(),
        Some(prepared as omni_cpu::GuestAddr),
        "Ndk::looper_for_current_thread and ALooper_forThread must be one answer"
    );
    assert_eq!(f.ndk.live_loopers(), 1);

    // Idempotent, as `ALooper_prepare` is.
    assert_eq!(f.prepare(), prepared, "a second prepare on one thread returns the first looper");
    assert_eq!(f.ndk.live_loopers(), 1, "and creates nothing");
}

/// **A looper identity is a real guest address inside this instance's arena, and a pointer this
/// layer did not hand out is refused.**
#[test]
fn a_looper_is_an_arena_address_and_a_forged_one_is_refused() {
    let _guard = serialized();
    let f = fixture("identity");
    let looper = f.prepare();
    let arena = f.ndk.arena() as u64;
    assert!(
        looper >= arena && looper < arena + f.ndk.arena_bytes() as u64,
        "{looper:#x} is not in the arena at {arena:#x}"
    );

    // Inside the arena but not on a slot boundary: the division would otherwise index whatever it
    // produced, which is the shape a checked handle exists to refuse.
    let error = f.refusal_of("ALooper_acquire", |asm| {
        asm.mov(0, looper + 4);
    });
    assert_eq!(error.symbol(), Some("ALooper_acquire"));
    assert!(error.to_string().contains("ALooper"), "{error}");

    // And a pointer nowhere near it.
    let error = f.refusal_of("ALooper_acquire", |asm| {
        asm.mov(0, 0xdead_beef);
    });
    assert!(error.to_string().contains("dead"), "the refusal must name the pointer: {error}");
}

/// **The reference count is a count**, and releasing one more than was acquired is a refusal.
///
/// Clamping at zero is the believable wrong answer: it keeps a looper alive that guest code
/// believes it has destroyed, and the next `pollOnce` answers for descriptors nobody owns.
#[test]
fn acquire_and_release_count_and_the_last_release_destroys_the_looper() {
    let _guard = serialized();
    let f = fixture("refcount");
    let looper = f.prepare();

    // Creation made one; the guest takes two more.
    for _ in 0..2 {
        let entry = f.program_calling("ALooper_acquire", |asm| {
            asm.mov(0, looper);
        });
        assert!(matches!(f.run(entry).expect("completes"), ExitReason::Returned { .. }));
    }
    assert_eq!(f.ndk.live_loopers(), 1);

    // Three releases take it to zero, and the third destroys it.
    for expected_live in [1usize, 1, 0] {
        let entry = f.program_calling("ALooper_release", |asm| {
            asm.mov(0, looper);
        });
        assert!(matches!(f.run(entry).expect("completes"), ExitReason::Returned { .. }));
        assert_eq!(f.ndk.live_loopers(), expected_live);
    }

    // **And the thread's binding went with it**, which is the condition §8.1's fourth failure
    // mode is about: `forThread` answers NULL again.
    assert_eq!(f.value_of("ALooper_forThread", |_asm| {}), 0);
    assert_eq!(f.ndk.looper_for_current_thread(), None);

    // A fourth release names a looper that is no longer live.
    let error = f.refusal_of("ALooper_release", |asm| {
        asm.mov(0, looper);
    });
    assert!(error.to_string().contains("not live"), "{error}");
}

/// **Releasing a reference the guest does not hold is refused, in one guest program.**
///
/// Two releases back to back: the first takes the count to zero and frees the slot, the second
/// finds nothing there. The refusal comes from the identity check rather than from a negative
/// count, and that is the whole shape of the thing — **a guard on `references < 0` is
/// unreachable**, because the slot is freed the moment the count reaches zero. The first version
/// of the handler had one; writing this test is what found it, and it was deleted rather than
/// kept as a branch no input can take.
#[test]
fn releasing_a_reference_the_guest_does_not_hold_is_refused() {
    let _guard = serialized();
    let f = fixture("underflow");
    let looper = f.prepare();
    let thunk = f.thunk("ALooper_release");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(23, looper);
    asm.push(mov_reg(0, 23));
    asm.bl(thunk);
    asm.push(mov_reg(0, 23));
    asm.bl(thunk);
    asm.push(ret(21));
    f.guest.load(asm.words());
    let error = match f.run(entry) {
        Err(error) => error,
        Ok(exit) => panic!("a release past the last reference completed with {exit:?}"),
    };
    assert_eq!(error.symbol(), Some("ALooper_release"));
    assert!(error.to_string().contains("not live"), "{error}");
    // And it really did free the slot rather than leaving a looper behind.
    assert_eq!(f.ndk.live_loopers(), 0);
}

// =================================================================== watching descriptors

/// **`addFd` registers, `removeFd` reports whether it removed, and a second `addFd` replaces.**
#[test]
fn add_fd_registers_and_remove_fd_distinguishes_removed_from_never_watched() {
    let _guard = serialized();
    let f = fixture("addfd");
    let looper = f.prepare();
    let (read_fd, write_fd) = f.pipe();

    assert_eq!(f.add_fd(looper, read_fd, 1, ALOOPER_EVENT_INPUT, 0, 0xabcd), 1);
    let held = f.ndk.registrations(looper as omni_cpu::GuestAddr);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].fd, read_fd);
    assert_eq!(held[0].ident, 1, "with no callback, the caller's ident is kept");
    assert_eq!(held[0].data, 0xabcd);

    // A second registration for the same descriptor replaces the first, as AOSP's `addFd` does:
    // keeping both would report one descriptor twice.
    assert_eq!(f.add_fd(looper, read_fd, 7, ALOOPER_EVENT_INPUT, 0, 0x1234), 1);
    let held = f.ndk.registrations(looper as omni_cpu::GuestAddr);
    assert_eq!(held.len(), 1, "one descriptor, one registration");
    assert_eq!(held[0].ident, 7);
    assert_eq!(held[0].data, 0x1234);

    assert_eq!(f.add_fd(looper, write_fd, 2, ALOOPER_EVENT_OUTPUT, 0, 0), 1);
    assert_eq!(f.ndk.registrations(looper as omni_cpu::GuestAddr).len(), 2);

    let removed = f.value_of("ALooper_removeFd", |asm| {
        asm.mov(0, looper);
        asm.mov(1, i64::from(write_fd) as u64);
    });
    assert_eq!(removed as i64, 1, "1 when it was watched");
    let again = f.value_of("ALooper_removeFd", |asm| {
        asm.mov(0, looper);
        asm.mov(1, i64::from(write_fd) as u64);
    });
    assert_eq!(again as i64, 0, "0 when it was not: that is not a failure");
    assert_eq!(f.ndk.registrations(looper as omni_cpu::GuestAddr).len(), 1);
}

/// **A registration with a callback stores `ALOOPER_POLL_CALLBACK` as its ident**, as AOSP does.
///
/// §5.2's constructor passes `ident = 0` *and* a callback. Keeping the zero would make `pollOnce`
/// report ident 0 — a legal-looking answer the glue has no branch for.
#[test]
fn a_registration_with_a_callback_cannot_be_reported_by_ident() {
    let _guard = serialized();
    let f = fixture("callback-ident");
    let looper = f.prepare();
    let (read_fd, _write_fd) = f.pipe();
    // Any non-zero address; it is stored, not called, in this test.
    let callback = f.guest.code as u64;
    assert_eq!(f.add_fd(looper, read_fd, 0, ALOOPER_EVENT_INPUT, callback, 0), 1);
    let held = f.ndk.registrations(looper as omni_cpu::GuestAddr);
    assert_eq!(held[0].ident, ALOOPER_POLL_CALLBACK);
    assert_eq!(held[0].callback, callback as omni_cpu::GuestAddr);
}

/// **What `addFd` refuses**, each with the believable wrong answer it declines to give.
#[test]
fn add_fd_refuses_what_it_cannot_answer_for() {
    let _guard = serialized();
    let f = fixture("addfd-refusals");
    let looper = f.prepare();
    let (read_fd, _write_fd) = f.pipe();

    // A descriptor this instance does not hold: a looper's answer about a descriptor is the
    // filesystem seam's answer, and there is none for one not in the table.
    let error = f.refusal_of("ALooper_addFd", |asm| {
        asm.mov(0, looper);
        asm.mov(1, 61);
        asm.mov(2, 1);
        asm.mov(3, ALOOPER_EVENT_INPUT as u64);
        asm.mov(4, 0);
        asm.mov(5, 0);
    });
    assert!(error.to_string().contains("does not hold"), "{error}");

    // An output-only event asked for as a request. Accepting it would say this layer had
    // subscribed to something it reports unconditionally.
    let error = f.refusal_of("ALooper_addFd", |asm| {
        asm.mov(0, looper);
        asm.mov(1, i64::from(read_fd) as u64);
        asm.mov(2, 1);
        asm.mov(3, ALOOPER_EVENT_HANGUP as u64);
        asm.mov(4, 0);
        asm.mov(5, 0);
    });
    assert!(error.to_string().contains("ALOOPER_EVENT_INPUT"), "{error}");

    // Neither a callback nor a reportable ident: the registration could never be reported.
    let error = f.refusal_of("ALooper_addFd", |asm| {
        asm.mov(0, looper);
        asm.mov(1, i64::from(read_fd) as u64);
        asm.mov(2, u64::from(u32::MAX)); // -1 as an int
        asm.mov(3, ALOOPER_EVENT_INPUT as u64);
        asm.mov(4, 0);
        asm.mov(5, 0);
    });
    assert!(error.to_string().contains("never be reported"), "{error}");
}

// =================================================================== pollOnce

/// **`pollOnce` reports the ident of a ready descriptor and fills all three out-parameters.**
///
/// The assertion is on `outFd`, `outEvents` and `outData` separately, against sentinels: a handler
/// that returned the right ident and wrote nothing is what this distinguishes.
#[test]
fn poll_once_reports_an_ident_and_writes_every_out_parameter() {
    let _guard = serialized();
    let f = fixture("poll-ident");
    let looper = f.prepare();
    let (read_fd, write_fd) = f.pipe();
    assert_eq!(f.add_fd(looper, read_fd, 9, ALOOPER_EVENT_INPUT, 0, 0xfeed_face), 1);

    let out = f.guest.data + 0x100;
    f.guest.write_u64(out, 0x5A5A_5A5A_5A5A_5A5A);
    f.guest.write_u64(out + 8, 0x5A5A_5A5A_5A5A_5A5A);
    f.guest.write_u64(out + 16, 0x5A5A_5A5A_5A5A_5A5A);

    // Nothing in the pipe yet: a zero timeout is POLL_TIMEOUT, and nothing is written.
    let returned = f.value_of("ALooper_pollOnce", |asm| {
        asm.mov(0, 0);
        asm.mov(1, out as u64);
        asm.mov(2, (out + 8) as u64);
        asm.mov(3, (out + 16) as u64);
    });
    assert_eq!(returned as i32, ALOOPER_POLL_TIMEOUT, "nothing is ready");
    assert_eq!(
        f.guest.read_u64(out),
        0x5A5A_5A5A_5A5A_5A5A,
        "a timeout writes no out-parameter: the sentinel survives"
    );

    // One byte, and the ident comes back with all three parameters.
    let source = f.guest.data + 0x200;
    f.guest.write_u64(source, 0x41);
    let written = f.value_of("write", |asm| {
        asm.mov(0, i64::from(write_fd) as u64);
        asm.mov(1, source as u64);
        asm.mov(2, 1);
    });
    assert_eq!(written as i64, 1);

    let returned = f.value_of("ALooper_pollOnce", |asm| {
        asm.mov(0, 0);
        asm.mov(1, out as u64);
        asm.mov(2, (out + 8) as u64);
        asm.mov(3, (out + 16) as u64);
    });
    assert_eq!(returned as i32, 9, "the ident the guest registered");
    assert_eq!(f.guest.read_u64(out) as u32 as i32, read_fd, "outFd");
    assert_eq!(f.guest.read_u64(out + 8) as u32 as i32, ALOOPER_EVENT_INPUT, "outEvents");
    assert_eq!(f.guest.read_u64(out + 16), 0xfeed_face, "outData");
}

/// **A callback is real guest code, it is called with the NDK's three arguments, and returning
/// zero removes the registration.**
///
/// The callback writes its own arguments into guest memory, so what is asserted is what the guest
/// function actually received in `X0`, `X1` and `X2` — not what this layer believes it passed.
#[test]
fn poll_once_calls_a_guest_callback_with_its_arguments_and_zero_removes_it() {
    let _guard = serialized();
    let f = fixture("poll-callback");
    let looper = f.prepare();
    let (read_fd, write_fd) = f.pipe();

    // int callback(int fd, int events, void *data) { record = {fd, events, data}; return 1; }
    let record = f.guest.data + 0x300;
    let callback = f.guest.next_entry();
    let mut asm = Asm::at(callback);
    asm.mov(23, record as u64);
    asm.push(str_imm(0, 23, 0));
    asm.push(str_imm(1, 23, 8));
    asm.push(str_imm(2, 23, 16));
    asm.mov(0, 1); // keep the registration
    asm.push(ret(30));
    f.guest.load(asm.words());

    assert_eq!(f.add_fd(looper, read_fd, 0, ALOOPER_EVENT_INPUT, callback as u64, 0xc0de), 1);
    for offset in [0usize, 8, 16] {
        f.guest.write_u64(record + offset, 0x5A5A_5A5A_5A5A_5A5A);
    }

    let source = f.guest.data + 0x200;
    f.guest.write_u64(source, 0x42);
    assert_eq!(
        f.value_of("write", |asm| {
            asm.mov(0, i64::from(write_fd) as u64);
            asm.mov(1, source as u64);
            asm.mov(2, 1);
        }) as i64,
        1
    );

    let returned = f.value_of("ALooper_pollOnce", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
    });
    assert_eq!(returned as i32, ALOOPER_POLL_CALLBACK, "a callback ran, so POLL_CALLBACK");
    assert_eq!(f.guest.read_u64(record) as u32 as i32, read_fd, "the callback's fd argument");
    assert_eq!(
        f.guest.read_u64(record + 8) as u32 as i32,
        ALOOPER_EVENT_INPUT,
        "the callback's events argument"
    );
    assert_eq!(f.guest.read_u64(record + 16), 0xc0de, "the callback's data argument");
    assert_eq!(
        f.ndk.registrations(looper as omni_cpu::GuestAddr).len(),
        1,
        "a callback returning non-zero keeps its registration"
    );

    // **Returning zero removes it**, which is the NDK's documented contract and how the glue
    // detaches its pipe. A second callback that returns 0, over the byte still in the pipe.
    let dropper = f.guest.next_entry();
    let mut asm = Asm::at(dropper);
    asm.mov(0, 0);
    asm.push(ret(30));
    f.guest.load(asm.words());
    assert_eq!(f.add_fd(looper, read_fd, 0, ALOOPER_EVENT_INPUT, dropper as u64, 0), 1);
    let returned = f.value_of("ALooper_pollOnce", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
    });
    assert_eq!(returned as i32, ALOOPER_POLL_CALLBACK);
    assert!(
        f.ndk.registrations(looper as omni_cpu::GuestAddr).is_empty(),
        "a callback returning 0 asks to be removed"
    );
}

/// **An ident is reported before any callback runs**, which is AOSP's order and the one the glue
/// depends on.
///
/// `android_app_entry` registers its command pipe with `LOOPER_ID_MAIN` and no callback, and
/// `GameLoop` switches on the return. A layer that ran callbacks first would answer
/// `ALOOPER_POLL_CALLBACK` to a poll that had an ident waiting.
#[test]
fn an_ident_is_reported_before_a_callback_is_run() {
    let _guard = serialized();
    let f = fixture("poll-order");
    let looper = f.prepare();
    let (ident_read, ident_write) = f.pipe();
    let (callback_read, callback_write) = f.pipe();

    let ran = f.guest.data + 0x300;
    f.guest.write_u64(ran, 0);
    let callback = f.guest.next_entry();
    let mut asm = Asm::at(callback);
    asm.mov(23, ran as u64);
    asm.mov(24, 1);
    asm.push(str_imm(24, 23, 0));
    asm.mov(0, 1);
    asm.push(ret(30));
    f.guest.load(asm.words());

    // The callback registration goes in FIRST, so an implementation that simply took the first
    // ready entry would take it.
    assert_eq!(f.add_fd(looper, callback_read, 0, ALOOPER_EVENT_INPUT, callback as u64, 0), 1);
    assert_eq!(f.add_fd(looper, ident_read, 5, ALOOPER_EVENT_INPUT, 0, 0), 1);

    let source = f.guest.data + 0x200;
    f.guest.write_u64(source, 0x43);
    for fd in [callback_write, ident_write] {
        assert_eq!(
            f.value_of("write", |asm| {
                asm.mov(0, i64::from(fd) as u64);
                asm.mov(1, source as u64);
                asm.mov(2, 1);
            }) as i64,
            1
        );
    }

    let returned = f.value_of("ALooper_pollOnce", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
    });
    assert_eq!(returned as i32, 5, "the ident, although the callback was registered first");
    assert_eq!(f.guest.read_u64(ran), 0, "and the callback did not run");
}

/// **`pollOnce` on a thread with no looper is `ALOOPER_POLL_ERROR`**, which is an answer the
/// caller has a branch for and is not a value anything stores.
#[test]
fn poll_once_without_a_looper_is_poll_error() {
    let _guard = serialized();
    let f = fixture("poll-no-looper");
    let returned = f.value_of("ALooper_pollOnce", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
    });
    assert_eq!(returned as i32, ALOOPER_POLL_ERROR);
}

/// **An indefinite `pollOnce` is refused by name**, with the same argument `poll(fds, n, -1)` is.
///
/// A host thread parked on a descriptor nobody writes to cannot be ended by a step budget, because
/// a sleeping thread executes no guest instructions (D16). Returning `ALOOPER_POLL_TIMEOUT`
/// instead would report a timeout to a call that was given none.
#[test]
fn an_indefinite_poll_once_is_refused_and_says_what_would_change_it() {
    let _guard = serialized();
    let f = fixture("poll-indefinite");
    let _looper = f.prepare();
    let error = f.refusal_of("ALooper_pollOnce", |asm| {
        asm.mov(0, u64::from(u32::MAX)); // -1 as an int
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
    });
    assert_eq!(error.symbol(), Some("ALooper_pollOnce"));
    let text = error.to_string();
    assert!(text.contains("indefinite"), "{text}");
    assert!(text.contains("host-driven event source"), "it must say what would change it: {text}");
}

/// **A `pollOnce` with a timeout waits and returns when a writer arrives**, and the assertion is
/// the value rather than the elapsed time.
///
/// If the wait were absent, the poll would answer `ALOOPER_POLL_TIMEOUT` at once and this fails
/// deterministically. A slow machine makes it stop exercising the wait and still pass, which is
/// the safe direction — `VERIFICATION.md` entry 6.
#[test]
fn a_poll_once_with_a_timeout_waits_for_a_writer() {
    let _guard = serialized();
    let f = fixture("poll-wait");
    let looper = f.prepare();
    let (read_fd, write_fd) = f.pipe();
    assert_eq!(f.add_fd(looper, read_fd, 3, ALOOPER_EVENT_INPUT, 0, 0), 1);

    let bionic = Arc::clone(&f.bionic);
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let fs = bionic.filesystem().expect("the instance has a root");
        fs.write(write_fd, b"G").expect("one byte into the pipe")
    });

    let returned = f.value_of("ALooper_pollOnce", |asm| {
        asm.mov(0, 5_000);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
    });
    assert_eq!(writer.join().expect("the writer thread"), 1);
    assert_eq!(
        returned as i32,
        3,
        "POLL_TIMEOUT here would mean the poll answered before the writer arrived rather than \
         waiting for it"
    );
}

// =================================================================== the instrumentation

/// **Every looper operation is recorded, by thread, and the record is what §8.1's fifth failure
/// mode asks for.**
///
/// Membership, not a count: the assertion is that each named operation is in the log with the
/// detail that identifies it, because a total with the wrong members would pass a count.
#[test]
fn the_event_log_names_every_operation_and_the_thread_that_made_it() {
    let _guard = serialized();
    let f = fixture("events");
    let looper = f.prepare();
    let (read_fd, _write_fd) = f.pipe();
    assert_eq!(f.add_fd(looper, read_fd, 4, ALOOPER_EVENT_INPUT, 0, 0), 1);
    let _ = f.value_of("ALooper_forThread", |_asm| {});
    let _ = f.value_of("ALooper_pollOnce", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
    });

    let events = f.ndk.events();
    assert_eq!(f.ndk.events_dropped(), 0, "nothing was dropped in a handful of operations");
    let kinds: std::collections::BTreeSet<&str> = events.iter().map(|e| e.what).collect();
    for what in ["prepare", "addFd", "forThread", "pollOnce"] {
        assert!(kinds.contains(what), "the log has no `{what}`: {events:?}");
    }
    let add = events.iter().find(|e| e.what == "addFd").expect("an addFd event");
    assert!(add.detail.contains(&format!("fd {read_fd}")), "{add:?}");
    assert_eq!(add.looper, looper as omni_cpu::GuestAddr);
    assert!(
        events.iter().all(|e| e.thread == 0),
        "one host thread made all of them, so they all carry thread 0: {events:?}"
    );

    // The census counts what was called, which is the other half of the measurement.
    let census = f.ndk.census();
    assert_eq!(census.get("ALooper_prepare").copied(), Some(1));
    assert_eq!(census.get("ALooper_addFd").copied(), Some(1));
    assert_eq!(census.get("ALooper_pollOnce").copied(), Some(1));
}

/// **The looper ceiling refuses rather than sharing**, and it names the cap.
#[test]
fn a_thread_past_the_looper_ceiling_is_refused_rather_than_given_anothers() {
    let _guard = serialized();
    let f = fixture("ceiling");
    // Fill the arena from the host, which is the only way to make MAX_LOOPERS threads cheaply.
    let mut handles = Vec::new();
    for _ in 0..MAX_LOOPERS {
        let ndk = Arc::clone(&f.ndk);
        handles.push(
            std::thread::spawn(move || {
                let at = ndk.prepare_looper();
                // Hold the thread alive until every looper is made, so no slot is reused.
                std::thread::sleep(std::time::Duration::from_millis(50));
                at
            }),
        );
    }
    let made: Vec<_> = handles.into_iter().map(|h| h.join().expect("a prepare thread")).collect();
    assert_eq!(made.iter().filter(|r| r.is_ok()).count(), MAX_LOOPERS);
    assert_eq!(f.ndk.live_loopers(), MAX_LOOPERS);

    // The calling thread is the seventeenth.
    let error = f.ndk.prepare_looper().expect_err("the arena is full");
    assert!(error.to_string().contains(&MAX_LOOPERS.to_string()), "{error}");
    let error = f.refusal_of("ALooper_prepare", |asm| {
        asm.mov(0, ALOOPER_PREPARE_ALLOW_NON_CALLBACKS as u64);
    });
    assert_eq!(error.symbol(), Some("ALooper_prepare"));
}

/// **A handler on a thread with no NDK activation refuses by name**, rather than building a
/// default registry that would give each guest thread its own private loopers.
#[test]
fn an_ndk_handler_without_an_activation_refuses_and_names_the_setup_call() {
    let _guard = serialized();
    let f = fixture("not-active");
    let entry = f.program_calling("ALooper_forThread", |_asm| {});
    let error = {
        // bionic only: the NDK instance is deliberately not published.
        let _bionic = f.bionic.activate().expect("publish the bionic instance");
        let mut cpu = f.guest.thread(&f.boundary);
        match f.boundary.run(&mut cpu, entry, BUDGET) {
            Err(error) => error,
            Ok(exit) => panic!("completed with {exit:?} where a refusal was required"),
        }
    };
    assert!(matches!(error, AbiError::NdkNotActive { .. }), "{error:?}");
    assert!(error.to_string().contains("Ndk::activate"), "{error}");
}
