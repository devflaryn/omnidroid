//! `OMNI_ALLOC_TRACE_KB`: the tracing allocator records a live large allocation with its stack,
//! forgets it when freed or moved, and reports it -- and leaves small ones alone.
use omni_linux::alloc_trace::{live, report, start, Tracing};

#[global_allocator]
static HEAP: Tracing = Tracing;

#[test]
fn large_allocations_are_traced_and_forgotten() {
    std::env::set_var("OMNI_ALLOC_TRACE_KB", "512");
    start();
    let (n0, b0) = live();
    let big = vec![7u8; 1 << 20];
    let small = vec![1u8; 4096];
    let (n1, b1) = live();
    assert_eq!((n1 - n0, b1 - b0), (1, 1 << 20), "the 1 MiB block, not the 4 KiB one");
    let mut grown = big;
    grown.resize(3 << 20, 0);
    let (n2, b2) = live();
    assert_eq!((n2 - n0, b2 - b0), (1, grown.capacity()), "moved by the realloc, counted once");
    report();
    drop(grown);
    drop(small);
    assert_eq!(live(), (n0, b0), "freed");
}
