import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from omnidroid import engine  # noqa: E402

SNAPSHOT = pathlib.Path(__file__).with_name("engine_public_names.json")

# Names the test suite and cli.py reach for via attribute access. Anything
# in this set MUST stay resolvable as engine.<name> after every extraction.
def _current_names():
    return sorted(n for n in dir(engine) if not n.startswith("__"))

def test_facade_exposes_every_snapshot_name():
    if not SNAPSHOT.exists():
        SNAPSHOT.write_text(json.dumps(_current_names(), indent=2))
    expected = set(json.loads(SNAPSHOT.read_text()))
    missing = expected - set(_current_names())
    assert not missing, f"facade dropped names: {sorted(missing)}"
