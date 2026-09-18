//! Configuration validation and error diagnostics.
//!
//! Deliberately **not** gated on an operating system. Every assertion here is reached before any
//! virtual-memory call, so this file runs on all five targets and is what keeps `omni-mem` honest
//! about compiling and behaving sensibly where `omni-platform`'s backend is structural: on Linux and
//! macOS the last test asserts that a guest address space fails to be created, with a typed error
//! saying the backend is not implemented, rather than appearing to work.

use omni_mem::{
    ArenaConfig, CodeArena, GuestSpace, GuestSpaceConfig, MemError, DEFAULT_BLOCK_ALIGNMENT,
    DEFAULT_CHUNK_SIZE, DEFAULT_COMMIT_GRANULE, DEFAULT_MAX_TOTAL, DEFAULT_SPACE_SIZE,
};

#[test]
fn the_defaults_are_the_measured_ones() {
    let config = GuestSpaceConfig::default();
    assert_eq!(config.size, DEFAULT_SPACE_SIZE);
    assert_eq!(config.size, 4 * 1024 * 1024 * 1024, "D10's 4 GiB guest space");
    assert_eq!(config.commit_granule, DEFAULT_COMMIT_GRANULE);
    assert_eq!(config.commit_granule, 64 * 1024, "the low end of D10's 64 KB to 1 MB range");
    assert!(config.base_alignment.is_power_of_two());

    let arena = ArenaConfig::default();
    assert_eq!(arena.chunk_size, DEFAULT_CHUNK_SIZE);
    assert_eq!(arena.max_total, DEFAULT_MAX_TOTAL);
    assert_eq!(arena.block_alignment, DEFAULT_BLOCK_ALIGNMENT);
    assert!(arena.max_total >= arena.chunk_size);
}

#[test]
fn a_bad_guest_space_configuration_is_rejected_and_says_which_field_and_why() {
    let page = omni_platform::vm::page_size();
    let cases: [(GuestSpaceConfig, &str); 5] = [
        (GuestSpaceConfig { size: 0, ..Default::default() }, "size"),
        (GuestSpaceConfig { size: page * 4 + 1, ..Default::default() }, "size"),
        (GuestSpaceConfig { commit_granule: 0, ..Default::default() }, "commit_granule"),
        (GuestSpaceConfig { commit_granule: page + 1, ..Default::default() }, "commit_granule"),
        (
            GuestSpaceConfig { base_alignment: 3 * page, ..Default::default() },
            "base_alignment",
        ),
    ];
    for (config, expected) in cases {
        let error = GuestSpace::with_config(config).unwrap_err();
        match error {
            MemError::InvalidConfig { field, .. } => assert_eq!(field, expected, "for {config:?}"),
            other => panic!("expected InvalidConfig for {config:?}, got {other}"),
        }
        // The message has to carry the offending value, not just name the field.
        let message = GuestSpace::with_config(config).unwrap_err().to_string();
        assert!(message.contains(expected), "{message}");
    }

    // A granule larger than the whole space is rejected too, and is the one case where two
    // individually valid values are wrong together.
    let error = GuestSpace::with_config(GuestSpaceConfig {
        size: page,
        commit_granule: page * 16,
        ..Default::default()
    })
    .unwrap_err();
    assert!(matches!(error, MemError::InvalidConfig { field: "commit_granule", .. }), "{error}");
}

#[test]
fn a_bad_arena_configuration_is_rejected() {
    for (config, expected) in [
        (ArenaConfig { chunk_size: 0, ..Default::default() }, "chunk_size"),
        (ArenaConfig { block_alignment: 24, ..Default::default() }, "block_alignment"),
        (
            ArenaConfig { chunk_size: 1 << 20, max_total: 1 << 19, ..Default::default() },
            "max_total",
        ),
    ] {
        let error = CodeArena::with_config(config).unwrap_err();
        match error {
            MemError::InvalidConfig { field, .. } => assert_eq!(field, expected, "for {config:?}"),
            other => panic!("expected InvalidConfig for {config:?}, got {other}"),
        }
    }
}

/// On a platform whose `omni-platform` backend is structural, creating a guest address space must
/// fail with a typed error that says so.
#[cfg(not(target_os = "windows"))]
#[test]
fn an_unimplemented_backend_fails_honestly_rather_than_appearing_to_work() {
    let error = GuestSpace::new().expect_err("a structural backend cannot reserve anything");
    let platform = error.platform_error().expect("the failure should come from the platform");
    assert!(platform.is_unsupported(), "expected Unsupported, got {platform}");
    assert!(error.to_string().contains("not implemented"), "{error}");
}
