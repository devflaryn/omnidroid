# Vendored Boost header subset

Boost 1.88.0, headers only, 1,786 files of the 185 MiB tree. Licence: Boost
Software License 1.0, in `LICENSE_1_0.txt`; see `../../LICENSES.md` for why a
licence D3 does not name is here at all.

Boost is an **undeclared** dynarmic dependency. The prior-art survey listed
fmt / mcl / xbyak / zydis / robin-map; dynarmic also needs:

* `boost::icl` — `interval_set` and `interval_map`, for the set of guest address
  ranges whose translations are pending invalidation
  (`backend/block_range_information.h`, `backend/x64/a64_interface.cpp`);
* `boost::variant` — the IR terminal type (`ir/terminal.h`), which is recursive
  and therefore predates what `std::variant` can express comfortably.

`SUBSET.txt` lists exactly what is here. It is derived, not curated:
`../../tools/derive_boost_subset.py` takes the headers MSVC actually opened
while building the pin (from Ninja's dependency log), closes that set over every
`#include <boost/...>` regardless of preprocessor conditions, and adds
`boost/preprocessor/**` and `boost/mpl/aux_/preprocessed/**` whole because those
are reached through macro-built include directives that no textual scan can
follow.

**The measurement was taken on MSVC.** A GCC or Clang build may open headers
MSVC did not. Re-run the derivation script when adding a toolchain rather than
guessing; the build fails with a missing-header error, not silently.
