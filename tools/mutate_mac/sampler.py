"""macOS rows: the sampler seam's macOS backend (`crates/omni-platform/src/sampler/macos.rs`), which
`OMNI_PERF` samples guest threads through. Pure data; see `__init__.py`.

    python3 tools/mutate.py --only mac-sampler

Not rowed, because no test can see it: the resume on the path where `thread_get_state` fails (the
read does not fail for a live thread, and a thread left suspended by the *success* path hangs the
test binary instead of failing a test).
"""

SAMPLER = "crates/omni-platform/src/sampler/macos.rs"
UNIT = ["cargo", "test", "-p", "omni-platform", "--release", "--lib", "--no-fail-fast", "sampler::"]

ROWS = [
    ("mac-sampler-A1", "A", "'did it run' is the charged time alone, which lags a running thread by a tick",
     SAMPLER,
     """        Ok((Self::charged(&info).as_nanos() as u64).wrapping_add(running))""",
     """        Ok((Self::charged(&info).as_nanos() as u64).wrapping_add(running * 0))""",
     UNIT),
    ("mac-sampler-A2", "A", "the sampled pc is read from the stack pointer's slot",
     SAMPLER,
     """            let ip = state.pc as usize;""",
     """            let ip = state.sp as usize;""",
     UNIT),
    ("mac-sampler-A3", "A", "a private executable region (the MAP_JIT code cache) is not told as one",
     SAMPLER,
     """    if protection & VM_PROT_EXECUTE != 0 && shared == 0 {""",
     """    if protection & VM_PROT_EXECUTE != 0 && shared != 0 {""",
     UNIT),
    ("mac-sampler-A4", "A", "an image's extent is taken from __LINKEDIT, which the shared cache's images share",
     SAMPLER,
     """            if segment.segname.starts_with(b"__TEXT\\0") {""",
     """            if segment.segname.starts_with(b"__LINKEDIT\\0") {""",
     UNIT),
    ("mac-sampler-A5", "A", "the performance levels are numbered fastest first, so the E-cores read as P-cores",
     SAMPLER,
     """    for (level, &n) in counts.iter().enumerate().rev() {""",
     """    for (level, &n) in counts.iter().enumerate() {""",
     UNIT),
    ("mac-sampler-B1", "B", "any private region is a code cache, executable or not",
     SAMPLER,
     """    if protection & VM_PROT_EXECUTE != 0 && shared == 0 {""",
     """    if shared == 0 {""",
     UNIT),
    ("mac-sampler-B2", "B", "every read of 'did it run' says yes, a parked thread included",
     SAMPLER,
     """        let running = if info.run_state == TH_STATE_RUNNING {""",
     """        let running = if true {""",
     UNIT),
]
