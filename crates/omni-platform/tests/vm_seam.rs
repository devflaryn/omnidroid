//! Tests of the platform-independent half of the virtual-memory seam: the contract every backend
//! shares, the invariants of [`Protection`], and the diagnostic quality of the error type.
//!
//! These run on every target. The Windows-specific behaviour is in `vm_windows.rs` and the
//! commit-charge measurements are in `vm_commit_charge.rs`.

use omni_platform::vm::{self, MapExecutability, OsError, Protection, VmError};

#[test]
fn no_protection_is_both_writable_and_executable() {
    // The W^X invariant, asserted over every variant rather than trusted to review. If someone
    // adds a `ReadWriteExecute` variant to `Protection::ALL`, this fails.
    for protection in Protection::ALL {
        assert!(
            !(protection.is_writable() && protection.is_executable()),
            "{protection} is both writable and executable; Omnidroid never holds a W+X page (D12)"
        );
    }
    assert_eq!(Protection::ALL.len(), 4, "Protection gained or lost a variant");

    // And the three that carry access are distinguishable from each other and from `None`.
    assert!(!Protection::None.is_readable());
    assert!(Protection::Read.is_readable());
    assert!(Protection::ReadWrite.is_writable());
    assert!(Protection::ReadExecute.is_executable());
    assert!(!Protection::Read.is_writable());
    assert!(!Protection::ReadWrite.is_executable());
}

#[test]
fn page_size_is_a_power_of_two_and_divides_the_allocation_granularity() {
    let page = vm::page_size();
    let granularity = vm::allocation_granularity();
    assert!(page.is_power_of_two(), "page size {page} is not a power of two");
    assert!(page >= 4096, "page size {page} is below the 4 KB minimum this design assumes");
    assert!(
        granularity.is_power_of_two(),
        "allocation granularity {granularity} is not a power of two"
    );
    assert_eq!(
        granularity % page,
        0,
        "allocation granularity {granularity} is not a multiple of the page size {page}"
    );
}

#[test]
fn zero_size_is_rejected_before_the_os_is_called() {
    let err = vm::reserve(0, vm::allocation_granularity()).unwrap_err();
    assert_eq!(err, VmError::ZeroSize { operation: "reserve" });
    assert!(err.to_string().contains("size of 0 bytes"), "{err}");
}

#[test]
fn alignment_must_be_a_power_of_two() {
    for align in [0usize, 3, 100, 65535] {
        let err = vm::reserve(vm::allocation_granularity(), align).unwrap_err();
        assert_eq!(
            err,
            VmError::AlignmentNotPowerOfTwo { operation: "reserve", align: align as u64 },
            "alignment {align} should have been rejected"
        );
        // Global Constraint 7: the message names the value that was wrong.
        assert!(err.to_string().contains(&align.to_string()), "{err}");
    }
}

#[test]
fn os_errors_render_with_their_symbolic_names() {
    // The codes that the Windows measurements turn on. A bare number in a log is not diagnostic.
    for (code, name) in [
        (5u32, "ERROR_ACCESS_DENIED"),
        (6, "ERROR_INVALID_HANDLE"),
        (87, "ERROR_INVALID_PARAMETER"),
        (487, "ERROR_INVALID_ADDRESS"),
        (1132, "ERROR_MAPPED_ALIGNMENT"),
        (1455, "ERROR_COMMITMENT_LIMIT"),
    ] {
        let err = OsError(code);
        assert_eq!(err.name(), Some(name));
        let text = err.to_string();
        assert!(text.contains(&code.to_string()), "{text} should contain {code}");
        assert!(text.contains(name), "{text} should contain {name}");
    }
    // An unknown code still reports its number rather than being swallowed.
    assert_eq!(OsError(123_456).name(), None);
    assert_eq!(OsError(123_456).to_string(), "os error 123456");
}

#[test]
fn errors_name_the_operation_and_the_offending_values() {
    let misaligned = VmError::Misaligned {
        operation: "map_file",
        what: "file offset",
        value: 4660,
        required: 4096,
        os_equivalent: OsError(1132),
    };
    let text = misaligned.to_string();
    for fragment in ["map_file", "file offset", "4660", "4096", "ERROR_MAPPED_ALIGNMENT"] {
        assert!(text.contains(fragment), "{text} should contain {fragment}");
    }
    assert_eq!(misaligned.os_error(), Some(OsError(1132)));

    let not_exact = VmError::PlaceholderNotExactSize {
        operation: "map_file",
        address: 0x7ff6_0000_0000,
        size: 4096,
        source: OsError(487),
    };
    let text = not_exact.to_string();
    for fragment in ["7ff600000000", "4096", "ERROR_INVALID_ADDRESS", "split_placeholder"] {
        assert!(text.contains(fragment), "{text} should contain {fragment}");
    }

    let not_exec = VmError::FileNotOpenedExecutable {
        operation: "map_file",
        path: r"C:\cache\libroblox.so".to_string(),
    };
    let text = not_exec.to_string();
    assert!(text.contains(r"C:\cache\libroblox.so"), "{text}");
    assert!(text.contains("Executable"), "{text} should say how to fix it");
    assert_eq!(not_exec.os_error(), None);

    let unsupported = VmError::Unsupported { operation: "map_file", platform: "linux" };
    assert!(unsupported.is_unsupported());
    let text = unsupported.to_string();
    assert!(text.contains("map_file"), "{text}");
    assert!(text.contains("linux"), "{text}");
    assert!(!misaligned.is_unsupported());
}

#[test]
fn the_two_partial_unmap_refusals_hand_back_the_numbers_needed_to_emulate_one() {
    // A caller that has to emulate a guest partial munmap must unmap the whole view and re-map the
    // survivors, so both refusals have to state the view's real base and length. If they did not,
    // the caller would go and work it out itself, which is how the refusal gets bypassed.
    let not_base = VmError::NotViewBase {
        address: 0x7ff6_0000_2000,
        view_base: 0x7ff6_0000_0000,
        view_len: 0x10_000,
        offset: 0x2000,
    };
    let text = not_base.to_string();
    for fragment in ["7ff600002000", "7ff600000000", "65536", "8192"] {
        assert!(text.contains(fragment), "{text} should contain {fragment}");
    }
    assert_eq!(not_base.os_error(), None);
    assert!(!not_base.is_unsupported());

    let mismatch = VmError::ViewSizeMismatch {
        operation: "unmap",
        address: 0x7ff6_0000_0000,
        requested: 4096,
        view_len: 0x10_000,
        surviving: 0x10_000 - 4096,
    };
    let text = mismatch.to_string();
    for fragment in ["unmap", "7ff600000000", "4096", "65536", "61440"] {
        assert!(text.contains(fragment), "{text} should contain {fragment}");
    }
    assert_eq!(mismatch.os_error(), None);
}

#[test]
fn map_executability_is_readable_at_a_call_site() {
    // It is an enum rather than a bool precisely so that logs and call sites say which it is.
    assert_eq!(MapExecutability::Executable.to_string(), "executable");
    assert_eq!(MapExecutability::NonExecutable.to_string(), "non-executable");
    assert_ne!(MapExecutability::Executable, MapExecutability::NonExecutable);
}

/// On a platform whose backend is structural, every operation must fail with a typed error that
/// says so. A build there is honestly broken rather than quietly wrong.
#[test]
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn structural_backends_report_unsupported() {
    let err = vm::reserve(vm::allocation_granularity(), vm::allocation_granularity()).unwrap_err();
    assert!(err.is_unsupported(), "expected Unsupported, got {err}");
    assert_eq!(
        err,
        VmError::Unsupported { operation: "reserve", platform: std::env::consts::OS }
    );
    assert!(vm::process_commit_charge().unwrap_err().is_unsupported());
    assert!(vm::process_working_set().unwrap_err().is_unsupported());
}
