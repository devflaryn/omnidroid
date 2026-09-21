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
use omni_android::jni::Jni;
use omni_android::ndk::assets::{AssetTable, ASSET_MANAGER_CLASS};
use omni_android::ndk::config::{ACONFIGURATION_SCREENSIZE_LARGE, ACONFIGURATION_SCREENSIZE_NORMAL};
use omni_android::ndk::{
    DeviceConfiguration, Ndk, ScreenSize, WindowGeometry, ACONFIGURATION_NAVHIDDEN_NO,
    ALOOPER_EVENT_HANGUP, ALOOPER_EVENT_INPUT, ALOOPER_EVENT_OUTPUT, ALOOPER_POLL_CALLBACK,
    ALOOPER_POLL_ERROR, ALOOPER_POLL_TIMEOUT, ALOOPER_PREPARE_ALLOW_NON_CALLBACKS, MAX_LOOPERS,
    MAX_NATIVE_WINDOWS, MAX_OPEN_ASSETS, SURFACE_CLASS,
};
use omni_android::{AbiError, Boundary};
use omni_cpu::ExitReason;

/// A guest, a bionic instance, an NDK instance, and the boundary with everything bound.
struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    jni: Arc<Jni>,
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
    let jni = Jni::new(Arc::clone(&guest.space)).expect("a JNI instance");
    let builder = guest.boundary(640);
    bionic.bind_into(&builder).expect("bind every bionic handler");
    ndk.bind_into(&builder).expect("bind every NDK handler");
    jni.install_into(&builder).expect("install the JNI tables");
    bionic.set_log_to_stderr(false);
    let root = Scratch::new(tag);
    bionic.set_filesystem_root(&root.0).expect("a filesystem root");
    let host: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&guest.backend) as _;
    bionic.set_thread_host(ThreadHost::new(host)).expect("a thread host");
    let boundary = builder.finish();
    Fixture { guest, bionic, jni, ndk, boundary, _root: root }
}

impl Fixture {
    fn thunk(&self, symbol: &str) -> omni_cpu::GuestAddr {
        self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address
    }

    /// Run a program with **all three** instances published to this thread.
    fn run(&self, entry: omni_cpu::GuestAddr) -> Result<ExitReason, AbiError> {
        let _bionic = self.bionic.activate().expect("publish the bionic instance");
        let _jni = self.jni.activate().expect("publish the JNI instance");
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

// =================================================================== AAssetManager and AAsset

/// The `DeviceConfiguration` these tests decide on. Every field is a *decision*, which is the
/// whole point of there being no default — see `ndk::config`.
fn a_configuration() -> DeviceConfiguration {
    DeviceConfiguration {
        language: *b"en",
        country: *b"GB",
        screen_width_dp: 411,
        screen_height_dp: 731,
        screen_size: ScreenSize::Normal,
        nav_hidden: ACONFIGURATION_NAVHIDDEN_NO,
    }
}

/// A fixture with assets and a decided configuration, and the `jobject` that stands for the Java
/// `AssetManager` step 13 is handed.
fn with_assets(tag: &str) -> (Fixture, u64) {
    let f = fixture(tag);
    f.ndk
        .set_asset_source(omni_android::ndk::assets::source(
            AssetTable::new()
                .with("shaders/blit.vert", &b"#version 320 es"[..])
                .with("empty.bin", Vec::new()),
        ))
        .expect("an asset source");
    f.ndk.set_configuration(a_configuration());
    let object = f.jni.new_object(ASSET_MANAGER_CLASS).expect("a Java AssetManager");
    (f, object)
}

/// Read `len` bytes out of guest memory.
fn read_bytes(f: &Fixture, at: omni_cpu::GuestAddr, len: usize) -> Vec<u8> {
    (0..len)
        .map(|offset| {
            let address = at + offset;
            (f.guest.read_u64(address & !7) >> (8 * (address & 7))) as u8
        })
        .collect()
}

/// **`AAssetManager_fromJava` checks the `jobject` is really an `AssetManager`.**
///
/// Accepting any non-null value would turn a wrong argument — a `Configuration`, a stale handle —
/// into an asset manager that answers null for every asset, thousands of instructions from the
/// mistake. Asked twice with the same object it returns the **same** manager, which is what a
/// device does.
#[test]
fn asset_manager_from_java_checks_the_class_and_is_idempotent() {
    let _guard = serialized();
    let (f, object) = with_assets("from-java");

    let manager = f.value_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, object);
    });
    assert_ne!(manager, 0, "a real AAssetManager");
    assert_eq!(f.ndk.live_asset_managers(), 1);

    let again = f.value_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, object);
    });
    assert_eq!(again, manager, "the same jobject gets the same manager");
    assert_eq!(f.ndk.live_asset_managers(), 1, "and no second one is made");

    // A `jobject` of another class.
    let wrong = f.jni.new_object("android/content/res/Configuration").expect("a Configuration");
    let error = f.refusal_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, wrong);
    });
    assert!(error.to_string().contains("Configuration"), "the refusal names the class: {error}");

    // A handle nobody issued, and a null.
    let error = f.refusal_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0xdead_beef);
    });
    assert!(error.to_string().contains("live jobject"), "{error}");
    let error = f.refusal_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
    });
    assert!(error.to_string().contains("null"), "{error}");
}

/// **An instance with no asset source refuses by name and says which call supplies one.**
#[test]
fn an_instance_with_no_asset_source_refuses_and_names_the_setter() {
    let _guard = serialized();
    let f = fixture("no-assets");
    assert!(!f.ndk.has_asset_source());
    let object = f.jni.new_object(ASSET_MANAGER_CLASS).expect("a Java AssetManager");
    let error = f.refusal_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, object);
    });
    assert_eq!(error.symbol(), Some("AAssetManager_fromJava"));
    assert!(error.to_string().contains("Ndk::set_asset_source"), "{error}");
}

/// **An asset opens, reports its length, reads in pieces, and ends at end of file.**
#[test]
fn an_asset_opens_reads_in_pieces_and_ends_at_end_of_file() {
    let _guard = serialized();
    let (f, object) = with_assets("read");
    let manager = f.value_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, object);
    });

    let name = f.guest.data + 0x80;
    f.guest.write_bytes(name, b"shaders/blit.vert\0");
    let asset = f.value_of("AAssetManager_open", |asm| {
        asm.mov(0, manager);
        asm.mov(1, name as u64);
        asm.mov(2, 2); // AASSET_MODE_STREAMING
    });
    assert_ne!(asset, 0, "the asset is in the table, so it opens");
    assert_eq!(f.ndk.live_assets(), 1);

    let length = f.value_of("AAsset_getLength", |asm| {
        asm.mov(0, asset);
    });
    assert_eq!(length as i64, 15, "\"#version 320 es\" is fifteen bytes");

    // Four bytes, then the rest, then end of file. **In pieces on purpose**: a handler that
    // ignored its own position would answer the first four bytes every time and every count
    // would still look right.
    let buffer = f.guest.data + 0x200;
    f.guest.write_u64(buffer, 0x5A5A_5A5A_5A5A_5A5A);
    let read = f.value_of("AAsset_read", |asm| {
        asm.mov(0, asset);
        asm.mov(1, buffer as u64);
        asm.mov(2, 4);
    });
    assert_eq!(read as i64, 4);
    assert_eq!(&read_bytes(&f, buffer, 4), b"#ver");

    let read = f.value_of("AAsset_read", |asm| {
        asm.mov(0, asset);
        asm.mov(1, buffer as u64);
        asm.mov(2, 64);
    });
    assert_eq!(read as i64, 11, "the rest, not the whole asset again");
    assert_eq!(&read_bytes(&f, buffer, 11), b"sion 320 es");

    let read = f.value_of("AAsset_read", |asm| {
        asm.mov(0, asset);
        asm.mov(1, buffer as u64);
        asm.mov(2, 64);
    });
    assert_eq!(read as i64, 0, "end of file");

    let entry = f.program_calling("AAsset_close", |asm| {
        asm.mov(0, asset);
    });
    assert!(matches!(f.run(entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.ndk.live_assets(), 0);
}

/// **An asset that is not there is a null, not a refusal.** The engine probes for optional
/// content, and "there is no such asset" is an ordinary answer every caller branches on.
#[test]
fn an_asset_that_is_not_there_is_null() {
    let _guard = serialized();
    let (f, object) = with_assets("missing");
    let manager = f.value_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, object);
    });
    let name = f.guest.data + 0x80;
    f.guest.write_bytes(name, b"nope/not-here.bin\0");
    let asset = f.value_of("AAssetManager_open", |asm| {
        asm.mov(0, manager);
        asm.mov(1, name as u64);
        asm.mov(2, 3);
    });
    assert_eq!(asset, 0, "null, and the call did not fail");
    assert_eq!(f.ndk.live_assets(), 0, "and nothing was allocated for it");
}

/// **`AAsset_getBuffer` maps the bytes where the guest can read them, once, read-only.**
///
/// The assertion is that the **guest** can load bytes out of the returned pointer — through real
/// translated code — because "returns a plausible address" is exactly what a stub would do.
#[test]
fn asset_get_buffer_maps_the_bytes_once_and_the_guest_can_read_them() {
    let _guard = serialized();
    let (f, object) = with_assets("buffer");
    let manager = f.value_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, object);
    });
    let name = f.guest.data + 0x80;
    f.guest.write_bytes(name, b"shaders/blit.vert\0");
    let asset = f.value_of("AAssetManager_open", |asm| {
        asm.mov(0, manager);
        asm.mov(1, name as u64);
        asm.mov(2, 3);
    });

    let buffer = f.value_of("AAsset_getBuffer", |asm| {
        asm.mov(0, asset);
    });
    assert_ne!(buffer, 0, "a real pointer");
    let again = f.value_of("AAsset_getBuffer", |asm| {
        asm.mov(0, asset);
    });
    assert_eq!(again, buffer, "the same pointer every time: that is what makes it safe to store");

    // **The guest reads it**, which is the only assertion that distinguishes a mapping from a
    // number: `memcmp` against the expected text, through the boundary.
    let expected = f.guest.data + 0x300;
    f.guest.write_bytes(expected, b"#version 320 es");
    let same = f.value_of("memcmp", |asm| {
        asm.mov(0, buffer);
        asm.mov(1, expected as u64);
        asm.mov(2, 15);
    });
    assert_eq!(same as i64, 0, "the mapped bytes are the asset's bytes");

    // A zero-length asset maps nothing and answers NULL, which is the NDK's answer and not an
    // error: the caller has already learned the length is zero.
    f.guest.write_bytes(name, b"empty.bin\0");
    let empty = f.value_of("AAssetManager_open", |asm| {
        asm.mov(0, manager);
        asm.mov(1, name as u64);
        asm.mov(2, 3);
    });
    assert_ne!(empty, 0, "an empty asset still opens");
    assert_eq!(
        f.value_of("AAsset_getLength", |asm| {
            asm.mov(0, empty);
        }) as i64,
        0
    );
    assert_eq!(
        f.value_of("AAsset_getBuffer", |asm| {
            asm.mov(0, empty);
        }),
        0,
        "and its buffer is NULL rather than a mapping of nothing"
    );
}

/// **`AAsset_openFileDescriptor` refuses, and the refusal explains the measurement behind it.**
#[test]
fn asset_open_file_descriptor_refuses_because_every_entry_is_deflated() {
    let _guard = serialized();
    let (f, _object) = with_assets("fd");
    let error = f.refusal_of("AAsset_openFileDescriptor", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
    });
    assert_eq!(error.symbol(), Some("AAsset_openFileDescriptor"));
    let text = error.to_string();
    assert!(text.contains("DEFLATED"), "{text}");
    assert!(text.contains("AAsset_read"), "it must name the fallback: {text}");
}

/// The open-asset ceiling answers **null**, which is how a device reports failing to open one.
#[test]
fn the_open_asset_ceiling_answers_null() {
    let _guard = serialized();
    let (f, object) = with_assets("asset-ceiling");
    let manager = f.value_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, object);
    });
    let name = f.guest.data + 0x80;
    f.guest.write_bytes(name, b"shaders/blit.vert\0");
    for index in 0..MAX_OPEN_ASSETS {
        let asset = f.value_of("AAssetManager_open", |asm| {
            asm.mov(0, manager);
            asm.mov(1, name as u64);
            asm.mov(2, 3);
        });
        assert_ne!(asset, 0, "open {index} must succeed");
    }
    assert_eq!(f.ndk.live_assets(), MAX_OPEN_ASSETS);
    let past = f.value_of("AAssetManager_open", |asm| {
        asm.mov(0, manager);
        asm.mov(1, name as u64);
        asm.mov(2, 3);
    });
    assert_eq!(past, 0, "the cap is a null, not a refusal");
}

// =================================================================== AConfiguration

/// **A configuration is empty until `fromAssetManager` fills it, and reading an empty one
/// refuses.**
///
/// Answering zero would be this layer inventing "language `\0\0`, screen size ANY" — a
/// legal-looking configuration the engine would act on. On a device `AConfiguration_new` gives
/// back whatever the allocator left.
#[test]
fn a_configuration_is_empty_until_it_is_filled_and_reading_an_empty_one_refuses() {
    let _guard = serialized();
    let (f, object) = with_assets("config");
    let manager = f.value_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, object);
    });

    let config = f.value_of("AConfiguration_new", |_asm| {});
    assert_ne!(config, 0);
    assert_eq!(f.ndk.live_configurations(), 1);

    let error = f.refusal_of("AConfiguration_getScreenWidthDp", |asm| {
        asm.mov(0, config);
    });
    assert!(error.to_string().contains("never filled"), "{error}");

    let entry = f.program_calling("AConfiguration_fromAssetManager", |asm| {
        asm.mov(0, config);
        asm.mov(1, manager);
    });
    assert!(matches!(f.run(entry).expect("completes"), ExitReason::Returned { .. }));

    assert_eq!(
        f.value_of("AConfiguration_getScreenWidthDp", |asm| {
            asm.mov(0, config);
        }) as i64,
        411
    );
    assert_eq!(
        f.value_of("AConfiguration_getScreenHeightDp", |asm| {
            asm.mov(0, config);
        }) as i64,
        731
    );
    assert_eq!(
        f.value_of("AConfiguration_getScreenSize", |asm| {
            asm.mov(0, config);
        }) as i64,
        i64::from(ACONFIGURATION_SCREENSIZE_NORMAL)
    );
    assert_eq!(
        f.value_of("AConfiguration_getNavHidden", |asm| {
            asm.mov(0, config);
        }) as i64,
        i64::from(ACONFIGURATION_NAVHIDDEN_NO)
    );

    // **Two bytes, unterminated.** The header says `char out[2]`; a third byte — even a NUL —
    // writes past the object the caller gave, so the sentinel after the pair must survive.
    let out = f.guest.data + 0x300;
    f.guest.write_u64(out, 0x5A5A_5A5A_5A5A_5A5A);
    let entry = f.program_calling("AConfiguration_getLanguage", |asm| {
        asm.mov(0, config);
        asm.mov(1, out as u64);
    });
    assert!(matches!(f.run(entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(&read_bytes(&f, out, 2), b"en");
    assert_eq!(read_bytes(&f, out + 2, 1)[0], 0x5A, "the third byte is untouched");

    f.guest.write_u64(out, 0x5A5A_5A5A_5A5A_5A5A);
    let entry = f.program_calling("AConfiguration_getCountry", |asm| {
        asm.mov(0, config);
        asm.mov(1, out as u64);
    });
    assert!(matches!(f.run(entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(&read_bytes(&f, out, 2), b"GB");
    assert_eq!(read_bytes(&f, out + 2, 1)[0], 0x5A, "the third byte is untouched");

    // **A copy, not a reference.** A device's configuration changing does not change one the
    // guest already holds, which is why `onConfigurationChanged` exists.
    f.ndk.set_configuration(DeviceConfiguration {
        screen_width_dp: 1_280,
        screen_size: ScreenSize::Large,
        ..a_configuration()
    });
    assert_eq!(
        f.value_of("AConfiguration_getScreenWidthDp", |asm| {
            asm.mov(0, config);
        }) as i64,
        411,
        "the configuration the guest holds does not change under it"
    );

    // Re-reading through `fromAssetManager` is how a guest on a device picks up the change.
    let entry = f.program_calling("AConfiguration_fromAssetManager", |asm| {
        asm.mov(0, config);
        asm.mov(1, manager);
    });
    assert!(matches!(f.run(entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(
        f.value_of("AConfiguration_getScreenWidthDp", |asm| {
            asm.mov(0, config);
        }) as i64,
        1_280
    );
    assert_eq!(
        f.value_of("AConfiguration_getScreenSize", |asm| {
            asm.mov(0, config);
        }) as i64,
        i64::from(ACONFIGURATION_SCREENSIZE_LARGE)
    );

    let entry = f.program_calling("AConfiguration_delete", |asm| {
        asm.mov(0, config);
    });
    assert!(matches!(f.run(entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.ndk.live_configurations(), 0);
    // A second delete finds nothing, and says so rather than making a double delete invisible.
    let error = f.refusal_of("AConfiguration_delete", |asm| {
        asm.mov(0, config);
    });
    assert!(error.to_string().contains("not a live"), "{error}");
}

/// **An instance whose configuration nobody decided refuses by name.**
#[test]
fn an_undecided_configuration_refuses_and_names_the_setter() {
    let _guard = serialized();
    let f = fixture("undecided");
    f.ndk
        .set_asset_source(omni_android::ndk::assets::source(AssetTable::new()))
        .expect("an asset source");
    assert_eq!(f.ndk.configuration(), None);
    let object = f.jni.new_object(ASSET_MANAGER_CLASS).expect("a Java AssetManager");
    let manager = f.value_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, object);
    });
    let config = f.value_of("AConfiguration_new", |_asm| {});
    let error = f.refusal_of("AConfiguration_fromAssetManager", |asm| {
        asm.mov(0, config);
        asm.mov(1, manager);
    });
    assert_eq!(error.symbol(), Some("AConfiguration_fromAssetManager"));
    assert!(error.to_string().contains("Ndk::set_configuration"), "{error}");
}

/// **Handles of different kinds are not interchangeable**, although they all live in one arena.
///
/// The check that makes this true is `Slots::index_of`, which is per-kind: each family has its own
/// range. Without it an `AConfiguration *` passed where an `AAssetManager *` belongs would index a
/// table it does not belong to.
#[test]
fn a_handle_of_one_kind_is_refused_where_another_belongs() {
    let _guard = serialized();
    let (f, object) = with_assets("kinds");
    let looper = f.prepare();
    let manager = f.value_of("AAssetManager_fromJava", |asm| {
        asm.mov(0, 0);
        asm.mov(1, object);
    });
    let config = f.value_of("AConfiguration_new", |_asm| {});
    assert!(looper != manager && manager != config, "three distinct identities");

    // A looper where an asset manager belongs.
    let name = f.guest.data + 0x80;
    f.guest.write_bytes(name, b"shaders/blit.vert\0");
    let error = f.refusal_of("AAssetManager_open", |asm| {
        asm.mov(0, looper);
        asm.mov(1, name as u64);
        asm.mov(2, 3);
    });
    assert!(error.to_string().contains("did not hand that out"), "{error}");

    // A configuration where a looper belongs.
    let error = f.refusal_of("ALooper_acquire", |asm| {
        asm.mov(0, config);
    });
    assert!(error.to_string().contains("ALooper"), "{error}");

    // An asset manager where a configuration belongs.
    let error = f.refusal_of("AConfiguration_getScreenSize", |asm| {
        asm.mov(0, manager);
    });
    assert!(error.to_string().contains("did not hand that out"), "{error}");
}

// =================================================================== ANativeWindow

/// A geometry the tests use, and a second one for the resize.
///
/// **1440x3120 rather than 1920x1080.** Not because the numbers matter — nothing here reads them
/// but the assertions — but because 1920x1080 is the exact value `ndk::window` names as the
/// believable wrong answer, and a test whose expected value is the invented default cannot tell
/// the refusal from the invention.
fn a_geometry() -> WindowGeometry {
    WindowGeometry::new(1440, 3120).expect("a positive geometry")
}

/// A fixture with a decided window geometry and a Java `Surface` to make a window from.
fn with_surface(tag: &str) -> (Fixture, u64) {
    let f = fixture(tag);
    f.ndk.set_window_geometry(a_geometry());
    let surface = f.jni.new_object(SURFACE_CLASS).expect("a Java Surface");
    (f, surface)
}

/// `ANativeWindow_fromSurface(NULL, surface)` through the guest.
fn from_surface(f: &Fixture, surface: u64) -> u64 {
    f.value_of("ANativeWindow_fromSurface", |asm| {
        asm.mov(0, 0);
        asm.mov(1, surface);
    })
}

/// Call a one-argument `void` NDK function through the guest and require it to complete.
fn call_with(f: &Fixture, symbol: &str, argument: u64) {
    let entry = f.program_calling(symbol, |asm| {
        asm.mov(0, argument);
    });
    assert!(matches!(f.run(entry).expect("completes"), ExitReason::Returned { .. }));
}

/// **`ANativeWindow_fromSurface` checks the `jobject` is really a `Surface`.**
///
/// The same check `AAssetManager_fromJava` makes and for the same reason: accepting any non-null
/// value turns a wrong argument into a window that answers nonsense thousands of instructions
/// later. §8 row 17 hands this the `Surface` the Java side passed `onSurfaceCreatedNative`, so a
/// value of another class is a host that built the wrong object.
///
/// Asked twice with the same `Surface` it returns the **same** window and takes a second
/// reference, which is what a device does — the `ANativeWindow` is the `Surface`'s native peer.
#[test]
fn from_surface_checks_the_class_and_the_same_surface_is_the_same_window() {
    let _guard = serialized();
    let (f, surface) = with_surface("from-surface");

    let window = from_surface(&f, surface);
    assert_ne!(window, 0, "a real ANativeWindow");
    assert_eq!(f.ndk.live_windows(), 1);
    assert_eq!(
        f.ndk.window_references(window as omni_cpu::GuestAddr),
        Some(1),
        "fromSurface acquires one reference for the caller, which is what the NDK documents"
    );

    let again = from_surface(&f, surface);
    assert_eq!(again, window, "the same Surface gets the same window");
    assert_eq!(f.ndk.live_windows(), 1, "and no second one is made");
    assert_eq!(
        f.ndk.window_references(window as omni_cpu::GuestAddr),
        Some(2),
        "a second fromSurface is a second reference, not a second window: row 17 releases the old \
         window, and a second object here would destroy one the other holder still has"
    );

    // A different Surface is a different window.
    let other_surface = f.jni.new_object(SURFACE_CLASS).expect("a second Java Surface");
    let other = from_surface(&f, other_surface);
    assert_ne!(other, window, "a different Surface is a different window");
    assert_eq!(f.ndk.live_windows(), 2);

    // A `jobject` of another class.
    let wrong = f.jni.new_object(ASSET_MANAGER_CLASS).expect("a Java AssetManager");
    let error = f.refusal_of("ANativeWindow_fromSurface", |asm| {
        asm.mov(0, 0);
        asm.mov(1, wrong);
    });
    assert_eq!(error.symbol(), Some("ANativeWindow_fromSurface"));
    assert!(error.to_string().contains("AssetManager"), "the refusal names the class: {error}");

    // A handle nobody issued, and a null.
    let error = f.refusal_of("ANativeWindow_fromSurface", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0xdead_beef);
    });
    assert!(error.to_string().contains("live jobject"), "{error}");
    let error = f.refusal_of("ANativeWindow_fromSurface", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
    });
    assert!(error.to_string().contains("null"), "{error}");
}

/// **The reference count is a count**, and the last release destroys the window.
///
/// `live_windows()` alone cannot tell a window held twice from one held once — that is
/// `VERIFICATION.md` entry 1's substitution, so the count itself is asserted at every step.
/// Clamping at zero is the believable wrong answer: it keeps a window alive that guest code
/// believes it has destroyed, and §8 row 17 — which releases the old window before asking for a
/// new one — would then hold a handle to a surface nobody owns.
#[test]
fn window_acquire_and_release_count_and_the_last_release_destroys_it() {
    let _guard = serialized();
    let (f, surface) = with_surface("window-refcount");
    let window = from_surface(&f, surface);
    let at = window as omni_cpu::GuestAddr;
    assert_eq!(f.ndk.window_references(at), Some(1));

    // fromSurface made one; the guest takes two more.
    for expected in [2i64, 3] {
        call_with(&f, "ANativeWindow_acquire", window);
        assert_eq!(f.ndk.window_references(at), Some(expected));
    }
    assert_eq!(f.ndk.live_windows(), 1);

    // Three releases take it to zero, and the third destroys it.
    for (expected_live, expected_references) in [(1usize, Some(2i64)), (1, Some(1)), (0, None)] {
        call_with(&f, "ANativeWindow_release", window);
        assert_eq!(f.ndk.live_windows(), expected_live);
        assert_eq!(f.ndk.window_references(at), expected_references);
    }

    // A fourth release names a window that is no longer live. The refusal comes from the identity
    // check, not from a negative count -- a `references < 0` guard would be unreachable here for
    // `ALooper_release`'s reason (VERIFICATION.md entry 12), and there is none.
    let error = f.refusal_of("ANativeWindow_release", |asm| {
        asm.mov(0, window);
    });
    assert_eq!(error.symbol(), Some("ANativeWindow_release"));
    assert!(error.to_string().contains("not live"), "{error}");

    // And the same Surface now makes a fresh window, because the old one is gone.
    let fresh = from_surface(&f, surface);
    assert_eq!(f.ndk.live_windows(), 1);
    assert_eq!(f.ndk.window_references(fresh as omni_cpu::GuestAddr), Some(1));
}

/// **A window identity is an arena address in its own range, and a forged one is refused.**
///
/// The point of the per-kind range: a handle of one kind passed where another belongs is a
/// refusal rather than a lookup that happens to succeed.
#[test]
fn a_window_is_an_arena_address_and_a_forged_one_is_refused() {
    let _guard = serialized();
    let (f, surface) = with_surface("window-identity");
    let window = from_surface(&f, surface);
    let arena = f.ndk.arena() as u64;
    assert!(
        window >= arena && window < arena + f.ndk.arena_bytes() as u64,
        "{window:#x} is not in the arena at {arena:#x}"
    );

    // Inside the arena but off a slot boundary.
    let error = f.refusal_of("ANativeWindow_acquire", |asm| {
        asm.mov(0, window + 4);
    });
    assert_eq!(error.symbol(), Some("ANativeWindow_acquire"));
    assert!(error.to_string().contains("ANativeWindow"), "{error}");

    // A pointer nowhere near it, named in the refusal.
    let error = f.refusal_of("ANativeWindow_getWidth", |asm| {
        asm.mov(0, 0xdead_beef);
    });
    assert!(error.to_string().contains("dead"), "the refusal must name the pointer: {error}");

    // A looper where a window belongs, and a window where a looper belongs. Both directions,
    // because a one-directional check passes against a layer that shares one table.
    let looper = f.prepare();
    assert_ne!(looper, window);
    let error = f.refusal_of("ANativeWindow_getHeight", |asm| {
        asm.mov(0, looper);
    });
    assert!(error.to_string().contains("ANativeWindow"), "{error}");
    let error = f.refusal_of("ALooper_acquire", |asm| {
        asm.mov(0, window);
    });
    assert!(error.to_string().contains("ALooper"), "{error}");
}

/// **An instance whose window geometry nobody decided refuses by name and says which call
/// decides.**
///
/// This is the decision `ndk::window` documents at length: there is no real surface yet, so the
/// dimensions are the embedding's. Returning `android/native_window.h`'s documented
/// negative-on-error would report a device failure that did not happen; returning 1920x1080 would
/// be a device profile nobody chose, indistinguishable in every log from one the host meant.
///
/// The window itself is made **without** a geometry, deliberately: nothing on §8 row 17's path
/// (`fromSurface` -> `callbacks[7] onNativeWindowCreated` -> `APP_CMD_INIT_WINDOW`) asks for a
/// dimension, so refusing there would refuse a call that needs nothing this layer lacks.
#[test]
fn an_undecided_window_geometry_refuses_and_names_the_setter() {
    let _guard = serialized();
    let f = fixture("undecided-window");
    assert_eq!(f.ndk.window_geometry(), None);
    let surface = f.jni.new_object(SURFACE_CLASS).expect("a Java Surface");

    // The window is made, acquired and released without a geometry: those calls need none.
    let window = from_surface(&f, surface);
    assert_ne!(window, 0, "fromSurface does not need the geometry");
    call_with(&f, "ANativeWindow_acquire", window);
    call_with(&f, "ANativeWindow_release", window);
    assert_eq!(f.ndk.live_windows(), 1);

    for symbol in ["ANativeWindow_getWidth", "ANativeWindow_getHeight"] {
        let error = f.refusal_of(symbol, |asm| {
            asm.mov(0, window);
        });
        assert_eq!(error.symbol(), Some(symbol));
        let text = error.to_string();
        assert!(text.contains("Ndk::set_window_geometry"), "{error}");
        assert!(text.contains("1920x1080"), "the refusal names the wrong answer it refused: {error}");
    }

    // And once the host decides, the same window answers -- through the same handle, with no new
    // call to fromSurface, which is the point of reading the geometry at call time.
    f.ndk.set_window_geometry(a_geometry());
    assert_eq!(
        f.value_of("ANativeWindow_getWidth", |asm| {
            asm.mov(0, window);
        }) as i64,
        1440
    );
    assert_eq!(
        f.value_of("ANativeWindow_getHeight", |asm| {
            asm.mov(0, window);
        }) as i64,
        3120
    );
}

/// **A resize reaches a window the guest already holds**, which is where this deliberately
/// differs from `AConfiguration`.
///
/// `AConfiguration_fromAssetManager` snapshots, because a configuration the guest holds does not
/// change under it on a device -- that is why `onConfigurationChanged` exists.
/// `ANativeWindow_getWidth` is the opposite: it queries the live surface, and §8 row 18's
/// `onSurfaceChangedNative` may call `callbacks[8] onNativeWindowResized` without the window
/// handle changing. A snapshot here would keep answering the old size, which shows up only as a
/// viewport stale by one event.
///
/// The two dimensions are asserted **separately and with different values**, because a handler
/// that returned the width for both would pass a test that used a square.
#[test]
fn a_resize_reaches_a_window_the_guest_already_holds() {
    let _guard = serialized();
    let (f, surface) = with_surface("window-resize");
    let window = from_surface(&f, surface);
    assert_eq!(
        f.value_of("ANativeWindow_getWidth", |asm| {
            asm.mov(0, window);
        }) as i64,
        1440
    );
    assert_eq!(
        f.value_of("ANativeWindow_getHeight", |asm| {
            asm.mov(0, window);
        }) as i64,
        3120,
        "the height is not the width"
    );

    // The host rotates the device. The same handle, no new fromSurface.
    f.ndk.set_window_geometry(WindowGeometry::new(3120, 1440).expect("the rotated geometry"));
    assert_eq!(
        f.value_of("ANativeWindow_getWidth", |asm| {
            asm.mov(0, window);
        }) as i64,
        3120,
        "getWidth queries the live surface, as AOSP's query(NATIVE_WINDOW_WIDTH) does"
    );
    assert_eq!(
        f.value_of("ANativeWindow_getHeight", |asm| {
            asm.mov(0, window);
        }) as i64,
        1440
    );
}

/// The window ceiling answers **null**, which is how a device reports failing to produce one.
///
/// A refusal would be wrong: §8 row 17's caller branches on null, and a cap this layer chose is
/// not a reason to invent a failure mode the caller has no arm for. `MAX_NATIVE_WINDOWS` distinct
/// `Surface`s fill it, because the same `Surface` twice is the same window.
#[test]
fn the_window_ceiling_answers_null() {
    let _guard = serialized();
    let f = fixture("window-ceiling");
    f.ndk.set_window_geometry(a_geometry());
    let mut windows = Vec::new();
    for _ in 0..MAX_NATIVE_WINDOWS {
        let surface = f.jni.new_object(SURFACE_CLASS).expect("a Java Surface");
        let window = from_surface(&f, surface);
        assert_ne!(window, 0);
        windows.push(window);
    }
    assert_eq!(f.ndk.live_windows(), MAX_NATIVE_WINDOWS);
    // Membership, not a total: every one is a distinct address.
    let mut sorted = windows.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), MAX_NATIVE_WINDOWS, "each window is its own slot");

    let surface = f.jni.new_object(SURFACE_CLASS).expect("one Surface too many");
    assert_eq!(from_surface(&f, surface), 0, "the ceiling is a null, not a refusal");
    assert_eq!(f.ndk.live_windows(), MAX_NATIVE_WINDOWS, "and nothing was evicted");

    // Freeing one lets the next in.
    call_with(&f, "ANativeWindow_release", windows[0]);
    assert_eq!(f.ndk.live_windows(), MAX_NATIVE_WINDOWS - 1);
    assert_ne!(from_surface(&f, surface), 0, "the freed slot is reused");
}

/// **The census counts each `ANativeWindow` symbol by name**, so a gate can assert which calls
/// the engine actually made rather than that the run completed.
#[test]
fn every_window_symbol_is_counted_by_name() {
    let _guard = serialized();
    let (f, surface) = with_surface("window-census");
    let window = from_surface(&f, surface);
    call_with(&f, "ANativeWindow_acquire", window);
    let _ = f.value_of("ANativeWindow_getWidth", |asm| {
        asm.mov(0, window);
    });
    let _ = f.value_of("ANativeWindow_getHeight", |asm| {
        asm.mov(0, window);
    });
    call_with(&f, "ANativeWindow_release", window);

    let census = f.ndk.census();
    for symbol in [
        "ANativeWindow_fromSurface",
        "ANativeWindow_acquire",
        "ANativeWindow_release",
        "ANativeWindow_getWidth",
        "ANativeWindow_getHeight",
    ] {
        assert_eq!(census.get(symbol).copied(), Some(1), "`{symbol}` was not counted once");
    }
    // A refused call is still a call that happened, and is still counted: a census that only
    // counted successes would make the refusal invisible in exactly the run where it mattered.
    let _ = f.refusal_of("ANativeWindow_getWidth", |asm| {
        asm.mov(0, 0xdead_beef);
    });
    assert_eq!(f.ndk.census().get("ANativeWindow_getWidth").copied(), Some(2));
}
