"""macOS rows: the fault workstream (omni-platform fault seam on macOS). Pure data; see
`__init__.py`."""

FAULT = "crates/omni-platform/src/fault/macos.rs"
FAULT_TESTS = [
    "cargo", "test", "-p", "omni-platform", "--test", "fault_macos", "--lib", "--no-fail-fast",
]

ROWS = [
    ("mac-fault-A1", "A", "threads started after the first install do not get the port",
     FAULT,
     """    if event == PTHREAD_INTROSPECTION_THREAD_START {
        let port = PORT.load(Ordering::Acquire);""",
     """    if event == PTHREAD_INTROSPECTION_THREAD_START && false {
        let port = PORT.load(Ordering::Acquire);""",
     FAULT_TESTS),
    ("mac-fault-A2", "A", "threads already running at the first install are not claimed",
     FAULT,
     """        if thread_id(thread) != Some(server) {
            claim_thread(thread, port);
        }""",
     """        let _ = server;""",
     FAULT_TESTS),
    ("mac-fault-A3", "A", "a declined fault is sent to the handlers again instead of being passed on",
     FAULT,
     """        if matches {
            return KERN_FAILURE;
        }""",
     """        let _ = matches;""",
     FAULT_TESTS),
    ("mac-fault-A4", "A", "the vector registers are not restored after the handler",
     FAULT,
     """        thread_set_state(thread, ARM_NEON_STATE64, core::ptr::addr_of!(slot.neon).cast(), words::<NeonState>())
            == KERN_SUCCESS
            && thread_set_state(""",
     """        thread_set_state(""",
     FAULT_TESTS),
    ("mac-fault-A5", "A", "every data abort is reported as a read (WnR ignored)",
     FAULT,
     """        0x24 | 0x25 => Some(if esr & (1 << 6) != 0 { FaultAccess::Write } else { FaultAccess::Read }),""",
     """        0x24 | 0x25 => Some(FaultAccess::Read),""",
     FAULT_TESTS),
    ("mac-fault-A6", "A", "a breakpoint that is not the trampoline's is treated as handled",
     FAULT,
     """    if current.pc != trampoline_brk() {
        // Somebody else's breakpoint: the debugger's, on the task port.
        return KERN_FAILURE;
    }""",
     """    if current.pc != trampoline_brk() {
        return KERN_SUCCESS;
    }""",
     FAULT_TESTS),
    ("mac-fault-A7", "A", "release returns without waiting for a handler in flight",
     FAULT,
     """    if slot.active.load(Ordering::SeqCst) != 0 {
        DRAINED.fetch_add(1, Ordering::Relaxed);
        let mut spins: u32 = 0;
        while slot.active.load(Ordering::SeqCst) != 0 {""",
     """    if slot.active.load(Ordering::SeqCst) != 0 {
        DRAINED.fetch_add(1, Ordering::Relaxed);
        let mut spins: u32 = 0;
        while slot.active.load(Ordering::SeqCst) != 0 && false {""",
     FAULT_TESTS),
    ("mac-fault-A8", "A", "one general register (x9) is lost on the way back from the handler",
     FAULT,
     """    // SAFETY: the thread is suspended in this exception; both states are complete structs saved
    // from it.
    let restored = unsafe {""",
     """    slot.saved.x[9] = 0;
    // SAFETY: the thread is suspended in this exception; both states are complete structs saved
    // from it.
    let restored = unsafe {""",
     FAULT_TESTS),
    ("mac-fault-B1", "B", "the handlers run on the server thread (no trampoline): the faulting "
     "thread's own thread-locals are not the ones the handler sees",
     FAULT,
     """    let Some(index) = server.slots.iter().position(|slot| !slot.in_use) else {""",
     """    if any_handler() {
        let fault = Fault { address: address as usize, access, instruction_pointer: saved.pc as usize };
        EXAMINED.fetch_add(1, Ordering::Relaxed);
        return match dispatch(&fault) {
            FaultOutcome::Resolved => KERN_SUCCESS,
            FaultOutcome::NotOurs => KERN_FAILURE,
        };
    }
    let Some(index) = server.slots.iter().position(|slot| !slot.in_use) else {""",
     FAULT_TESTS),
]
