#!/usr/bin/env python3
"""Run a notebook's code cells in order, as one script -- the Colab benchmark flow on a Linux box
without Jupyter. IPython's display calls print (or are skipped); matplotlib draws to files.

    OMNI_NB_<PARAM>=... taskset -c 0,1 python3 tools/nb_run.py notebooks/omnidroid_boot_bench.ipynb

Parameters come from the environment as the notebook documents (`OMNI_NB_<NAME>`). Needs pandas,
matplotlib and pillow (a venv is fine).
"""
import json, sys, types

path = sys.argv[1]
nb = json.load(open(path, encoding="utf-8"))

import matplotlib  # noqa: E402
matplotlib.use("Agg")

ipy = types.ModuleType("IPython")
disp = types.ModuleType("IPython.display")
disp.display = lambda *a, **k: [print(x) for x in a]
disp.Image = lambda *a, **k: f"[image {k.get('filename') or (a[0] if a else '')}]"
ipy.display = disp
sys.modules["IPython"] = ipy
sys.modules["IPython.display"] = disp

g = {"__name__": "__main__", "display": disp.display}
for i, cell in enumerate(c for c in nb["cells"] if c["cell_type"] == "code"):
    src = "".join(cell["source"])
    print(f"\n===== cell {i} =====", flush=True)
    exec(compile(src, f"{path}#cell{i}", "exec"), g)
