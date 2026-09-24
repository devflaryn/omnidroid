//! **The main thread, handed to AppKit before `main` runs.**
//!
//! # The problem
//!
//! AppKit is main-thread-only: `-[NSWindow initWithContentRect:…]` off the main thread throws,
//! and the event loop that delivers a window's input must run there. This seam's callers do not
//! run there. libtest runs every test on a thread of its own -- MEASURED on this host,
//! `pthread_main_np()` is 0 inside a test even with `--test-threads=1` -- and the gate
//! (`omni-android/tests/gameactivity.rs`) creates its window on its test thread and polls it
//! there. The seam's contract (`Window` is `!Send`, created and polled on one thread) is
//! Win32's, and it has to keep holding.
//!
//! # The decision
//!
//! A constructor in this module runs before `main`, from the executable's `__mod_init_func`.
//! It starts a secondary pthread -- with the main thread's stack size, `getrlimit(RLIMIT_STACK)`,
//! at least 8 MiB -- that calls the program's real `main(argc, argv, envp, apple)` and then
//! `exit`s with what it returned, and the original main thread becomes the **AppKit thread**: it
//! runs the main run loop for ever, serving work sent from other threads through libdispatch's
//! main queue (`dispatch_sync_f`) and, once the first window exists, pumping `NSApplication`'s
//! events. A [`super::Window`] proxies every AppKit call to that thread synchronously and drains
//! the events it produces from a thread-safe queue.
//!
//! **Alternatives rejected, and why:**
//!
//! * *Require callers to create windows on the main thread.* The gate and every live test run on
//!   libtest threads; there is no main thread to give them, and the seam's `!Send` rule would
//!   stop them handing a window over.
//! * *`harness = false` test binaries with their own `main`.* Fixes the tests this crate owns and
//!   none of the others -- `omni-gfx`'s and `omni-android`'s live tests would each need it -- and
//!   leaves the production embedding with the same problem.
//! * *A seam change to a main-thread-affine window type.* The right long-term shape if an
//!   embedding ever owns its main thread (an `.app`), and not needed for any caller today.
//!
//! # When it does not happen, and what the window seam then says
//!
//! The constructor declines -- returns normally, so `main` runs on the main thread as it always
//! would -- when any of the following is true, and [`status`] then names the reason, which
//! `Window::new` turns into [`WindowError::MainThreadUnavailable`](crate::window::WindowError).
//! It never hangs and never creates a window off the main thread.
//!
//! * It is not running on the main thread (`pthread_main_np`).
//! * This code is not in the main executable (`dladdr` of the constructor against
//!   `_NSGetMachExecuteHeader`): a dylib's constructor runs before the executable's own
//!   initializers, which it would then pre-empt.
//! * The executable is inside an application bundle (`….app/Contents/MacOS/`): an application
//!   runs its own `NSApplication` on its main thread, and taking that away from it would break it.
//! * It cannot find itself in the executable's initializer list, or the list is in a form this
//!   code has never read (more than one `__mod_init_func`, or ld's `__TEXT,__init_offsets`).
//!   **Entries after it are run by it**, in order, with dyld's own arguments, before `main` is
//!   released -- the constructor never returns, so dyld would never run them. MEASURED
//!   (`otool -s __DATA_CONST __mod_init_func`): the list is `__DATA_CONST,__mod_init_func`, and the
//!   linker places a test crate's own initializer **after** this one --
//!   `tests/window_macos.rs::another_initializer_runs_once_on_the_main_thread_before_main` is that
//!   case, and asserts it ran exactly once, on the main thread. dynarmic's C++ static initializers
//!   (three in its own test binaries) will land on one side or the other the same way.
//! * `main` cannot be found: the entry point comes from `LC_MAIN`'s `entryoff` (MEASURED equal to
//!   `dlsym(RTLD_MAIN_ONLY, "main")` in debug and in thin-LTO release test binaries), and when
//!   `dlsym` also answers, the two must agree.
//! * `pthread_create` fails.
//!
//! # What changes for a program it happens to
//!
//! `main` runs on a pthread instead of thread 0. MEASURED with a prototype, debug and thin-LTO
//! release: exit codes propagate (a panicking `main` exits 101, a failing libtest binary exits
//! 101, `std::process::exit(7)` exits 7), panics print as before, std still names the thread
//! `main`, and a stack overflow is still caught and reported by std's guard page. What does not
//! happen any more: dyld's own "main is about to be called" notification to a debugger, since
//! dyld never gets to call `main`.

use core::ffi::{c_char, c_int, c_void, CStr};
use core::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

use objc2::MainThreadMarker;

/// The constructor, as the executable's initializer list holds it: dyld calls it with
/// `(argc, argv, envp, apple, program_vars)`.
type Initializer =
    extern "C" fn(c_int, *const *const c_char, *const *const c_char, *const *const c_char, *const c_void);

/// **The constructor's entry in `__mod_init_func`.** `#[used]` keeps it through optimisation and
/// LTO; the section's type (`S_MOD_INIT_FUNC_POINTERS`) is what makes dyld call it and what makes
/// the linker keep it once the object is linked. [`status`] reads this static, so any binary that
/// can create a window links the object that holds it.
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static CONSTRUCTOR: Initializer = constructor;

/// Where the constructor got to. See [`Status`].
static STATE: AtomicU8 = AtomicU8::new(Status::NotRun as u8);

/// What the constructor did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Status {
    /// The constructor has not run.
    NotRun = 0,
    /// The main thread is serving AppKit, and `main` runs on a pthread.
    Serving = 1,
    /// Not on the main thread.
    NotMainThread = 2,
    /// Not linked into the main executable.
    NotInExecutable = 3,
    /// The executable is in an application bundle.
    AppBundle = 4,
    /// Not found in the initializer list, or the list is in a format not read here.
    NotLastInitializer = 5,
    /// `main` could not be found, or `LC_MAIN` and `dlsym` disagree.
    NoMain = 6,
    /// `pthread_create` failed.
    ThreadFailed = 7,
}

impl Status {
    fn from_u8(value: u8) -> Status {
        match value {
            1 => Status::Serving,
            2 => Status::NotMainThread,
            3 => Status::NotInExecutable,
            4 => Status::AppBundle,
            5 => Status::NotLastInitializer,
            6 => Status::NoMain,
            7 => Status::ThreadFailed,
            _ => Status::NotRun,
        }
    }

    /// Why the main thread is not available, for the window seam's refusal.
    pub(crate) const fn why(self) -> &'static str {
        match self {
            Status::NotRun => {
                "omni-platform's macOS constructor never ran, so nothing hands the main thread to \
                 AppKit (it runs from the executable's __mod_init_func; a binary that strips or \
                 never links it has no AppKit thread)"
            }
            Status::Serving => "the main thread is serving AppKit",
            Status::NotMainThread => {
                "omni-platform's macOS constructor ran on a thread that is not the process's main \
                 thread, so it could not hand the main thread to AppKit"
            }
            Status::NotInExecutable => {
                "omni-platform is not linked into the main executable (a dylib or a plugin), and a \
                 library's constructor cannot take the main thread without pre-empting the \
                 executable's own initializers"
            }
            Status::AppBundle => {
                "the executable is inside an application bundle (.app/Contents/MacOS/), which runs \
                 its own NSApplication on the main thread; omni-platform does not take it away"
            }
            Status::NotLastInitializer => {
                "omni-platform's macOS constructor could not find itself in the executable's \
                 __mod_init_func (or the list is in a section it does not read), so it could not \
                 run the initializers after it and did not take the main thread"
            }
            Status::NoMain => {
                "the executable's entry point could not be found (no LC_MAIN, or LC_MAIN and \
                 dlsym(\"main\") disagree), so main could not be moved off the main thread"
            }
            Status::ThreadFailed => {
                "pthread_create failed for the thread main was to move to, so main kept the main \
                 thread and AppKit has none"
            }
        }
    }
}

/// What the constructor did. [`Status::Serving`] is permanent once reached.
pub(crate) fn status() -> Status {
    // Reading `CONSTRUCTOR` here is what ties the constructor's object file to every caller of
    // the window seam; see its documentation.
    let constructor = core::hint::black_box(CONSTRUCTOR);
    debug_assert!(constructor as usize != 0);
    Status::from_u8(STATE.load(Ordering::Acquire))
}

// ---------------------------------------------------------------------------------- FFI

/// `dispatch_queue_s`, opaque.
#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}

#[allow(non_upper_case_globals)]
unsafe extern "C" {
    /// `_NSGetMachExecuteHeader(3)`: the main executable's Mach-O header, `crt_externs.h`.
    fn _NSGetMachExecuteHeader() -> *const MachHeader64;
    /// `getsectiondata(3)`, `mach-o/getsect.h`.
    fn getsectiondata(
        header: *const MachHeader64,
        segment: *const c_char,
        section: *const c_char,
        size: *mut libc::c_ulong,
    ) -> *mut u8;
    /// `_NSGetExecutablePath(3)`, `mach-o/dyld.h`.
    fn _NSGetExecutablePath(buffer: *mut c_char, size: *mut u32) -> c_int;
    /// The main dispatch queue: `dispatch_get_main_queue()` is `&_dispatch_main_q`.
    static _dispatch_main_q: DispatchQueue;
    /// `dispatch_sync_f(3)`.
    fn dispatch_sync_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

#[link(name = "CoreFoundation", kind = "framework")]
#[allow(non_upper_case_globals)]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    static kCFRunLoopCommonModes: *const c_void;
    fn CFRunLoopGetMain() -> *mut c_void;
    fn CFRunLoopStop(run_loop: *mut c_void);
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source_handled: u8) -> i32;
    fn CFAbsoluteTimeGetCurrent() -> f64;
    fn CFRunLoopTimerCreate(
        allocator: *const c_void,
        fire_date: f64,
        interval: f64,
        flags: u64,
        order: isize,
        callout: extern "C" fn(*mut c_void, *mut c_void),
        context: *mut c_void,
    ) -> *mut c_void;
    fn CFRunLoopAddTimer(run_loop: *mut c_void, timer: *mut c_void, mode: *const c_void);
}

/// `struct mach_header_64`, `mach-o/loader.h`. Declared here because `libc`'s copy is deprecated
/// in favour of a crate this one does not otherwise need.
#[repr(C)]
struct MachHeader64 {
    magic: u32,
    cputype: i32,
    cpusubtype: i32,
    filetype: u32,
    ncmds: u32,
    sizeofcmds: u32,
    flags: u32,
    reserved: u32,
}

/// `struct load_command`, `mach-o/loader.h`.
#[repr(C)]
struct LoadCommand {
    cmd: u32,
    cmdsize: u32,
}

/// `LC_MAIN`, `mach-o/loader.h`.
const LC_MAIN: u32 = 0x8000_0028;

// ------------------------------------------------------------------------- the constructor

/// See this module's header. Declines by recording a reason and returning; takes the main thread
/// by never returning.
extern "C" fn constructor(
    argc: c_int,
    argv: *const *const c_char,
    envp: *const *const c_char,
    apple: *const *const c_char,
    vars: *const c_void,
) {
    // Nothing here may unwind into dyld.
    let decided = catch_unwind(|| decide(constructor as *const () as usize));
    let (main, later) = match decided {
        Ok(Ok(found)) => found,
        Ok(Err(status)) => {
            STATE.store(status as u8, Ordering::Release);
            return;
        }
        Err(_) => {
            STATE.store(Status::NoMain as u8, Ordering::Release);
            return;
        }
    };
    let entry = Box::into_raw(Box::new(MainCall { argc, argv, envp, apple, main }));
    // SAFETY: plain pthread calls on locals; `entry` is handed to the new thread, which owns it,
    // or reclaimed here if the thread was not created.
    unsafe {
        let mut attr: libc::pthread_attr_t = core::mem::zeroed();
        libc::pthread_attr_init(&raw mut attr);
        libc::pthread_attr_setstacksize(&raw mut attr, main_stack_size());
        let mut thread: libc::pthread_t = core::mem::zeroed();
        let created = libc::pthread_create(&raw mut thread, &raw const attr, run_main, entry.cast());
        libc::pthread_attr_destroy(&raw mut attr);
        if created != 0 {
            drop(Box::from_raw(entry));
            STATE.store(Status::ThreadFailed as u8, Ordering::Release);
            return;
        }
        libc::pthread_detach(thread);
    }
    // **The initializers after this one, run here, in order, with dyld's own arguments** -- what
    // dyld would have done had this returned, and what it will now never do. Only once the thread
    // exists: a failure to create it returns to dyld, which must then find them not yet run.
    for initializer in later {
        // SAFETY: an entry of the executable's own `__mod_init_func`, called with the arguments
        // dyld passes every entry of it.
        let initializer: Initializer = unsafe { core::mem::transmute::<usize, Initializer>(initializer) };
        initializer(argc, argv, envp, apple, vars);
    }
    let (open, opened) = &MAIN_GATE;
    *open.lock().unwrap_or_else(PoisonError::into_inner) = true;
    opened.notify_all();
    serve();
}

/// Held shut until every initializer has run: `main` must not start before them.
static MAIN_GATE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// The arguments `main` will be called with.
struct MainCall {
    argc: c_int,
    argv: *const *const c_char,
    envp: *const *const c_char,
    apple: *const *const c_char,
    main: usize,
}

/// The secondary thread: the program's `main`, then `exit` with what it returned -- which is
/// exactly what dyld's `start` does after `main` returns.
extern "C" fn run_main(entry: *mut c_void) -> *mut c_void {
    // SAFETY: `entry` came from `Box::into_raw` in `constructor` and is owned by this thread.
    let call = unsafe { Box::from_raw(entry.cast::<MainCall>()) };
    let (open, opened) = &MAIN_GATE;
    let mut is_open = open.lock().unwrap_or_else(PoisonError::into_inner);
    while !*is_open {
        is_open = opened.wait(is_open).unwrap_or_else(PoisonError::into_inner);
    }
    drop(is_open);
    // SAFETY: `call.main` is the executable's entry point, found by `decide` from `LC_MAIN` (and
    // cross-checked against `dlsym` when that answers), and `main`'s C signature is
    // `int main(int, char **, char **, char **)`.
    let main: extern "C" fn(c_int, *const *const c_char, *const *const c_char, *const *const c_char) -> c_int =
        unsafe { core::mem::transmute::<usize, _>(call.main) };
    let code = main(call.argc, call.argv, call.envp, call.apple);
    // SAFETY: `exit(3)`, from any thread, is how a C program ends with a status.
    unsafe { libc::exit(code) }
}

/// The main thread's stack size: `RLIMIT_STACK`'s soft limit, which is what the kernel gave
/// thread 0, and at least 8 MiB (the macOS default) when it is unlimited or unreadable.
fn main_stack_size() -> usize {
    const FLOOR: usize = 8 << 20;
    // SAFETY: writes a `rlimit`.
    let mut limit: libc::rlimit = unsafe { core::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::getrlimit(libc::RLIMIT_STACK, &raw mut limit) } != 0 || limit.rlim_cur == libc::RLIM_INFINITY {
        return FLOOR;
    }
    usize::try_from(limit.rlim_cur).unwrap_or(FLOOR).max(FLOOR)
}

/// Every check in this module's header, in order: `Ok((main, the initializers after this one))` to
/// take the main thread.
fn decide(this: usize) -> Result<(usize, Vec<usize>), Status> {
    // SAFETY: no arguments.
    if unsafe { libc::pthread_main_np() } != 1 {
        return Err(Status::NotMainThread);
    }
    // SAFETY: no arguments; the header of the main executable, live for the process.
    let header = unsafe { _NSGetMachExecuteHeader() };
    let mut info: libc::Dl_info = unsafe { core::mem::zeroed() };
    // SAFETY: `dladdr` reads the address and writes `info`.
    if unsafe { libc::dladdr(this as *const c_void, &raw mut info) } == 0 || info.dli_fbase as usize != header as usize {
        return Err(Status::NotInExecutable);
    }
    if in_app_bundle() {
        return Err(Status::AppBundle);
    }
    let later = initializers_after(header, this).ok_or(Status::NotLastInitializer)?;
    let main = entry_point(header).ok_or(Status::NoMain)?;
    // SAFETY: `dlsym` with a C-string literal.
    let exported = unsafe { libc::dlsym(libc::RTLD_MAIN_ONLY, c"main".as_ptr()) } as usize;
    if exported != 0 && exported != main {
        return Err(Status::NoMain);
    }
    Ok((main, later))
}

/// True when the executable's path is `….app/Contents/MacOS/…`.
fn in_app_bundle() -> bool {
    let mut buffer = vec![0 as c_char; 4096];
    let mut size = buffer.len() as u32;
    // SAFETY: writes at most `size` bytes into `buffer`.
    if unsafe { _NSGetExecutablePath(buffer.as_mut_ptr(), &raw mut size) } != 0 {
        return false;
    }
    // SAFETY: NUL-terminated on success.
    let path = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    path.to_bytes().windows(b".app/Contents/MacOS/".len()).any(|window| window == b".app/Contents/MacOS/")
}

/// The entries after `this` in the executable's one `__mod_init_func` section, or `None` when
/// `this` is not in it or the list is in a form this code does not read. See this module's header.
fn initializers_after(header: *const MachHeader64, this: usize) -> Option<Vec<usize>> {
    let read = |segment: &CStr, section: &CStr| {
        let mut size: libc::c_ulong = 0;
        // SAFETY: `getsectiondata` reads the live header and writes `size`.
        let data = unsafe { getsectiondata(header, segment.as_ptr(), section.as_ptr(), &raw mut size) };
        (!data.is_null() && size > 0).then_some((data, size as usize))
    };
    let pointer_lists: Vec<(*mut u8, usize)> = [c"__DATA_CONST", c"__DATA"]
        .into_iter()
        .filter_map(|segment| read(segment, c"__mod_init_func"))
        .collect();
    if pointer_lists.len() != 1 || read(c"__TEXT", c"__init_offsets").is_some() {
        return None;
    }
    let (data, size) = pointer_lists[0];
    // SAFETY: the section holds `size / 8` rebased pointers, live for the process.
    let entries = unsafe { core::slice::from_raw_parts(data.cast::<usize>(), size / core::mem::size_of::<usize>()) };
    let at = entries.iter().position(|&entry| entry == this)?;
    Some(entries[at + 1..].to_vec())
}

/// `LC_MAIN`'s `entryoff`, which is relative to the executable's `__TEXT` (the header).
fn entry_point(header: *const MachHeader64) -> Option<usize> {
    // SAFETY: walks the load commands of the live main-executable header, each of which says its
    // own size; `ncmds` bounds the walk.
    unsafe {
        let mut command = header.cast::<u8>().add(core::mem::size_of::<MachHeader64>());
        for _ in 0..(*header).ncmds {
            let load = &*command.cast::<LoadCommand>();
            if load.cmd == LC_MAIN {
                let entryoff = command.add(8).cast::<u64>().read_unaligned();
                return Some(header as usize + usize::try_from(entryoff).ok()?);
            }
            command = command.add(load.cmdsize as usize);
        }
    }
    None
}

// --------------------------------------------------------------------------- the AppKit thread

/// A timer that never fires in practice, so that the main run loop has a source in every mode
/// and `CFRunLoopRunInMode` blocks rather than returning `kCFRunLoopRunFinished` at once.
extern "C" fn keep_alive(_timer: *mut c_void, _info: *mut c_void) {}

/// The main thread's loop, for ever.
///
/// Before the first window: the plain run loop, which serves the main dispatch queue. After it
/// ([`super::appkit::start_application`] stops the run loop once to switch): `NSApplication`'s
/// event pump, which serves the main queue too, from inside `nextEventMatchingMask:`.
fn serve() -> ! {
    // SAFETY: CoreFoundation calls on the main thread, with the constant modes CF defines.
    unsafe {
        let timer = CFRunLoopTimerCreate(
            core::ptr::null(),
            CFAbsoluteTimeGetCurrent() + 1.0e9,
            1.0e9,
            0,
            0,
            keep_alive,
            core::ptr::null_mut(),
        );
        CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopCommonModes);
    }
    STATE.store(Status::Serving as u8, Ordering::Release);
    // SAFETY: this is the main thread; `decide` checked `pthread_main_np`.
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    loop {
        objc2::rc::autoreleasepool(|_| {
            if super::appkit::started() {
                super::appkit::pump_one(mtm);
            } else {
                // SAFETY: a CF call on the main thread with CF's own mode constant.
                unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 1.0e10, 0) };
            }
        });
    }
}

/// Make the plain run loop return, so that [`serve`] switches to the event pump.
pub(super) fn stop_run_loop() {
    // SAFETY: `CFRunLoopStop` is thread-safe and the main run loop exists.
    unsafe { CFRunLoopStop(CFRunLoopGetMain()) };
}

/// **Run `work` on the AppKit thread and return what it returned**, synchronously.
///
/// Inline when already there (a `dispatch_sync` to the main queue from the main thread
/// deadlocks). A panic inside `work` is caught on the main thread -- it must not unwind through
/// libdispatch -- and resumed here, on the caller's thread.
///
/// The caller must have seen [`status`] answer [`Status::Serving`]: with no AppKit thread the main
/// queue is never drained and this would wait for ever. `super::Window::create` and the web view
/// seam's `open` are the gates, and every other entry point needs what they made.
pub(crate) fn on_main<R, F: FnOnce(MainThreadMarker) -> R>(work: F) -> R {
    if let Some(mtm) = MainThreadMarker::new() {
        return work(mtm);
    }
    debug_assert_eq!(status(), Status::Serving, "on_main without an AppKit thread would never return");

    struct Job<F, R> {
        work: Option<F>,
        out: Option<std::thread::Result<R>>,
    }
    extern "C" fn trampoline<R, F: FnOnce(MainThreadMarker) -> R>(context: *mut c_void) {
        // SAFETY: `context` is the `Job` below, alive until `dispatch_sync_f` returns.
        let job = unsafe { &mut *context.cast::<Job<F, R>>() };
        // SAFETY: the main queue runs its work on the main thread.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        if let Some(work) = job.work.take() {
            job.out = Some(catch_unwind(AssertUnwindSafe(|| {
                objc2::rc::autoreleasepool(|_| work(mtm))
            })));
        }
    }
    let mut job = Job { work: Some(work), out: None };
    // SAFETY: the main queue is a static; `job` outlives the call because `dispatch_sync_f` does
    // not return until the work has run.
    unsafe {
        dispatch_sync_f(&raw const _dispatch_main_q, (&raw mut job).cast(), trampoline::<R, F>);
    }
    // `dispatch_sync_f` returns only after `trampoline` ran, and `trampoline` always fills `out`
    // (the `work` it takes was put there one line above the call and nothing else takes it).
    match job.out.expect("dispatch_sync_f returns after its work has run, and the work fills `out`") {
        Ok(value) => value,
        Err(panic) => resume_unwind(panic),
    }
}
