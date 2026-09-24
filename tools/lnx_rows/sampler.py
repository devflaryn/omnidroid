"""lnx-sampler-: the Linux backend of the sampler seam (`crates/omni-platform/src/sampler/linux.rs`)
and the host library names `OMNI_PERF` classifies by (`crates/omni-android/src/perf.rs`).

Row format (the same seven fields as `tools/mutate.py`'s table):
    (id, direction "A" revert-a-fix | "B" over-correct, description, path, old, new, argv)
`old` must match the file exactly once; `argv` must pass on the unmutated tree.

    flock ~/odb/build.lock python3 tools/mutate_linux.py --only lnx-sampler

Not rowed, because no test can see them: the handler's `errno` save (every system call it makes
succeeds for a mapped instruction pointer, so the value it would clobber is never changed), and the
install-only-over-a-default-disposition check (the handler is installed once per process, before
any test could have taken the signal).
"""

SAMPLER = "crates/omni-platform/src/sampler/linux.rs"
PERF = "crates/omni-android/src/perf.rs"

UNIT = ["cargo", "test", "-p", "omni-platform", "--release", "--no-fail-fast", "--lib", "sampler::"]
PERF_UNIT = ["cargo", "test", "-p", "omni-android", "--release", "--no-fail-fast", "--lib", "perf::"]

ROWS = [
    ("lnx-sampler-A1", "A", "a handler answers whatever request is armed, not the one its signal carries",
     SAMPLER,
     """        let seq = unsafe { (*core::ptr::from_ref(info).cast::<Queued>()).value } as u64;""",
     """        let seq = ARMED.load(Ordering::Acquire) & !CLAIMED;""",
     UNIT),
    ("lnx-sampler-A2", "A", "a timed-out request is left armed, so the late signal answers it",
     SAMPLER,
     """            if ARMED.compare_exchange(seq, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {""",
     """            if true {""",
     UNIT),
    ("lnx-sampler-A3", "A", "the sampled instruction pointer is read from RSP",
     SAMPLER,
     """            let ip = gregs[libc::REG_RIP as usize] as usize;""",
     """            let ip = gregs[libc::REG_RSP as usize] as usize;""",
     UNIT),
    ("lnx-sampler-A4", "A", "'did it run' reads the process's CPU clock, not the thread's",
     SAMPLER,
     """        let rc = unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut clock) };""",
     """        let rc = { clock = libc::CLOCK_PROCESS_CPUTIME_ID; 0 };""",
     UNIT),
    ("lnx-sampler-A5", "A", "anonymous executable memory is not told as a code cache",
     SAMPLER,
     """    if m.exec && !m.shared && m.path.is_empty() {""",
     """    if false && m.exec && !m.shared && m.path.is_empty() {""",
     UNIT),
    ("lnx-sampler-A6", "A", "every mapped file is an image, data files included",
     SAMPLER,
     """        .filter(|m| m.exec && is_image_path(&m.path))""",
     """        .filter(|m| is_image_path(&m.path))""",
     UNIT),
    ("lnx-sampler-A7", "A", "a maps path is cut at its first space",
     SAMPLER,
     """        path: rest.trim_start_matches(' ').trim_end().to_string(),""",
     """        path: rest.split_whitespace().next().unwrap_or("").to_string(),""",
     UNIT),
    ("lnx-sampler-A8", "A", "glibc is not counted as the operating system's",
     PERF,
     """        | "libc.so.6" | "ld-linux-x86-64.so.2" | "[vdso]" """,
     """        | "ld-linux-x86-64.so.2" | "[vdso]" """,
     PERF_UNIT),
    ("lnx-sampler-B1", "B", "any cached answer is trusted for a second, data included",
     SAMPLER,
     """            Some(kind @ (MemoryKind::Image { .. } | MemoryKind::PrivateWritableExecutable { .. }))""",
     """            Some(kind)""",
     UNIT),
    ("lnx-sampler-B2", "B", "any executable mapping that is not an image is a code cache, shared memfds included",
     SAMPLER,
     """    if m.exec && !m.shared && m.path.is_empty() {""",
     """    if m.exec && !is_image_path(&m.path) {""",
     UNIT),
    ("lnx-sampler-B3", "B", "the sampler does not wait for the handler's answer at all",
     SAMPLER,
     """const ANSWER_WAIT: Duration = Duration::from_millis(20);""",
     """const ANSWER_WAIT: Duration = Duration::from_millis(0);""",
     UNIT),
]
