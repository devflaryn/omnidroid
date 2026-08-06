# tests/test_omni_input.py
import json
import pathlib
import subprocess
import sys

import pytest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from tools import omni_input  # noqa: E402


# ------------------------------------------------------------ text encoding --

def test_spaces_become_the_input_text_space_token():
    # Android's `input text` is the only consumer of %s; a raw space would be
    # split into separate args by the guest shell and silently truncate.
    # No quoting is added when nothing needs it — only the space is rewritten.
    assert omni_input.encode_text("hello world") == "hello%sworld"


@pytest.mark.parametrize("raw", [
    "a'b", 'a"b', "a;rm -rf /", "a$(id)", "a`id`", "a|b", "a&b", "a>b",
])
def test_shell_metacharacters_cannot_escape_the_quoting(raw):
    # The encoded value is pasted into a guest shell line, so anything that
    # could terminate the quoting would run as a command in the guest.
    enc = omni_input.encode_text(raw)
    assert subprocess.list2cmdline  # sanity: stdlib present
    # Round-tripping through a POSIX shell must yield the original (with the
    # %s token standing in for spaces).
    out = subprocess.run(["sh", "-c", f"printf %s {enc}"],
                         capture_output=True, text=True, timeout=10).stdout
    assert out == raw.replace(" ", "%s")


# ---------------------------------------------------------------- ui parsing --

DUMP = """<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>
<hierarchy rotation="0">
 <node index="0" text="" resource-id="" class="android.widget.FrameLayout"
       package="p" content-desc="" clickable="false" enabled="true"
       focused="false" bounds="[0,0][1280,800]" />
 <node index="1" text="Download path" resource-id="com.x:id/dl_text"
       class="android.widget.EditText" package="p" content-desc=""
       clickable="true" enabled="true" focused="true"
       bounds="[48,402][1232,444]" />
 <node index="2" text="" resource-id="" class="android.widget.ImageButton"
       package="p" content-desc="Navigate up" clickable="true" enabled="true"
       focused="false" bounds="[16,36][56,76]" />
</hierarchy>"""


class FakeDev:
    """Stands in for Device: returns the dump for the uiautomator call."""
    def __init__(self, xml=DUMP):
        self.xml = xml

    def sh(self, script, timeout=60):
        return subprocess.CompletedProcess([], 0, stdout=self.xml, stderr="")


def test_nodes_get_tap_centres_from_bounds():
    nodes = omni_input.ui_nodes(FakeDev())
    edit = next(n for n in nodes if n["cls"] == "EditText")
    assert edit["center"] == [640, 423]
    assert edit["id"] == "dl_text"          # short id, not the full package path
    assert edit["focused"] is True


def test_unlabelled_layout_nodes_are_filtered_out_by_default():
    nodes = omni_input.ui_nodes(FakeDev())
    kept = omni_input.interesting(nodes)
    assert len(nodes) == 3 and len(kept) == 2   # the bare FrameLayout drops out


def test_dump_preceded_by_uiautomator_chatter_still_parses():
    # `uiautomator dump` prints "UI hierchary dumped to: ..." before the XML
    # when stdout is not fully suppressed; parsing must start at <hierarchy.
    noisy = "UI hierchary dumped to: /data/local/tmp/ui.xml\n" + DUMP
    assert len(omni_input.ui_nodes(FakeDev(noisy))) == 3


def test_a_dump_without_a_hierarchy_is_an_error_not_an_empty_screen():
    with pytest.raises(SystemExit):
        omni_input.ui_nodes(FakeDev("ERROR: could not get idle state."))


# ------------------------------------------------------------------ matching --

def test_exact_text_match_beats_substring_candidates():
    nodes = [{"text": "Update", "desc": "", "id": "", "full_id": ""},
             {"text": "Update channel", "desc": "", "id": "", "full_id": ""}]
    assert omni_input.match_nodes(nodes, text="Update") == [nodes[0]]


def test_substring_match_is_case_insensitive_when_nothing_is_exact():
    nodes = [{"text": "Download path", "desc": "", "id": "", "full_id": ""}]
    assert omni_input.match_nodes(nodes, text="download") == nodes


def test_resource_id_matches_short_or_fully_qualified():
    nodes = [{"text": "", "desc": "", "id": "dl_text", "full_id": "com.x:id/dl_text"}]
    assert omni_input.match_nodes(nodes, rid="dl_text") == nodes
    assert omni_input.match_nodes(nodes, rid="com.x:id/dl_text") == nodes


# ------------------------------------------------------------ stale instance --

def _write_run(tmp_path, name, pid, port=16001):
    d = tmp_path / "runtime" / name
    d.mkdir(parents=True)
    (d / "run.json").write_text(json.dumps({"pid": pid, "adb_port": port}))


def test_a_dead_pid_means_the_port_is_stale_and_the_instance_is_not_live(
        tmp_path, monkeypatch):
    monkeypatch.setattr(omni_input, "omni_root", lambda: tmp_path)
    _write_run(tmp_path, "acc0", 999999)
    monkeypatch.setattr(omni_input, "_pid_is_this_instance",
                        lambda pid, name: False)
    assert omni_input.running_instances() == []


def test_a_live_pid_belonging_to_another_instance_is_rejected(monkeypatch):
    # The recycled-port failure mode: the pid is alive, but it is a DIFFERENT
    # VM, so its adb port must never be treated as ours.
    monkeypatch.setattr(omni_input.subprocess, "run",
                        lambda *a, **k: subprocess.CompletedProcess(
                            [], 0, stdout="qemu-system-aarch64 -name omni-other\n",
                            stderr=""))
    assert omni_input._pid_is_this_instance(123, "acc0") is False
    assert omni_input._pid_is_this_instance(123, "other") is True


def test_resolve_refuses_to_guess_when_several_instances_are_live(
        tmp_path, monkeypatch):
    monkeypatch.setattr(omni_input, "omni_root", lambda: tmp_path)
    _write_run(tmp_path, "acc0", 111, 16001)
    _write_run(tmp_path, "acc1", 222, 16002)
    monkeypatch.setattr(omni_input, "_pid_is_this_instance",
                        lambda pid, name: True)
    with pytest.raises(SystemExit):
        omni_input.resolve_instance(None)
    assert omni_input.resolve_instance("acc1") == ("acc1", 16002)


def test_a_single_live_instance_is_selected_without_a_name(
        tmp_path, monkeypatch):
    monkeypatch.setattr(omni_input, "omni_root", lambda: tmp_path)
    _write_run(tmp_path, "acc0", 111, 16001)
    monkeypatch.setattr(omni_input, "_pid_is_this_instance",
                        lambda pid, name: True)
    assert omni_input.resolve_instance(None) == ("acc0", 16001)


# --------------------------------------------------------------------- grid --

def test_grid_keeps_the_image_pixel_for_pixel_addressable(tmp_path):
    # A coordinate read off the overlay is sent straight to `input tap`, so the
    # overlay must not resize or crop what it annotates.
    from PIL import Image
    p = tmp_path / "s.png"
    Image.new("RGB", (1280, 800), (10, 10, 10)).save(p)
    omni_input.draw_grid(p, 100)
    assert Image.open(p).size == (1280, 800)


def test_grid_actually_marks_the_step_lines(tmp_path):
    from PIL import Image
    p = tmp_path / "s.png"
    Image.new("RGB", (400, 400), (0, 0, 0)).save(p)
    omni_input.draw_grid(p, 100)
    px = Image.open(p).convert("RGB").load()
    # A ruled column is drawn at x=100; x=150 stays untouched background.
    assert px[100, 250] != (0, 0, 0)
    assert px[150, 250] == (0, 0, 0)


# -------------------------------------------------------------------- input --

def test_a_nonzero_input_exit_is_an_error_not_a_silent_success(monkeypatch):
    # `input` failing (e.g. no touchscreen source) must not be reported as a
    # delivered tap — that is how a drive loop ends up acting on a dead screen.
    dev = omni_input.Device("acc0", 16001)
    monkeypatch.setattr(dev, "sh", lambda *a, **k: subprocess.CompletedProcess(
        [], 1, stdout="", stderr="Error: Unknown source"))
    with pytest.raises(SystemExit):
        dev.input("tap", "1", "2")
