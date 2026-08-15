#!/usr/bin/env python3
"""Millisecond-precise screen capture for omnidroid.

Instead of polling `adb screencap` on a fixed interval, this attaches to the
instance's QEMU VNC server and observes EVERY completed framebuffer update
through :class:`vncview.RFBClient`'s ``on_frame`` hook. Each update carries the
host monotonic high-resolution instant it completed (``perf_counter_ns``), and
two consecutive updates can never collapse into one frame — so a loading screen
that is shown for only a few milliseconds before the screen goes black is
captured as TWO distinct keyframes with the true elapsed time between them.

Design (why the recv thread only enqueues):
  The RFB receive loop requests the next incremental update *after* the
  ``on_frame`` callback returns. Any heavy work in the callback (downscale,
  diff, PNG encode) would therefore widen the window during which QEMU coalesces
  guest changes into a single update — exactly what would swallow a 1 ms
  transition. So the callback does O(1) work: it hands the already-copied
  framebuffer bytes (and their completion timestamp) to a bounded queue and
  returns immediately. A separate worker thread does the downscale/diff/encode.
  The queue uses blocking puts (back-pressure, never drop): under sustained load
  the recv thread briefly waits rather than discarding a frame that might BE the
  transition. Timestamps are taken at RFB-completion time, so timing stays exact
  even if the worker briefly lags.

Change detection keeps only meaningful frames. Each sampled frame is compared to
the last KEPT keyframe (not the previous raw sample), which makes a small
looping animation (a spinner) invisible — it never drifts far from what was last
saved — while a real page/screen transition, a dialog, or a move into/out of a
black screen crosses the threshold immediately. A black frame is only a VISUAL
observation; whether the app actually CRASHED is decided separately from process
and logcat evidence by the caller (see cmd_capture in omni.py).

This module is deliberately self-contained (stdlib + Pillow + the sibling
vncview) so it works identically from a source checkout and the frozen exe.
"""
import io
import json
import os
import queue
import threading
import time

# vncview is a sibling module (omnidroid/); the frozen exe adds --hidden-import
# vncview, and a checkout runs with the repo root on sys.path (see omni.py).
from omnidroid import vncview


# Change-detection defaults. Kept behaviourally aligned with the agent's adb
# fallback (omni-agent/tools/_emulator_frame_capture.py) so a report reads the
# same whichever provider produced it.
#
# The two scene thresholds are set from MEASURED separation, not taste (see
# tests/test_keyframe_thresholds.py, which pins these cases):
#
#   animating spinner .................. 0.06 % changed / 0.06 mean
#   progress bar, empty -> full ........ 1.13 % changed / 1.21 mean
#   ---------------- scene threshold: 2 % ------------------------------
#   small toast (2 % of screen) ........ 2.33 % changed / 4.56 mean
#   popup dialog (6 % of screen) ....... 6.38 % changed / 13.55 mean
#   full menu/scene change ............. 92.9 % changed / 142.3 mean
#
# It used to sit at 8 % / 14.0 — 7x above the loudest ignorable animation, which
# also silently dropped every popup smaller than ~1/8 of the screen (the 6 %
# dialog above missed on BOTH metrics). 2 % / 4.0 clears the animation noise
# floor and still catches a toast.
#
# The usable band is genuinely narrow (1.13 % .. 2.33 %) because "% of changed
# pixels" is a SIZE metric: a wide thin progress bar and a small dialog move a
# similar pixel count, and nothing here distinguishes them by shape. The
# consequence, accepted deliberately: an animated element larger than ~2 % of the
# screen (a big spinning logo) WILL keyframe. Widening the band needs a different
# metric (e.g. contiguous-region change), not a nudged constant — so
# tests/test_keyframe_thresholds.py asserts the ordering rather than the numbers.
DEFAULT_SAMPLE_SCALE_W = 160
DEFAULT_PIXEL_THRESHOLD = 24        # per-channel delta for a pixel to "change"
DEFAULT_CHANGE_PERCENT = 2.0        # % of changed pixels => scene change
DEFAULT_CHANGE_THRESHOLD = 4.0      # mean abs delta backstop (0-255)
DEFAULT_BLACK_THRESHOLD = 10.0      # mean brightness below => black frame
DEFAULT_MAX_KEYFRAMES = 240
_QUEUE_MAXSIZE = 256                # bounded; blocking put = back-pressure


def _lazy_pil():
    """Import Pillow lazily with a clear message (mirrors vncview.run_viewer)."""
    from PIL import Image, ImageChops, ImageStat
    return Image, ImageChops, ImageStat


def _image_from_bgrx(width, height, bgrx):
    Image, _ImageChops, _ImageStat = _lazy_pil()
    # QEMU (virtio-gpu on this base) delivers R,G,B,x bytes per pixel; PIL's
    # "RGBX" raw decoder reads that straight into an RGB image. Verified against
    # `adb screencap` ground truth (exact match); the old "BGRX" was R/B-swapped.
    return Image.frombytes("RGB", (width, height), bgrx, "raw", "RGBX")


def _frame_metrics(img_a, img_b, pixel_threshold):
    """(mean channel delta, changed-pixel %) between two equal-size RGB images.

    A pixel counts as changed when ANY RGB channel crosses ``pixel_threshold``.
    This keeps a tiny spinner below the scene threshold while catching menus,
    dialogs, and colour-only transitions a single grayscale average would miss.
    """
    from PIL import ImageChops, ImageStat
    diff = ImageChops.difference(img_a, img_b)
    means = ImageStat.Stat(diff).mean
    mean_abs = sum(means) / max(1, len(means))
    pixels = list(diff.getdata())
    if not pixels:
        return 0.0, 0.0
    changed = sum(1 for px in pixels if max(px) >= pixel_threshold)
    return mean_abs, changed * 100.0 / len(pixels)


def _mean_brightness(img):
    from PIL import ImageStat
    return ImageStat.Stat(img.convert("L")).mean[0]


def _wall_clock(epoch_ms):
    """Local wall-clock 'HHMMSS_mmm' for embedding in a keyframe filename.
    Colons are illegal in Windows filenames, so this uses no separators inside
    the time and a millisecond suffix — enough to read the absolute instant a
    frame was captured straight off the filename."""
    secs = epoch_ms // 1000
    ms = int(epoch_ms % 1000)
    lt = time.localtime(secs)
    return f"{lt.tm_hour:02d}{lt.tm_min:02d}{lt.tm_sec:02d}_{ms:03d}"


def _keyframe_filename(index, t_ms, delta_ms, epoch_ms):
    """Encode the WHOLE timing story into the filename so a reader who only sees
    the file list still knows: which frame (index), elapsed since capture start
    (t{t_ms}ms), the gap from the previous KEPT frame (+{delta_ms}ms — 'instant'
    vs 'took time'), and the absolute local instant (w{HHMMSS_mmm}). Nothing in
    the pipeline parses this back out; metadata.json remains the source of truth
    for timing, so the format is free to be human-first."""
    return (f"frame_{index:04d}_t{int(t_ms)}ms_+{int(delta_ms)}ms_"
            f"w{_wall_clock(epoch_ms)}.png")


def _atomic_write_json(path, obj):
    """Write JSON via tmp + os.replace so a reader (the agent polling for live
    frames during a continuous capture) never sees a half-written document."""
    tmp = f"{path}.tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(obj, fh, indent=2, ensure_ascii=False)
        fh.write("\n")
    os.replace(tmp, path)


class KeyframeSelector:
    """Stateful change detector: feed it every sampled frame in order; it decides
    which to KEEP as keyframes. Comparing each frame to the last KEPT frame (not
    the previous raw sample) is what makes a small looping animation invisible
    while a real transition — or a move into/out of a black screen — is kept.

    Pure and framebuffer-source-agnostic (takes a PIL RGB image), so it is unit
    testable without a VNC server and shared by run_capture below.
    """

    def __init__(self, sample_scale_w=DEFAULT_SAMPLE_SCALE_W,
                 pixel_threshold=DEFAULT_PIXEL_THRESHOLD,
                 change_percent=DEFAULT_CHANGE_PERCENT,
                 change_threshold=DEFAULT_CHANGE_THRESHOLD,
                 black_threshold=DEFAULT_BLACK_THRESHOLD):
        self.sample_scale_w = sample_scale_w
        self.pixel_threshold = pixel_threshold
        self.change_percent = change_percent
        self.change_threshold = change_threshold
        self.black_threshold = black_threshold
        self._last_kept_small = None
        self._last_was_black = False

    def _downscale(self, full_rgb):
        w, h = full_rgb.size
        scale_h = max(1, int(h * (self.sample_scale_w / max(1, w))))
        return full_rgb.resize((self.sample_scale_w, scale_h))

    def consider(self, full_rgb):
        """Decide on one frame. Returns a dict of metrics + keep/reason (the
        caller adds t_ms/delta_ms/file and writes the PNG). Advances internal
        state only when the frame is kept."""
        small = self._downscale(full_rgb)
        brightness = _mean_brightness(small)
        is_black = brightness < self.black_threshold
        keep, reason, diff_score, changed_percent = False, None, 0.0, 0.0
        if self._last_kept_small is None:
            keep, reason = True, "baseline"
        else:
            diff_score, changed_percent = _frame_metrics(
                small, self._last_kept_small, self.pixel_threshold)
            if changed_percent >= self.change_percent:
                keep, reason = True, "scene_change"
            elif diff_score >= self.change_threshold:
                keep, reason = True, "mean_diff"
            elif is_black and not self._last_was_black:
                keep, reason = True, "black_transition"
            elif not is_black and self._last_was_black:
                keep, reason = True, "black_exit"
        if keep:
            self._last_kept_small = small
            self._last_was_black = is_black
        return {
            "keep": keep,
            "reason": reason,
            "diff_score": round(diff_score, 3),
            "changed_percent": round(changed_percent, 3),
            "mean_brightness": round(brightness, 2),
            "black_screen": is_black,
        }


class _FrameSink:
    """RFBClient.on_frame target: O(1) enqueue of (bytes, timestamp, seq)."""

    def __init__(self):
        self.q = queue.Queue(maxsize=_QUEUE_MAXSIZE)
        self._stop = threading.Event()

    def on_frame(self, width, height, bgrx, completed_ns, sequence):
        if self._stop.is_set():
            return
        # Blocking put = back-pressure, never drop: a dropped frame could be the
        # very transition we exist to catch. Under sustained load the recv thread
        # waits briefly here instead.
        try:
            self.q.put((width, height, bgrx, completed_ns, sequence), timeout=2.0)
        except queue.Full:
            pass  # 2s of no drain => recorder is stopping; safe to shed.

    def stop(self):
        self._stop.set()


def run_capture(host, port, output_dir, duration_seconds,
                sample_scale_w=DEFAULT_SAMPLE_SCALE_W,
                pixel_threshold=DEFAULT_PIXEL_THRESHOLD,
                change_percent=DEFAULT_CHANGE_PERCENT,
                change_threshold=DEFAULT_CHANGE_THRESHOLD,
                black_threshold=DEFAULT_BLACK_THRESHOLD,
                max_keyframes=DEFAULT_MAX_KEYFRAMES,
                settle_seconds=0.0,
                stop_event=None,
                metadata_path=None,
                enrich=None):
    """Attach to the VNC server at ``host:port`` and record keyframes into
    ``output_dir`` (created if needed).

    Two windowing modes, selected by ``duration_seconds``:

    * **bounded** (``duration_seconds > 0``): observe for that many seconds, then
      return — the classic ``omnidroid capture`` window.
    * **continuous / auto** (``duration_seconds`` is None or <= 0): observe until
      ``stop_event`` is set or the VNC connection ends (the instance stopped).
      This is what powers the always-on auto-screenshot feature: every big
      on-screen change lands as a keyframe with no fixed end time.

    ``stop_event`` (a ``threading.Event``) ends either mode early and cleanly.

    ``metadata_path`` turns on LIVE flushing: metadata.json is rewritten
    atomically after every kept keyframe (and once at the end), so a reader — the
    omni-agent polling its workspace — sees new frames as they happen instead of
    only after the capture returns. ``enrich(meta)`` is called just before each
    flush to let the caller fold in process/logcat annotations live.

    Returns the final metadata dict. Raises on connect failure so the caller can
    surface a typed error.
    """
    Image, _c, _s = _lazy_pil()  # fail early + clearly if Pillow is missing
    os.makedirs(output_dir, exist_ok=True)

    try:
        duration_val = float(duration_seconds) if duration_seconds is not None else 0.0
    except (TypeError, ValueError):
        duration_val = 0.0
    unbounded = duration_val <= 0

    sink = _FrameSink()
    client = vncview.RFBClient(host, port, on_frame=sink.on_frame)
    client.connect()  # RFB handshake; requests the first (full) update

    # t=0 is "capture started": set both clocks at the same instant so frame
    # t_ms (monotonic) and logcat epoch correlation (wall clock) share an origin.
    start_ns = time.perf_counter_ns()
    start_epoch_ms = time.time_ns() // 1_000_000
    duration_ns = max(0, int(duration_val * 1_000_000_000))

    net = threading.Thread(target=client.run, name="rfb-recv", daemon=True)
    net.start()

    keyframes = []
    warnings = []
    selector = KeyframeSelector(sample_scale_w, pixel_threshold, change_percent,
                                change_threshold, black_threshold)
    state = {"samples": 0, "index": 0, "last_kept_t_ms": None,
             "first_update_ns": None}

    def _build_metadata(final):
        # Elapsed clock drives duration in continuous mode (no fixed window);
        # bounded mode reports the requested duration for stable expectations.
        if unbounded:
            duration_ms = max(0, int(round(
                (time.perf_counter_ns() - start_ns) / 1_000_000)))
        else:
            duration_ms = int(round(duration_val * 1000))
        meta = {
            "metadata_version": 2,
            "auto": unbounded,
            "mode": "continuous" if unbounded else "bounded",
            "running": not final,
            "duration_ms": duration_ms,
            "duration_seconds": round(duration_ms / 1000.0, 3),
            "sample_scale_w": sample_scale_w,
            "pixel_threshold": pixel_threshold,
            "change_percent": change_percent,
            "change_threshold": change_threshold,
            "black_threshold": black_threshold,
            "samples_taken": state["samples"],
            "updates_seen": state["samples"],
            "keyframe_count": len(keyframes),
            "keyframes": keyframes,
            "capture_provider": "omnidroid",
            "clock": "monotonic",
            "timestamp_precision": "milliseconds",
            "coverage": "vnc_framebuffer",
            "start_epoch_ms": start_epoch_ms,
            "vnc_host": host,
            "vnc_port": port,
            "warnings": warnings,
        }
        if enrich is not None:
            try:
                enrich(meta)
            except Exception as e:  # noqa: BLE001 — enrichment must never kill capture
                warnings.append(f"enrich failed: {e}")
        return meta

    def _flush(final=False):
        if metadata_path is None:
            return
        try:
            _atomic_write_json(metadata_path, _build_metadata(final))
        except Exception as e:  # noqa: BLE001 — a flush hiccup must not stop capture
            warnings.append(f"metadata flush failed: {e}")

    def _process(width, height, bgrx, completed_ns, sequence):
        state["samples"] += 1
        if state["first_update_ns"] is None:
            state["first_update_ns"] = completed_ns
        t_ms = max(0, int(round((completed_ns - start_ns) / 1_000_000)))
        try:
            full = _image_from_bgrx(width, height, bgrx)
        except Exception as e:  # noqa: BLE001
            warnings.append(f"decode failed at t={t_ms}ms: {e}")
            return
        d = selector.consider(full)
        if not d["keep"]:
            return
        if len(keyframes) >= max_keyframes:
            if not any("max_keyframes" in w for w in warnings):
                warnings.append(
                    f"max_keyframes={max_keyframes} reached; later scene "
                    "changes were counted but not saved")
            state["last_kept_t_ms"] = t_ms
            return

        prev_t = state["last_kept_t_ms"]
        delta_ms = 0 if prev_t is None else max(0, t_ms - prev_t)
        frame_epoch_ms = start_epoch_ms + t_ms
        fname = _keyframe_filename(state["index"], t_ms, delta_ms, frame_epoch_ms)
        try:
            full.save(os.path.join(output_dir, fname))
        except Exception as e:  # noqa: BLE001
            warnings.append(f"save failed at t={t_ms}ms: {e}")
            return
        keyframes.append({
            "index": state["index"],
            "t_ms": t_ms,
            "elapsed_ms": t_ms,
            "t_seconds": round(t_ms / 1000.0, 3),
            "delta_ms": delta_ms,
            "wall_epoch_ms": frame_epoch_ms,
            "wall_clock": _wall_clock(frame_epoch_ms),
            "file": fname,
            "diff_score": d["diff_score"],
            "changed_percent": d["changed_percent"],
            "mean_brightness": d["mean_brightness"],
            "black_screen": d["black_screen"],
            "reason": d["reason"] or "scene_change",
            "rfb_sequence": sequence,
            "app_state": "unknown",
            "pid": None,
            "crash": False,
        })
        state["last_kept_t_ms"] = t_ms
        state["index"] += 1
        # Live flush: a reader tailing metadata.json sees this frame immediately.
        _flush(final=False)

    def _stopping():
        return stop_event is not None and stop_event.is_set()

    # Write an initial (empty) metadata so a reader that races the first frame
    # still finds a valid, well-formed document.
    _flush(final=False)

    # Drain loop: pull frames the recv thread enqueues until the window elapses
    # (bounded) or a stop is requested / the connection ends (continuous).
    try:
        while True:
            if not unbounded and time.perf_counter_ns() - start_ns >= duration_ns:
                break
            if _stopping():
                break
            if client.closed.is_set():
                if client.error:
                    warnings.append(f"vnc connection ended early: {client.error}")
                break
            if unbounded:
                timeout = 0.5
            else:
                remaining_ns = duration_ns - (time.perf_counter_ns() - start_ns)
                timeout = min(0.5, max(0.01, remaining_ns / 1e9))
            try:
                item = sink.q.get(timeout=timeout)
            except queue.Empty:
                continue
            _process(*item)
        # Window ended: stop intake, then drain what already arrived so a
        # last-instant transition still lands (its timestamp is already fixed).
        sink.stop()
        deadline = time.perf_counter_ns() + 500_000_000
        while time.perf_counter_ns() < deadline:
            try:
                item = sink.q.get_nowait()
            except queue.Empty:
                break
            _process(*item)
    finally:
        sink.stop()
        client.close()

    metadata = _build_metadata(final=True)
    _flush(final=True)
    return metadata
