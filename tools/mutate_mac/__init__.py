"""The macOS port's mutation rows (prefix `mac-`).

They live here rather than in `tools/mutate.py`'s own table so that the three ports' tables merge
without touching each other's lines: Windows owns `mutate.py`, and this package is appended to its
`MUTATIONS` by one line there. One module per workstream, each owning its own `ROWS` list, so two
workstreams never edit the same list either.

**Pure data.** Importing this package must not touch the working tree or run anything -- VERIFICATION
process rule 3: an import that applied mutations once already ran a whole suite by accident.

Row shape, as in `mutate.py`: (id, direction, description, file, old, new, command). Commands run on
the macOS host; `python3 tools/mutate.py --only mac-` runs exactly these rows.
"""

from . import cpu, elf, fault, gfx, platform, window

ROWS = cpu.ROWS + platform.ROWS + fault.ROWS + window.ROWS + gfx.ROWS + elf.ROWS
