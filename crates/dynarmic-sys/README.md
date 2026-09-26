# `dynarmic-sys`

Raw FFI to the pinned dynarmic A64 JIT: the vendored source, the build script, the `extern "C"`
shim over dynarmic's callback interfaces (`shim/od_dynarmic.{h,cpp}`, `OD_DYNARMIC_ABI_VERSION`
4), and the matching `#[repr(C)]` declarations in `src/lib.rs`. No policy lives here: `omni-cpu`
(`src/dynarmic/`) maps it onto the `GuestCpu` trait behind its default `dynarmic` feature.

## Vendored

| | |
|---|---|
| dynarmic | `yuzu-mirror/dynarmic@9d4582339990d4eae53f1dc7160686920fc2075c` (6.7.0) + `patches/` |
| Boost | 1.88.0, a derived 1,786-file header subset (`vendor/boost/README.md`) |

`vendor/PIN.txt` says why the tree is vendored rather than a submodule (the upstream is gone, D5).
`LICENSES.md` lists every licence, including BSL-1.0. Patches: `patches/README.md`.

## Building

Needs CMake 3.12+ (4.x works) and a C++20 compiler; Ninja is optional but preferred. On Windows
the `cc` crate finds MSVC itself. A missing tool stops the build with a one-line reason. dynarmic
is always built `Release` with only the A64 frontend, whatever the Cargo profile.

The build script handles:

- Boost, an undeclared dynarmic dependency (`boost::icl`, `boost::variant`), through
  `cmake/boost/`'s CONFIG package.
- `-DCMAKE_POLICY_VERSION_MINIMUM=3.5`, which CMake 4.x needs for `robin-map`.
- MSVC's 260-character path limit (`C1083`): it checks the budget first; set
  `OMNIDROID_DYNARMIC_BUILD_DIR` to a short path (e.g. `C:\od-build`) if it is exceeded.
- `-DDYNARMIC_USE_BUNDLED_EXTERNALS=ON` off Windows, so a Homebrew `fmt` is not picked up.

Cargo watches only `build.rs`, the shim and `vendor/PIN.txt`: touch `PIN.txt` (or
`cargo clean -p dynarmic-sys`) after editing anything under `vendor/`. `CMAKE`, `CXX` and `CC`
select the tools.

## Before using it

1. **Re-entrancy.** A callback can be entered from generated code at any instruction boundary.
   The context pointer must not be the object `od_jit_run` was reached through, `run` takes
   `&self`, and callbacks must not unwind. `tests/harness/mod.rs` is the worked example.
2. **Fastmem degrades silently.** Identity fastmem is 30-49x faster than the callback path (D4).
   Assert it with `od_jit_effective_config` and `od_jit_stats`.
3. **Stopping a runaway guest.** `optimization::INTERRUPTIBLE` is `ALL_SAFE` on x64 and
   `ALL_SAFE & !FAST_DISPATCH` on arm64 (patches 0018-0020, D35). A direct-branch loop still honours
   only one of budget or halt unless `BLOCK_LINKING` is cleared, which costs about 7x on
   4-instruction blocks (`libroblox.so` averages 4.30). `tests/hostile.rs::the_stoppability_matrix`
   is the table; `patches/README.md` item 2 explains it.
4. **W^X.** x64's code cache is `PAGE_EXECUTE_READWRITE` (the D12 exception) and guest-addressable
   in principle under identity fastmem; Apple arm64 is W^X per thread (`MAP_JIT`, `tests/wx.rs`).
   The `w-xor-x` feature is refused unless `OMNIDROID_DYNARMIC_ALLOW_BROKEN_WX=1`, because it
   crashes on this pin (`patches/README.md` item 3).

## Tests

`cargo test -p dynarmic-sys --release`. With `OD_TEST_SHARED_CACHE=1` every test `Vm` runs on a
shared code cache of its own (x64). Benchmarks:
`cargo test -p dynarmic-sys --release --test bench -- --ignored --nocapture`.
