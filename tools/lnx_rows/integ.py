"""lnx-integ-: fixes made while integrating the Linux backends into the shared tree.

Row format: see `build.py`.
"""

PROCENV = "crates/omni-android/src/bionic/procenv.rs"
PROCESS_ERROR = "crates/omni-platform/src/process/error.rs"
LOADER_M1 = "crates/omni-elf/tests/loader_m1.rs"

WINDOW_LINUX = "crates/omni-platform/src/window/linux.rs"
EWMH = ["env", "OMNI_GFX_WINDOW_TESTS=1", "cargo", "test", "-p", "omni-platform", "--release",
        "--no-fail-fast", "--test", "window_linux_ewmh", "--", "--ignored", "--test-threads=1"]

PROCENV_LINUX = ["cargo", "test", "-p", "omni-android", "--release", "--no-fail-fast",
                 "--test", "procenv_linux"]
RELRO = ["cargo", "test", "-p", "omni-elf", "--release", "--no-fail-fast", "--test", "loader_m1",
         "--", "writing_to_sealed_relro_faults"]

ROWS = [
    # FMOD's setpriority(0, 0, -16) on a host without RLIMIT_NICE: the kernel's EACCES used to be a
    # refusal, which ends the guest thread. It is the guest's errno now.
    ("lnx-integ-A1", "A", "a withheld nice is a refusal again (the thread dies)",
     PROCENV,
     """        if error.is_permission_denied() {
            let mut view = enter(c, &state);""",
     """        if false {
            let mut view = enter(c, &state);""",
     PROCENV_LINUX),
    ("lnx-integ-A2", "A", "EACCES is not recognised as a permission refusal",
     PROCESS_ERROR,
     """            if *errno == libc_errno::EACCES || *errno == libc_errno::EPERM)""",
     """            if *errno == libc_errno::EPERM)""",
     PROCENV_LINUX),
    ("lnx-integ-A3", "A", "the guest's errno is not the kernel's EACCES",
     PROCENV,
     """            view.set_errno(omni_bionic::errno::consts::EACCES);
            drop(view);
            c.ret().i32(-1);
            return Ok(());""",
     """            view.set_errno(omni_bionic::errno::consts::EPERM);
            drop(view);
            c.ret().i32(-1);
            return Ok(());""",
     PROCENV_LINUX),
    ("lnx-integ-B1", "B", "a withheld nice is answered as success",
     PROCENV,
     """            view.set_errno(omni_bionic::errno::consts::EACCES);
            drop(view);
            c.ret().i32(-1);
            return Ok(());""",
     """            drop(view);
            c.ret().i32(0);
            return Ok(());""",
     PROCENV_LINUX),
    # The relro test's verdict on unix is the signal, not a Windows exit code. Two rows show the
    # verdict discriminates on Linux: a child that dies some OTHER way must not pass as a fault,
    # and a relro that is never sealed must be caught.
    ("lnx-integ-A4", "A", "the relro child dies of abort, not of the write fault",
     LOADER_M1,
     """        unsafe { std::ptr::write_volatile(f.space.ptr(at, 1).expect("in the space"), 0x5a) };""",
     """        let _ = at;
        std::process::abort();""",
     RELRO),
    ("lnx-integ-A5", "A", "relro is not sealed by default",
     "crates/omni-elf/src/loader/mod.rs",
     """            seal_relro: true,""",
     """            seal_relro: false,""",
     RELRO),
    # Restoring a minimised window: GNOME's Mutter restores an X11 client only through
    # _NET_ACTIVE_WINDOW (measured with xclock/xprop on the owner's Xwayland); a map alone leaves it
    # Iconic. The test runs its own Xvfb and a manager with exactly those measured rules.
    ("lnx-integ-A6", "A", "restore only maps the window (Mutter leaves it iconic)",
     WINDOW_LINUX,
     """            if self.window_manager_running() {
                self.request_focus();
            }
            return Ok(());""",
     """            return Ok(());""",
     EWMH),
]
