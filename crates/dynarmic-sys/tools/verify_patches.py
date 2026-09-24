#!/usr/bin/env python3
"""Check that `vendor/dynarmic/` is exactly the pin plus `patches/*.patch`, in order.

The pin cannot be fetched (the upstream is a mirror of a repository that no longer exists, and the
build must not touch the network). The pristine tree is instead the one this repository vendored,
in the commit that vendored it (`PRISTINE_TREE` below, the git tree object of
`crates/dynarmic-sys/vendor/dynarmic` at 64034d4, before patch 0001). Two checks:

1. **pristine + patches == the working tree**, byte for byte: every patch applies (`--check` first)
   on top of the previous ones, and the result is compared with `vendor/dynarmic/`. An edit made in
   the vendored tree but not recorded in a patch fails here -- and only here, which is why the
   pristine tree has to come from history rather than be reconstructed from the tree under test.
2. **the working tree reverses to pristine**: every patch reverse-applies newest-first.

Usage: python3 crates/dynarmic-sys/tools/verify_patches.py
Exit status 0 means verified. Everything happens in git object space against private index files
(`GIT_INDEX_FILE`), so the repository's eol attributes apply to both sides, ignored build outputs
(dynarmic's CMake writes MIG sources into `backend/*/mig/`) are not compared, and neither the real
index nor the working tree is touched.
"""

import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
CRATE = os.path.dirname(HERE)
REPO = os.path.dirname(os.path.dirname(CRATE))
VENDOR_REL = "crates/dynarmic-sys/vendor/dynarmic"
# `git rev-parse 64034d4:crates/dynarmic-sys/vendor/dynarmic`: the vendored pin, before any patch.
PRISTINE_TREE = "272616d92142946104b9fe5ca4a45c93137ecd46"
PATCHES = sorted(
    os.path.join(CRATE, "patches", p)
    for p in os.listdir(os.path.join(CRATE, "patches"))
    if p.endswith(".patch")
)


def git(index, *args, check=True):
    """Run git against a private index file, so the real index is never touched."""
    env = dict(os.environ, GIT_INDEX_FILE=index)
    r = subprocess.run(["git", *args], cwd=REPO, env=env, capture_output=True, text=True)
    if check and r.returncode != 0:
        sys.exit(f"FAILED: git {' '.join(args)}\n{r.stderr}")
    return r.stdout.strip()


def working_tree_hash(tmp):
    """The git tree of `vendor/dynarmic/` as it is on disk now -- tracked files and untracked ones
    that are not ignored, with the repository's eol normalisation -- without touching the real
    index."""
    index = os.path.join(tmp, "work.index")
    # From HEAD, so files that are tracked but match an ignore rule (upstream ships a
    # `SelfTest.vcxproj.user`) stay in; `-A` records deletions too.
    git(index, "read-tree", "HEAD")
    git(index, "add", "-A", "--", VENDOR_REL)
    return git(index, "write-tree", f"--prefix={VENDOR_REL}/")


def main():
    with tempfile.TemporaryDirectory(prefix="od-verify-patches-") as tmp:
        current = working_tree_hash(tmp)

        # 1. pristine (from history) + patches, in order, must be the working tree.
        index = os.path.join(tmp, "pristine.index")
        git(index, "read-tree", PRISTINE_TREE)
        for patch in PATCHES:
            git(index, "apply", "--cached", "--check", patch)
            git(index, "apply", "--cached", patch)
            print(f"ok  pristine + {os.path.basename(patch)}")
        rebuilt = git(index, "write-tree")
        if rebuilt != current:
            diff = git(index, "diff", "--stat", rebuilt, current, check=False)
            sys.exit(f"FAILED: pristine + patches ({rebuilt}) is not the vendored tree ({current}):\n{diff}")

        # 2. the working tree reverses to pristine, newest patch first.
        index = os.path.join(tmp, "reverse.index")
        git(index, "read-tree", current)
        for patch in reversed(PATCHES):
            git(index, "apply", "--cached", "-R", "--check", patch)
            git(index, "apply", "--cached", "-R", patch)
        if git(index, "write-tree") != PRISTINE_TREE:
            sys.exit("FAILED: reversing every patch does not give back the pristine tree")
        print(f"ok  reverse, {len(PATCHES)} patches, back to {PRISTINE_TREE[:12]}")
    print(f"verified: vendor/dynarmic == pin + {len(PATCHES)} patches, in order (tree {current[:12]})")


if __name__ == "__main__":
    main()
