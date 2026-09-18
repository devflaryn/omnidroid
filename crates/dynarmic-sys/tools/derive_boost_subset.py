"""Derive the vendored Boost header subset from a full Boost tree.

    python crates/dynarmic-sys/tools/derive_boost_subset.py \\
        --boost-root C:/boost_1_88_0 \\
        --ninja-build-dir target/dprobe/b \\
        --out crates/dynarmic-sys/vendor/boost

Boost is an undeclared dynarmic dependency: `boost::icl` for code-cache
invalidation ranges and `boost::variant` for the IR terminal type. Full Boost is
185 MiB, which is not a reasonable thing to vendor. This produces the subset
that is, and it produces it by measurement rather than by taste, so a re-pin or
a new toolchain can regenerate it instead of guessing.

Three steps:

1. Build dynarmic once against a full Boost with the Ninja generator. Ninja
   records every header each translation unit opened, from MSVC's
   `/showIncludes` or GCC/Clang's `-MD`, in `.ninja_deps`. `ninja -t deps`
   prints them. That is the ground truth for this compiler and this
   configuration.
2. Close that set textually: scan each header for `#include <boost/...>` and
   follow it, ignoring preprocessor conditions. This picks up the branches this
   compiler did not take.
3. Add `boost/preprocessor/**` and `boost/mpl/aux_/preprocessed/**` whole.
   Those are reached through macro-built `#include` directives -- `#include
   BOOST_PP_ITERATE()` and friends -- which no textual scan can follow, and
   their contents differ per compiler.

The result on MSVC 19.44 / x86-64, against Boost 1.88.0, is 1,786 files and
about 17 MiB.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys

BOOST_INCLUDE = re.compile(rb'^\s*#\s*include\s*[<"](boost/[^>"]+)[>"]', re.M)
WHOLE_DIRECTORIES = ("boost/preprocessor", "boost/mpl/aux_/preprocessed")


def observed_headers(build_dir):
    """Every boost header the compiler actually opened, from Ninja's dep log."""
    out = subprocess.run(["ninja", "-t", "deps"], cwd=build_dir, capture_output=True,
                         text=True, encoding="utf-8", errors="replace").stdout
    found = set()
    for match in re.finditer(r"[^\s]*[\\/]boost[\\/][^\s]+", out):
        path = match.group(0).replace("\\", "/")
        index = path.rfind("/boost/")
        if index >= 0:
            found.add(path[index + 1:])
    return found


def close_over_includes(boost_root, seeds):
    """Follow every `#include <boost/...>`, whatever the preprocessor conditions."""
    seen, missing, stack = set(), set(), list(seeds)
    while stack:
        rel = stack.pop().replace(os.sep, "/")
        if rel in seen or rel in missing:
            continue
        path = os.path.join(boost_root, rel)
        if not os.path.isfile(path):
            missing.add(rel)
            continue
        seen.add(rel)
        with open(path, "rb") as handle:
            data = handle.read()
        for match in BOOST_INCLUDE.finditer(data):
            stack.append(match.group(1).decode())
    return seen, missing


def whole_directories(boost_root):
    found = set()
    for directory in WHOLE_DIRECTORIES:
        for root, _, files in os.walk(os.path.join(boost_root, directory)):
            for name in files:
                path = os.path.join(root, name).replace(os.sep, "/")
                found.add(path[len(boost_root.replace(os.sep, "/")) + 1:])
    return found


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--boost-root", required=True, help="a full Boost source tree")
    parser.add_argument("--ninja-build-dir", required=True,
                        help="a Ninja build directory where dynarmic was built against it")
    parser.add_argument("--out", required=True, help="destination for the subset")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    boost_root = args.boost_root.replace("\\", "/").rstrip("/")
    if not os.path.isfile(os.path.join(boost_root, "boost/version.hpp")):
        print(f"not a Boost tree: {boost_root}", file=sys.stderr)
        return 1

    seeds = observed_headers(args.ninja_build_dir)
    if not seeds:
        print("ninja -t deps produced no boost headers; was dynarmic built there?",
              file=sys.stderr)
        return 1
    closure, missing = close_over_includes(boost_root, seeds)
    subset = sorted(closure | whole_directories(boost_root))

    total = sum(os.path.getsize(os.path.join(boost_root, r)) for r in subset)
    print(f"{len(seeds)} observed, {len(closure)} after closure, "
          f"{len(subset)} with the macro-driven directories "
          f"({total / 1048576:.1f} MiB)")
    if missing:
        print(f"{len(missing)} unresolved include(s), e.g. {sorted(missing)[:5]}")
    if args.dry_run:
        return 0

    for rel in subset:
        destination = os.path.join(args.out, rel)
        os.makedirs(os.path.dirname(destination), exist_ok=True)
        shutil.copy2(os.path.join(boost_root, rel), destination)
    licence = os.path.join(boost_root, "LICENSE_1_0.txt")
    if os.path.isfile(licence):
        shutil.copy2(licence, os.path.join(args.out, "LICENSE_1_0.txt"))
    with open(os.path.join(args.out, "SUBSET.txt"), "w", encoding="utf-8") as handle:
        handle.write("\n".join(subset) + "\n")
    print(f"wrote {len(subset)} files to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
