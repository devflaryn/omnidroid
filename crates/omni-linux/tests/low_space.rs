//! Every guest process asks for the same low range (`process::reserve_space`); the first holds it,
//! reserved around what the host held there then. A host piece inside it that is later given back
//! (a host thread's 1 MiB stack, when the thread exits) is the one free part of the range -- and a
//! space reserved around everything else was taken with 1 MiB free of 64 GiB (D5: every service
//! init started after odsign stopped failed to map its first segment, ~1 boot in 3).
use omni_platform::vm;

const LOW_BASE: usize = 0x1000_0000;

#[test]
fn a_later_space_is_whole_even_when_the_host_frees_a_piece_of_the_low_range() {
    let granule = vm::allocation_granularity();
    // A host allocation inside the low range, as a host thread's stack can be.
    let at = LOW_BASE + (1 << 30);
    let Ok(stack) = vm::reserve_placeholder_at(at, 1 << 20) else {
        eprintln!("the host already holds {at:#x}; nothing to reproduce here");
        return;
    };
    let first = omni_linux::process::reserve_space().expect("the first space");
    assert_eq!(first.base(), LOW_BASE, "the first space takes the low range, around the host's piece");
    // The host thread exits: its piece of the low range is free, and nobody's.
    vm::release(stack).expect("the host's piece given back");
    assert!(granule <= 1 << 20);

    let second = omni_linux::process::reserve_space().expect("the second space");
    let stats = second.stats();
    assert!(
        stats.free >= 32 << 30,
        "a process's space must be (nearly) all its own: {} bytes free of {} at {:#x}",
        stats.free,
        stats.reserved,
        second.base()
    );
    drop(first);
}
