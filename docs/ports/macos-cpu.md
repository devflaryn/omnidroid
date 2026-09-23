# macOS port — the CPU workstream (dynarmic's arm64 backend, `omni-cpu`)

Host: Apple M1, 16 GB, macOS 26.5, arm64. Branch `mac-cpu`. The orchestrator folds this file into
`docs/ports/macos.md`.

## Merge notes (every edit to code that also compiles on Windows)

Each is additive and leaves Windows x86-64 behaviour byte-for-byte unchanged.

1. `crates/omni-cpu/Cargo.toml`: the `dynarmic-sys` target section is
   `cfg(any(target_arch = "x86_64", target_arch = "aarch64"))` instead of `cfg(target_arch = "x86_64")`.
2. `crates/omni-cpu/src/lib.rs`: `pub mod dynarmic` is compiled for
   `any(target_arch = "x86_64", target_arch = "aarch64")`.
3. `crates/omni-cpu/src/dynarmic/mod.rs`: the `mxcsr` module is `#[cfg(target_arch = "x86_64")]`
   (unchanged body); a new `#[cfg(target_arch = "aarch64")] mod fpcr` with the same interface is
   re-exported as `mxcsr` on aarch64. See "FPCR in host callbacks" below.
4. `crates/omni-cpu/tests/*.rs`: the crate-level `cfg` of every test file names both architectures.
   `tests/thunk.rs`: the two `MXCSR` helpers are `#[cfg(target_arch = "x86_64")]`, with `FPCR`
   twins for aarch64, and the two literal flush-bit masks became one cfg'd `HOST_FLUSH_BITS` constant
   (same value on x86-64).

## FPCR in host callbacks

The x86-64 finding that the guest's `MXCSR` is live inside host callbacks has an exact arm64 twin.
dynarmic's arm64 prelude (`A64AddressSpace::EmitPrelude`) saves the host `FPCR`, installs the
guest's, and restores the host's only in `return_from_run_code`; the call trampolines
(`EmitCallTrampoline`) switch nothing. So an inline thunk handler reached through `CallSVC` runs host
Rust floating point under the guest's `FZ`/`DN`/`RMode`. The dispatcher's guard therefore switches
`FPCR` on aarch64, at the same single place, and `tests/thunk.rs`'s
`the_dispatcher_puts_the_host_mxcsr_under_a_handler_and_the_guest_s_back` asserts it on this host with
`FPCR.FZ` (bit 24) as the flush bit.
