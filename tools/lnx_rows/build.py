"""lnx-build-: the dynarmic build script on a GNU toolchain.

Row format (the same seven fields as `tools/mutate.py`'s table):
    (id, direction "A" revert-a-fix | "B" over-correct, description, path, old, new, argv)
`old` must match the file exactly once; `argv` must pass on the unmutated tree.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import HOME, with_env  # noqa: E402

# The dynarmic CMake tree the rows that rebuild dynarmic use. Separate from the everyday one so
# that a row which breaks CMake's configure cannot leave the tree every other build uses
# half-reconfigured.
# Per checkout, as `~/odb/cargo-locked`'s everyday tree is: CMake refuses a cache made from another
# source directory, and a worktree's `vendor/dynarmic` is another directory (MEASURED: a run from
# the main checkout after one from a worktree failed its pre-flight on exactly that).
_CHECKOUT = os.path.basename(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
MUTATION_DYNARMIC_DIR = os.path.join(HOME, "odb", f"dynarmic-mut-{_CHECKOUT}")

DYNARMIC_BUILD_RS = "crates/dynarmic-sys/build.rs"

# Build the dynarmic tree in its own directory and run the smallest test that executes guest code
# through it.
DYNARMIC_BUILD = with_env(
    {"OMNIDROID_DYNARMIC_BUILD_DIR": MUTATION_DYNARMIC_DIR},
    ["cargo", "test", "-p", "dynarmic-sys", "--release", "--no-fail-fast", "--test", "a64_exec"],
)

ROWS = [
    # dynarmic-sys/build.rs handed CMake the C++ driver as the C compiler. `cl.exe` compiles both;
    # `c++` does not, and CMake's C compiler check fails ("The C compiler identification is
    # unknown ... broken") before anything is built. The row reverts the fix.
    ("lnx-build-A1", "A", "CMake is given the C++ driver as its C compiler",
     DYNARMIC_BUILD_RS,
     """    c.arg(format!("-DCMAKE_C_COMPILER={}", cmake_path(&c_compiler_path(compiler))));""",
     """    c.arg(format!("-DCMAKE_C_COMPILER={}", cmake_path(compiler.path())));""",
     DYNARMIC_BUILD),
]
