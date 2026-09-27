//! Freeing a oneway transaction's buffer hands its node's next oneway to the process, as the
//! kernel's `binder_free_buf` does (`binder_enqueue_work_ilocked(w, &proc->todo)`), not to the thread
//! that freed it: libbinder frees a buffer from whatever thread drops the Parcel, and a thread that
//! only writes (`flushCommands`, a thread's exit) or waits on a reply held the next oneway, and
//! every later one to that node queued behind it ("64 oneway calls wait on node"). One test per
//! file: the broker is per host process.
mod binder_guest;

use binder_guest::*;

#[test]
fn the_next_oneway_goes_to_the_process_when_a_buffer_is_freed() {
    let mut receiver = Guest::new();
    let mut a = receiver.looper();
    let mut b = receiver.looper();
    receiver.set_context_mgr(&mut a);
    let mut sender = Guest::new();
    let mut s = sender.thread();
    for code in [1, 2] {
        let c = sender.transaction(BC_TRANSACTION, 0, code, TF_ONE_WAY, b"x", &[]);
        assert_eq!(sender.write_read(&mut s, &c, false).0, 0);
    }

    let first = receiver.read(&mut a, &[]);
    assert_eq!(codes(&first), [1], "the first oneway alone");
    // Thread A frees the buffer in a write alone, as libbinder's flushCommands does.
    assert_eq!(receiver.write_read(&mut a, &free(first[0].buffer), false).0, 0);
    // Thread B, idle in the looper, takes the next one.
    assert_eq!(codes(&receiver.read(&mut b, &[])), [2], "the next oneway is the process's, for any looper");
}
