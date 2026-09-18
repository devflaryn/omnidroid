# `dynarmic-sys`

Raw FFI bindings to the pinned dynarmic A64 JIT: the vendored source, the build
script that compiles it, the `extern "C"` shim over its virtual-callback
interfaces, and the `#[repr(C)]` declarations that match the shim.

Nothing here decides policy. The `GuestCpu` trait lives in `omni-cpu`, which has
**no build script and no C++ dependency**, so that trait keeps compiling on a
host with no C++ toolchain — which is the host the ARM64-native path
(`ARCHITECTURE.md` §6) targets. Mapping this crate onto the trait is a separate
piece of work.

## What is vendored

| | |
|---|---|
| dynarmic | `yuzu-mirror/dynarmic@9d4582339990d4eae53f1dc7160686920fc2075c` (6.7.0) |
| Boost | a 1,786-file header subset of 1.88.0 |

`vendor/PIN.txt` records both, why the tree is vendored rather than
submoduled, and how the Boost subset was derived. `LICENSES.md` records every
licence, including one — BSL-1.0 — that D3 does not currently name.

## Building

Needs CMake (3.12+, 4.x works) and a C++20 compiler. `cl.exe` does not need to
be on `PATH`; the `cc` crate locates MSVC through the registry and the build
script passes its environment to CMake. Ninja is optional but strongly
preferred. If any of that is missing, the build script says which one and stops,
rather than letting CMake print a page about a failed compiler check.

Four things the build script handles that are not obvious, all of them recorded
in D5 and re-confirmed here:

* **Boost is an undeclared dynarmic dependency** (`boost::icl` for code-cache
  invalidation ranges, `boost::variant` for the IR terminal type).
* **`-DCMAKE_POLICY_VERSION_MINIMUM=3.5`** is required: `robin-map` still
  declares `cmake_minimum_required(VERSION 3.1)`, which CMake 4.x refuses.
* **Build paths must be short.** MSVC still fails with `C1083` past 260
  characters and CMake nests object files ~120 characters below the binary
  directory. The script checks the budget before starting and points at
  `OMNIDROID_DYNARMIC_BUILD_DIR` if it is exceeded.
* dynarmic calls `find_package(Boost 1.57 REQUIRED)`. `cmake/boost/` provides a
  CONFIG-mode package for the vendored subset, so this keeps working when CMake
  finishes removing its deprecated `FindBoost` module.

dynarmic is always built `Release`, whatever Cargo profile is in use: a debug
build of a JIT is too slow to run a game engine under, and Rust's msvc target
links the release CRT regardless, so a Debug build would also mix CRTs.

## Two things to read before using this crate

Both are in the crate documentation (`cargo doc -p dynarmic-sys --open`) and
both are load-bearing.

1. **Re-entrancy.** Every callback can be entered from generated guest code at
   any instruction boundary. The context pointer must not be the object through
   which `od_jit_run` was reached, `run` must take `&self`, and callbacks must
   not unwind. `tests/harness/mod.rs` is the worked example.
2. **Configuration that is not optional.** Identity fastmem is 13.2x faster than
   the callback path (D4) and degrades to it *silently*; a runaway guest is
   unstoppable with dynarmic's default optimization flags. `od_jit_effective_config`
   and `od_jit_stats` exist so both can be asserted rather than assumed.
