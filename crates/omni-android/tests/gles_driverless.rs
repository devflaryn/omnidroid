//! **EGL and GLES with no driver, from translated ARM64**: `Gles::set_driverless`, the answers the
//! headless gate gives the engine when it falls back to OpenGL ES with no window it can draw into.
//!
//! No window and no host EGL: every call is a guest `BLR` into a thunk this boundary bound, and what
//! comes back is what AOSP's `libEGL` answers with no driver (`omni_android::gles::driverless` has
//! each answer's source). The engine's own sequence, MEASURED in the gate (2.739.691): `eglGetDisplay`
//! then `eglGetError`, then `Mode 4 failed: Error creating context: eglGetDisplay 300c`.

mod harness;

use std::cell::Cell;
use std::sync::Arc;

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::gles::driverless::{EGL_BAD_CONTEXT, EGL_BAD_DISPLAY, EGL_BAD_PARAMETER, EGL_SUCCESS};
use omni_android::gles::{Gles, ProcAnswer};
use omni_android::{AbiError, Boundary};
use omni_cpu::{ExitReason, GuestAddr};

const EGL_EXTENSIONS: u64 = 0x3055;
const EGL_VENDOR: u64 = 0x3053;
const GL_COLOR_BUFFER_BIT: u64 = 0x4000;
const GL_RENDERER: u64 = 0x1F01;

struct Fixture {
    guest: Guest,
    gles: Arc<Gles>,
    boundary: Arc<Boundary>,
    next: Cell<usize>,
}

impl Fixture {
    fn new() -> Self {
        let guest = Guest::new();
        let gles = Gles::new(Arc::clone(&guest.space));
        let builder = guest.boundary(omni_android::gles::bound_symbol_count());
        gles.bind_into(&builder).expect("bind EGL and GLES");
        gles.set_driverless();
        let boundary = builder.finish();
        Self { guest, gles, boundary, next: Cell::new(0x800) }
    }

    fn cstr(&self, text: &str) -> u64 {
        let at = self.next.get();
        self.next.set((at + text.len() + 16) & !15);
        let at = self.guest.data + at;
        self.guest.write_bytes(at, &text.bytes().chain(std::iter::once(0)).collect::<Vec<_>>());
        at as u64
    }

    /// Call `symbol` from guest code with `ints` in `x0`-`x7`, on this thread. Returns `x0`.
    fn call(&self, symbol: &str, ints: &[u64]) -> Result<u64, AbiError> {
        assert!(ints.len() <= 8);
        let target = self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address;
        let entry = self.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(9, target as u64);
        for (index, value) in ints.iter().enumerate() {
            asm.mov(index as u32, *value);
        }
        asm.push(blr(9));
        asm.mov(22, self.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        self.guest.load(asm.words());
        let _gles = self.gles.activate();
        let mut cpu = self.guest.thread(&self.boundary);
        let exit = self.boundary.run(&mut cpu, entry, BUDGET)?;
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        Ok(self.guest.read_u64(self.guest.data as GuestAddr))
    }

    fn gl(&self, symbol: &str, ints: &[u64]) -> u64 {
        self.call(symbol, ints).unwrap_or_else(|e| panic!("{symbol}: {e}"))
    }

    fn error(&self) -> i32 {
        self.gl("eglGetError", &[]) as u32 as i32
    }
}

/// The engine's sequence, then the rest of an EGL bring-up as a renderer that ignored the failure
/// would make it: every call answers, none refuses, and none succeeds.
#[test]
fn with_no_driver_the_display_is_egl_no_display_and_nothing_after_it_succeeds() {
    let _serial = serialized();
    let f = Fixture::new();
    assert!(f.gles.is_driverless());

    // The engine's two calls (MEASURED): the display, then the error it prints as `300c`.
    assert_eq!(f.gl("eglGetDisplay", &[0]), 0, "EGL_NO_DISPLAY");
    assert_eq!(f.error(), EGL_BAD_PARAMETER);
    assert_eq!(f.error(), EGL_SUCCESS, "eglGetError clears what it read");

    // Everything that takes the display it did not get.
    assert_eq!(f.gl("eglInitialize", &[0, 0, 0]) as u32, 0);
    assert_eq!(f.error(), EGL_BAD_DISPLAY);
    assert_eq!(f.gl("eglChooseConfig", &[0, 0, 0, 0, 0]) as u32, 0);
    assert_eq!(f.gl("eglCreateContext", &[0, 0, 0, 0]), 0, "EGL_NO_CONTEXT");
    assert_eq!(f.error(), EGL_BAD_DISPLAY);
    assert_eq!(f.gl("eglQueryString", &[0, EGL_VENDOR]), 0);
    assert_eq!(f.gl("eglTerminate", &[0]) as u32, 0);
    assert_eq!(f.error(), EGL_BAD_DISPLAY);

    // The calls with no display argument.
    assert_eq!(f.gl("eglGetProcAddress", &[f.cstr("glClear")]), 0);
    assert_eq!(f.error(), EGL_BAD_PARAMETER);
    assert_eq!(f.gl("eglBindAPI", &[0x30A0]) as u32, 0);
    assert_eq!(f.gl("eglGetCurrentContext", &[]), 0);
    assert_eq!(f.error(), EGL_SUCCESS);
    assert_eq!(f.gl("eglWaitGL", &[]) as u32, 0);
    assert_eq!(f.error(), EGL_BAD_CONTEXT);
    assert_eq!(f.gl("eglReleaseThread", &[]) as u32, 1);

    // GL with no context: 0, nothing done, and EGL's error untouched.
    assert_eq!(f.gl("eglGetDisplay", &[0]), 0);
    f.gl("glClear", &[GL_COLOR_BUFFER_BIT]);
    assert_eq!(f.gl("glGetString", &[GL_RENDERER]), 0);
    assert_eq!(f.gl("glGetError", &[]), 0);
    assert_eq!(f.error(), EGL_BAD_PARAMETER, "a GL call does not touch EGL's error");

    // The one call not modelled refuses by name rather than inventing a string.
    let refused = f.call("eglQueryString", &[0, EGL_EXTENSIONS]).expect_err("refused");
    assert!(refused.to_string().contains("client-extension"), "{refused}");

    // The census says what happened.
    let requests = f.gles.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!((requests[0].name.as_str(), requests[0].answer), ("glClear", ProcAnswer::NullNoDriver));
    let report = f.gles.report();
    assert!(report.contains("no driver"), "{report}");
    assert_eq!(f.gles.presents(), 0);
    let counts = f.gles.call_counts();
    assert_eq!(counts.get("eglGetDisplay"), Some(&2));
    assert!(f.gles.substitutions().iter().any(|s| s.call == "eglGetDisplay" && s.what.contains("EGL_BAD_PARAMETER")));
}

/// EGL's error is per thread (EGL 1.5 section 3.1): a failure on one guest thread is not read by
/// another's `eglGetError`.
#[test]
fn the_egl_error_is_each_threads_own() {
    let _serial = serialized();
    let f = Fixture::new();
    assert_eq!(f.gl("eglGetDisplay", &[0]), 0);
    let other = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let g = Fixture::new();
                g.error()
            })
            .join()
            .expect("the other thread")
    });
    assert_eq!(other, EGL_SUCCESS, "another thread saw this thread's error");
    assert_eq!(f.error(), EGL_BAD_PARAMETER);
}

/// A host set after `set_driverless` takes over again: the last writer wins.
#[test]
fn set_host_after_set_driverless_ends_the_driverless_answers() {
    let _serial = serialized();
    let f = Fixture::new();
    #[derive(Debug)]
    struct Refusing;
    impl omni_android::gles::GlesHost for Refusing {
        fn select(&self, _: omni_platform::window::RawWindow) -> omni_android::AbiResult<String> {
            Err(AbiError::Refused { symbol: "select".into(), address: 0, why: "test host".into() })
        }
        fn proc_address(&self, _: &str) -> omni_android::AbiResult<Option<omni_android::gles::HostProc>> {
            Ok(None)
        }
        fn default_display(
            &self,
            _: omni_platform::window::RawWindow,
        ) -> omni_android::AbiResult<omni_android::gles::DisplayOpened> {
            Err(AbiError::Refused { symbol: "default_display".into(), address: 0, why: "test host".into() })
        }
        fn create_window_surface(
            &self,
            _: u64,
            _: u64,
            _: omni_platform::window::RawWindow,
            _: &[i32],
        ) -> omni_android::AbiResult<omni_android::gles::SurfaceMade> {
            Err(AbiError::Refused { symbol: "create_window_surface".into(), address: 0, why: "test host".into() })
        }
        fn destroy_window_surface(&self, _: u64, _: u64) -> omni_android::AbiResult<u32> {
            Ok(0)
        }
        fn display_terminated(&self, _: u64) {}
    }
    f.gles.set_host(Arc::new(Refusing));
    assert!(!f.gles.is_driverless());
    // Forwarded now: with no window source to choose a host EGL for, the call refuses.
    assert!(f.call("eglGetDisplay", &[0]).is_err());
}
