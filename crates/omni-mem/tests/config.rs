//! Configuration validation and error diagnostics.
//!
//! Deliberately **not** gated on an operating system. Every assertion here is reached before any
//! virtual-memory call, so this file runs on all five targets and is what keeps `omni-mem` honest
//! about compiling and behaving sensibly where `omni-platform`'s backend is structural: on Linux and
//! macOS the last test asserts that a guest address space fails to be created, with a typed error
//! saying the backend is not implemented, rather than appearing to work.

use omni_mem::{
    ArenaConfig, CodeArena, GuestSpace, GuestSpaceConfig, MemError, DEFAULT_BLOCK_ALIGNMENT,
    DEFAULT_CHUNK_SIZE, DEFAULT_COMMIT_GRANULE, DEFAULT_MAX_COMMITTED,
    DEFAULT_MAX_COMMIT_REQUEST, DEFAULT_MAX_TOTAL, DEFAULT_SPACE_SIZE,
};

#[test]
fn the_defaults_are_the_measured_ones() {
    let config = GuestSpaceConfig::default();
    assert_eq!(config.size, DEFAULT_SPACE_SIZE);
    assert_eq!(config.size, 4 * 1024 * 1024 * 1024, "D10's 4 GiB guest space");
    assert_eq!(config.commit_granule, DEFAULT_COMMIT_GRANULE);
    assert_eq!(config.commit_granule, 64 * 1024, "the low end of D10's 64 KB to 1 MB range");
    assert!(config.base_alignment.is_power_of_two());
    // The two commit ceilings, pinned here because they are the bound on the *scarce* resource and
    // because the pair only works if each stays on its own side of the gap between legitimate use
    // and the demonstrated attack.
    assert_eq!(config.max_committed, DEFAULT_MAX_COMMITTED);
    assert_eq!(config.max_commit_request, DEFAULT_MAX_COMMIT_REQUEST);
    assert!(config.max_commit_request <= config.max_committed);

    // The loose one. It must clear D10's measured 3 GB of live use plus its page-table charge
    // (size/512), because the project goal has an instance legitimately needing several GB during
    // startup — a default that refuses that has broken the feature in the name of fixing the hole.
    const THREE_GB: usize = 3 * 1024 * 1024 * 1024;
    assert!(
        config.max_committed >= THREE_GB + THREE_GB / 512,
        "the total ceiling must permit D10's validated 3 GB of live use at the defaults"
    );
    // And it must stay below the space, or it bounds nothing: total commit can never exceed the
    // space's own size anyway.
    assert!(
        config.max_committed < config.size,
        "a commit ceiling at or above the space size bounds nothing"
    );

    // The tight one, which is what actually refuses the attack. Bracketed on both sides by measured
    // values: above the largest eager mapping anywhere in the workspace (the D10 requirement test's
    // 64 MiB chunk) and far below the smaller demonstrated tamper (+1026.004 MiB from a 1 GiB
    // `p_memsz`). The largest private anonymous piece any real library asks for is `libroblox.so`'s
    // 11,575,296-byte `.bss`; the next largest across the eleven is 61,440 bytes.
    const LARGEST_REAL_SEGMENT: usize = 11_575_296;
    const LARGEST_EAGER_MAPPING_IN_THE_SUITE: usize = 64 * 1024 * 1024;
    const SMALLER_DEMONSTRATED_ATTACK: usize = 1_076_000_000;
    assert!(
        config.max_commit_request >= 2 * LARGEST_EAGER_MAPPING_IN_THE_SUITE,
        "the per-request ceiling must leave room for the largest eager mapping the suite makes"
    );
    assert!(
        config.max_commit_request >= 8 * LARGEST_REAL_SEGMENT,
        "and a wide margin over the largest segment any real library asks to be committed at once"
    );
    assert!(
        config.max_commit_request * 8 <= SMALLER_DEMONSTRATED_ATTACK,
        "and it must refuse the measured tampered `p_memsz` with room to spare, or the pair does \
         not separate the attack from legitimate growth"
    );

    let arena = ArenaConfig::default();
    assert_eq!(arena.chunk_size, DEFAULT_CHUNK_SIZE);
    assert_eq!(arena.max_total, DEFAULT_MAX_TOTAL);
    assert_eq!(arena.block_alignment, DEFAULT_BLOCK_ALIGNMENT);
    assert!(arena.max_total >= arena.chunk_size);
}

#[test]
fn a_bad_guest_space_configuration_is_rejected_and_says_which_field_and_why() {
    let page = omni_platform::vm::page_size();
    let cases: [(GuestSpaceConfig, &str); 8] = [
        (GuestSpaceConfig { size: 0, ..Default::default() }, "size"),
        (GuestSpaceConfig { size: page * 4 + 1, ..Default::default() }, "size"),
        (GuestSpaceConfig { commit_granule: 0, ..Default::default() }, "commit_granule"),
        (GuestSpaceConfig { commit_granule: page + 1, ..Default::default() }, "commit_granule"),
        (
            GuestSpaceConfig { base_alignment: 3 * page, ..Default::default() },
            "base_alignment",
        ),
        // A ceiling of zero would refuse every commit. There is deliberately no "unlimited" value:
        // an absent or saturating limit is how hostile input becomes a larger permission.
        (GuestSpaceConfig { max_committed: 0, ..Default::default() }, "max_committed"),
        (GuestSpaceConfig { max_commit_request: 0, ..Default::default() }, "max_commit_request"),
        // A per-request ceiling above the total is a limit that can never bind.
        (
            GuestSpaceConfig {
                max_committed: 1024 * 1024,
                max_commit_request: 2 * 1024 * 1024,
                ..Default::default()
            },
            "max_commit_request",
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
        // A block alignment larger than a chunk. `1 << 62` used to be accepted, and then no address
        // inside a chunk could satisfy it, so every single allocation took a chunk of its own — an
        // arena that looks like it works and charges a chunk per block. It also overflowed the
        // round-up in `carve`.
        (ArenaConfig { block_alignment: 1 << 62, ..Default::default() }, "block_alignment"),
        (
            ArenaConfig {
                chunk_size: 64 * 1024,
                block_alignment: 128 * 1024,
                ..Default::default()
            },
            "block_alignment",
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
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
#[test]
fn an_unimplemented_backend_fails_honestly_rather_than_appearing_to_work() {
    let error = GuestSpace::new().expect_err("a structural backend cannot reserve anything");
    let platform = error.platform_error().expect("the failure should come from the platform");
    assert!(platform.is_unsupported(), "expected Unsupported, got {platform}");
    assert!(error.to_string().contains("not implemented"), "{error}");
}
