"""The Linux port's mutation rows, run through `tools/mutate.py`'s own harness.

    flock ~/odb/build.lock python3 tools/mutate_linux.py              # every lnx- row
    flock ~/odb/build.lock python3 tools/mutate_linux.py --only lnx-vm   # rows: tools/lnx_rows/*.py
    python3 tools/mutate_linux.py --list

Why a second file rather than rows appended to `tools/mutate.py`: three machines are working from
one base (`base-0923-night`) and are merged afterwards. `mutate.py`'s table is appended to by the
Windows branch every day, so rows added to it here would conflict on every merge, textually, at
the list terminator. This file imports the harness -- pre-flight, restore, retry, the `caught` /
`MISS` classification, the duplicate-id refusal -- unchanged, and swaps in its own table. Importing
`mutate` is safe: its run loop is behind `if __name__ == "__main__"` (VERIFICATION rule 3's third
angle is why that was checked before relying on it).

Every id starts `lnx-`, so the two tables can later be concatenated without a collision, and the
duplicate check below runs over both to prove it now rather than at merge time.

Commands run `--release`, unlike most of `mutate.py`'s: on this 7 GB, 4-core host a second
(debug) profile of the whole workspace is minutes of build and gigabytes of disk per command, and
the gate these rows defend is run in release. Where a defect is only visible in debug (VERIFICATION
entry 3) the row says so and names a debug command instead.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mutate  # noqa: E402  (the harness; see the module docstring)

import importlib.util  # noqa: E402

ROWS_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "lnx_rows")


def load_rows():
    """Every `ROWS` list in `tools/lnx_rows/*.py`, in file-name order.

    One file per area (`build.py`, `vm.py`, `net.py`, ...) so that the port's parallel workers
    each append to a file nobody else writes -- the same reason this table is not in `mutate.py`.
    A file is imported as a module; like `mutate.py` itself, it must do nothing at import time
    but define `ROWS` (and constants it uses).
    """
    rows = []
    for name in sorted(os.listdir(ROWS_DIR)):
        if not name.endswith(".py") or name.startswith("_"):
            continue
        spec = importlib.util.spec_from_file_location(f"lnx_rows_{name[:-3]}",
                                                      os.path.join(ROWS_DIR, name))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        rows.extend(module.ROWS)
    return rows


def main():
    rows = load_rows()
    ids = [row[0] for row in rows]
    bad = [i for i in ids if not i.startswith("lnx-")]
    if bad:
        print(f"{len(bad)} Linux row id(s) without the lnx- prefix: {', '.join(bad)}")
        return 2
    theirs = {row[0] for row in mutate.MUTATIONS}
    clash = sorted(set(ids) & theirs)
    if clash:
        print(f"{len(clash)} Linux row id(s) already used in tools/mutate.py: {', '.join(clash)}")
        return 2
    mutate.MUTATIONS = rows
    return mutate.main()


if __name__ == "__main__":
    sys.exit(main())
