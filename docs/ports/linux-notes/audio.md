# Linux port notes: audio (ALSA behind the AAudio seam)

Branch `lnx-audio`. Host: Ubuntu 26.04, i5-4460, HDA Intel PCH (ALC887-VD); PipeWire running in
the owner's session (pipewire, wireplumber, pipewire-pulse); ALSA `default` = PipeWire's ALSA plugin.
alsa-lib 1.2.15.3. **Silence only** was written in every test; nothing here was listened to.

## What is implemented

`crates/omni-platform/src/audio/linux.rs`: one playback PCM on `"default"`, hand-declared FFI under
`#[link(name = "asound")]` (no `-sys` crate, nothing fetched at build time; needs `libasound.so` to
build -- `libasound2-dev` -- and `libasound.so.2` to run). No `Cargo.toml` change was needed.

| seam call | ALSA |
|---|---|
| `open(buffer_frames)` | `snd_pcm_open("default", PLAYBACK, SND_PCM_NONBLOCK)`; hw params `RW_INTERLEAVED`, `FLOAT` (host-endian), `set_channels_near(2)`, `set_rate_near(48000)` then `set_rate(rate, 0)` exactly (a fractional rate is refused by ALSA, not rounded here), `set_period_size_near(10 ms)`, `set_buffer_size_min(asked)` + `set_buffer_size_first`; `snd_pcm_hw_params`; then **`snd_pcm_hw_params_current` + `get_access/format/channels/rate/period_size/buffer_size`** -- what is reported is what was granted. sw params: `avail_min` = period, `start_threshold` = boundary, `stop_threshold` = buffer |
| `format / buffer_frames / period_frames` | the granted values read back above |
| `writable_frames` | `snd_pcm_avail` (not `avail_update`: `avail` hw-syncs first; `avail_update` answers from the last position the driver reported) |
| `wait_writable(frames, timeout)` | started: `avail_min` set to `frames` (at least a period, at most the buffer; `snd_pcm_sw_params` only when it changes), then `snd_pcm_wait(ms)` to a deadline, timeout rounded up, capped at `INT_MAX`, never `-1`; a wake with less than `frames` free sleeps the missing frames' play time before polling again. Not started: the timeout is slept out (see "Differences") |
| `write` | `snd_pcm_writei` until every frame is in; no room (`-EAGAIN` or a 0 count) waits with `snd_pcm_wait` for at most a buffer's time + 100 ms, then `AudioError::Alsa { api: "snd_pcm_writei", errno: EAGAIN }` |
| `start` | `snd_pcm_pause(0)` from paused; `snd_pcm_start` from prepared **if anything is queued**, else deferred to the first write |
| `stop` | `snd_pcm_pause(1)`: the queue stays, nothing drains (WASAPI `Stop`'s semantics; not `snd_pcm_drop`, which discards) |
| `-EPIPE` (xrun) | `snd_pcm_prepare`, `Recoveries::xruns += 1`; the buffer then reads empty (all writable), as WASAPI's padding does after an underrun; a running stream is restarted by the next write |
| `-ESTRPIPE` (suspend) | `snd_pcm_resume` while `-EAGAIN` (<= 1 s), then `snd_pcm_prepare` if it cannot resume; `Recoveries::suspends += 1` |
| every other failure | `AudioError::Alsa { operation, api, errno, description: snd_strerror }` |

`snd_pcm_open` failing with `ENOENT`/`ENODEV` is `AudioError::NoDevice`; `EBUSY` and the rest are
`AudioError::Alsa`.

## Why ALSA and not PipeWire's native API

* ALSA's PCM API is already pull-shaped and maps onto the seam call for call; `pw_stream` is
  callback-driven and would need a ring buffer between its process callback and `write` (the work
  macOS has to do for Core Audio). PipeWire's ALSA plugin *is* that ring.
* The one concrete thing measured against ALSA: **it has no mix format.** MEASURED,
  `aplay --dump-hw-params -D default`: the plugin offers `RATE: [1 384000]`, `CHANNELS: [1 128]`,
  and left to itself `snd_pcm_hw_params` picks the first of each range (1 Hz mono). The card itself
  (`hw:CARD=PCH,DEV=0`) offers `RATE: [44100 192000]`, `FORMAT: S16_LE S32_LE`, no default either.
  So "the device's own format" is a **request**, 48 kHz stereo, chosen because PipeWire's graph
  runs at 48 kHz here (MEASURED, `pw-metadata -n settings`: `clock.rate 48000`,
  `clock.allowed-rates [ 48000 ]`), so the server converts nothing on this host. On a host whose
  graph runs at another rate PipeWire resamples, outside the seam, and the rate reported is still
  the true rate of the stream the seam writes. Reading the graph's rate would need PipeWire's
  registry/metadata API (and its `spa_interface` vtables), a much larger binding for one number;
  that is the upgrade path if a host shows it matters.
* Without a server the same code reaches the card: MEASURED below via `plughw:`.

## Measured (silence; all figures from the live tests, method stated)

* **Granted format, `default` (PipeWire)**: 48,000 Hz, 2 channels, period 480 frames; buffer 960
  for `open(0)`, 4,800 for `open(4800)`, 24,000 for `open(24000)` (exactly as asked).
  A request of 1,000,000 Hz x 200 channels was granted **384,000 Hz x 128** (buffer 7,680, period
  3,840) -- and that is what `format()` reported.
* **Real-time consumption**: a stream kept fed through `wait_writable(100 ms)` + `write`, consumed
  frames (written - queued) between two readings 2.0 s apart, after 300 ms of settling:
  47,999 / 48,000 / 48,000 / 48,000 / 48,038 frames/s (5 runs; 216-217 waits each; 0 xruns).
  The readings move in PipeWire quanta (512 frames here), so a 2 s window resolves ~0.5 %.
  "Consumed" through PipeWire means taken by the server's graph, which the card's clock paces.
* **Without the server**, `plughw:CARD=PCH,DEV=0` (alsa-lib's own conversion over the card; what a
  server-less host's `default` is): 48,000 Hz x2, buffer 9,600, period 480; 48,002 / 47,999 /
  48,002 frames/s over 1.21 s (3 runs), 0 xruns. `hw:CARD=PCH,DEV=0` refuses float at
  `snd_pcm_hw_params_set_format(SND_PCM_FORMAT_FLOAT)`, errno 22 (EINVAL) -- the seam's
  "no conversion here" refusal, by name.
* **Wake cadence** (running, buffer 4,800, period 480): a refilled stream's `wait_writable(2 s)`
  woke after 10.0-10.7 ms with 512 frames free (5 wakes per run, 5 runs; first of a run sometimes
  5-8 ms). PipeWire's quantum here is 512 frames (10.67 ms), not the granted 480-frame period.
* **Timeouts**: unstarted stream, `wait_writable(100 ms)`: 100.06-100.11 ms, empty or full (5 runs).
  Running stream with a 200 ms period (9,600 frames) and a full 1 s buffer, `wait_writable(20 ms)`:
  20.06-20.85 ms (5 waits per run, 3 runs).
* **Xruns**: a 4,800-frame stream left unfed for 4 buffers' time reads all-free and counts
  `xruns: 1` (found by `snd_pcm_avail`); refilled, it drains again 2-4 ms later; unfed again,
  `wait_writable` finds the second (`snd_pcm_wait` -> `-EPIPE`), `xruns: 2`. 5 runs.
* **Stop/start**: after 100 ms of playing, `stop` held 4,608-4,864 frames free and stayed there for
  150 ms; `start` drained on to 9,216-9,728 of 24,000 in 100 ms (queue kept, not discarded). An
  empty stream accepted `start` and began draining 2-6 ms after its first write, 0 xruns.
* **No room**: a 480-frame write into a full, unstarted 4,800-frame buffer (backend called
  directly; `mod.rs` refuses it first as `TooManyFrames`) failed `Alsa { snd_pcm_writei, errno 11 }`
  after 200.25 ms, for a 200 ms bound. Non-blocking `snd_pcm_writei` with no room answered **0**,
  not `-EAGAIN`, through both the plugin and `plughw` (C probe) -- handled as no room.

## Differences from the Windows backend, deliberate

* **An unstarted stream's wait sleeps its timeout out.** ALSA's poll returns at once on a prepared
  stream with room (avail >= avail_min); WASAPI's event does not fire until the stream is started,
  and the seam documents that. Nothing consumes an unstarted stream, so no period can free.
* **A running wait returns at once if the frames asked for are already free** (poll semantics),
  where WASAPI waits for the next period event. The wait used to be for *a period* whatever the
  caller could use, on the reasoning that "after a full write less than a period is free". That
  is false for AAudio's feeding loop, which writes only up to the stream's buffer size -- see
  "The feeding thread that spun" below.
* **Empty start is deferred** to the first write: the kernel refuses `snd_pcm_start` on an empty
  playback stream (`-EPIPE`, `snd_pcm_pre_start`); WASAPI starts it. PipeWire's plugin also starts
  it, which is why mutation row B3 is caught only by the card test.
* **Recoveries are counted** (`AudioOutput::recoveries()`, Linux-only): WASAPI shared mode has no
  xrun count, so this is not on the portable surface. `omni-android`'s `getXRunCount` keeps its own
  heuristic (`writable >= buffer_frames` after priming), which this backend's recovery satisfies:
  after an xrun the buffer reads all-free.

## Tests and exit codes

```text
~/odb/cargo-locked test -p omni-platform --release --lib audio::                     # 13 pass, 4 ignored (live) -> exit 0
OMNI_AUDIO_LIVE_TESTS=1 OMNI_AUDIO_HW_CARD=PCH ~/odb/cargo-locked test -p omni-platform --release --lib audio:: -- --include-ignored --test-threads=1   # 17 pass -> exit 0
OMNI_AUDIO_LIVE_TESTS=1 ~/odb/cargo-locked test -p omni-platform --release --test audio_live_linux -- --ignored --test-threads=1   # 5 pass -> exit 0
OMNI_AUDIO_LIVE_TESTS=1 ~/odb/cargo-locked test -p omni-platform --release --test audio_live -- --ignored --test-threads=1         # 3 pass -> exit 0 (the Windows contract test, unchanged, on Linux)
```

`tests/audio_live_linux.rs` and the live unit tests use `audio_live.rs`'s gate: `#[ignore]`d, and
run with `--ignored` without `OMNI_AUDIO_LIVE_TESTS=1` they fail. `hw_card_*` needs
`OMNI_AUDIO_HW_CARD` too (and the card not held by the server; it retries 10 s on `EBUSY`).

## Mutation rows (`tools/lnx_rows/audio.py`)

Command (every row): the audio unit tests, the gated live unit tests (`--skip hw_card`),
`tests/audio_live_linux.rs` and `tests/audio_live.rs`, each to its own exit code (`sh -c`), so the
caught list names every test that noticed. B3 runs the `hw_card` test instead (`OMNI_AUDIO_HW_CARD=PCH`,
this host's card).

**18/18 caught** (`flock ~/odb/build.lock python3 tools/mutate_linux.py --only lnx-audio`, third
run; pre-flight 19/19 patterns, 2/2 commands pass on the clean tree; tree `git diff --exit-code`
clean afterwards). `lnx-audio-A12` (open the PCM blocking) was in that run as a 19th row and was
**NOT CAUGHT**; it was then removed from the table (see below), leaving 18.

| row | mutation | caught by |
|---|---|---|
| A1 | `writable_frames` answers the whole buffer, ignoring `snd_pcm_avail` | 7 tests incl. `a_running_wait_...`, `a_fed_stream_...`, `an_xrun_...` |
| A2 | xrun recovered, not counted | `an_xrun_is_prepared_and_counted`, `an_xrun_is_recovered_counted_and_played_through` |
| A3 | xrun counted, no `snd_pcm_prepare` | 3 tests |
| A4 | format reported as asked, not granted | `the_format_reported_is_the_one_granted_not_the_one_asked_for` |
| A5 | running wait passes `-1` to `snd_pcm_wait` | `a_running_wait_returns_at_its_timeout_when_no_period_has_freed` |
| A6 | unstarted wait returns at once | `wait_writable_runs_to_its_timeout_...`, `audio_live::a_started_stream_...` |
| A7 | `stop` does not pause | `stop_holds_the_queue_and_start_plays_it`, `audio_live::a_started_stream_...` |
| A8 | a write does not restart a recovered running stream | `an_xrun_...`, `stop_holds_...` (empty start) |
| A9 | suspend not counted | two suspend unit tests |
| A10 | wait timeout rounds down | `timeouts_round_up_...` |
| A11 | `start_threshold` 1 (writes auto-start) | 3 tests |
| A13 | a 0-frame `snd_pcm_writei` not treated as no room | `a_write_with_no_room_fails_by_name_instead_of_blocking` |
| B1 | every failure treated as an xrun | `any_other_failure_is_returned_...` |
| B2 | xrun counted before its prepare succeeds | `an_xrun_whose_prepare_fails_...` |
| B3 | `snd_pcm_start` even with nothing queued | `hw_card_...` only (see below) |
| B4 | `writable_frames` holds a period back | 9 tests |
| B5 | a running wait sleeps its timeout too | `wait_writable_...`, `audio_live::a_started_stream_...` |
| B6 | prepare after a successful resume | `a_suspend_resumes_through_eagain_and_is_counted` |

What the runs taught, recorded because each changed the code:

* **Run 1 (17 rows): 16/17, and one row took 1963 s.** A1 held the harness for over half an hour
  (the build lock with it -- other workers queued) until the test binary was killed by hand:
  `write` looped without end on a **0-frame** `snd_pcm_writei` into a full, unstarted buffer.
  MEASURED with a C probe: ALSA answers 0, immediately, in blocking *and* non-blocking mode,
  through PipeWire's plugin and through `plughw`. Fixed by treating 0 (and `-EAGAIN`) as no room
  with a bounded `snd_pcm_wait` and a named `EAGAIN` failure; row A13 pins it. The first account
  of this (commit 2523b95) blamed blocking mode; A12 -- reverting non-blocking mode -- was NOT
  CAUGHT, and a blocking probe answered 0 too, so that account was wrong and is corrected in
  dd90e51. Non-blocking mode is kept as an unmeasured guard (a running stream whose device stops
  draining is the case it covers, and it could not be produced here).
* **B3 was NOT CAUGHT through PipeWire**: its plugin accepts `snd_pcm_start` on an empty stream,
  so the deferral is invisible there. The kernel refuses it (`-EPIPE`), so the `hw_card` test
  now starts an empty `plughw` stream, and B3 runs that test: caught.
* The harness runs mutated live tests that can hang; a watchdog script killed any of this
  worktree's test binaries alive > 120 s during runs 2-3 (it killed none in run 3).

## The feeding thread that spun (2026-09-25)

**MEASURED in the Pet Simulator 99 world** (i5-4460, `OMNI_PERF` sampler): one guest thread (g70,
FMOD's AAudio data-callback thread) at 88-96 % of a core for the whole session, every sample "in a
handler" charged to `sem_post` (the last import the thread made -- FMOD's callback posts its
mixer's semaphore), ~45 % in the executable and ~35 % in the kernel, ~0 guest instructions.
Windows showed no such thread.

**Cause.** FMOD's stream, from the session's own AAudio report: burst **480** (the ALSA period),
capacity **9,120** (FMOD asks `setBufferCapacityInFrames(9120)` after a probe answered 960), buffer
size **1,440** (`setBufferSizeInFrames`). The callback thread may only ask for a burst while the
host holds at most `1440 - 480` frames, i.e. with at least `9120 - 1440 + 480 = 8,160` free. It
waited with `wait_writable(50 ms)`, which on ALSA is `snd_pcm_wait` with `avail_min` one period --
**level-triggered**, and with at least 7,680 frames always free it answered at once, every time.
Nothing was written, and the loop asked again: `snd_pcm_wait` + `snd_pcm_avail` flat out. The
burst is not larger than the period (both 480); the buffer size far below the capacity is what
does it. WASAPI's event and Core Audio's semaphore fire once per period, so the same loop slept
there. The unprimed branch had a second spin of the same shape: a stream started again after a
pause, whose host stream kept its queue, found no room for a burst and never started the host,
so nothing ever drained -- silence and a whole core, on every backend.

**Fix.** The seam's `wait_writable` takes the frames the caller needs free and every backend waits
until that much is (ALSA: `avail_min`; WASAPI and Core Audio: keep waiting on the per-period wake
to the same deadline); AAudio waits for `buffer - size + burst` (`frames_wanted`), and starts a
host that already holds frames on its first pass. With the buffer size equal to the capacity that
is one burst: the old behaviour.

**Measured, this host (PipeWire, 48 kHz, period 480), before -> after:**

* `tests/audio_live.rs` `a_feeder_below_the_buffer_waits_for_its_burst_rather_than_spinning`
  (buffer 9,600, size 1,440, burst 480, 3 s): feeder thread CPU **2.999 s (99.98 %) -> 4.5 ms
  (0.15 %)**; waits **18,536,124 -> 301**; bursts 299 -> 301 (100/s both). "Before" is the same
  test with the old threshold (`wait_writable(0, ..)`, i.e. `avail_min` one period).
* `omni-android` `tests/aaudio.rs` `fmods_stream_on_the_host_device_is_fed_at_its_rate_without_spinning`
  (FMOD's own sequence through `PlatformOutput`, capacity 9,120, size 1,440, 2 s): data-callback
  thread **1.993 s (99.67 %) -> 8.3 ms (0.41 %)**; waits for 200 callbacks **9,655,152 -> 200**;
  100.0 callbacks/s both. "Before" is mutation `feed-A1` (the loop waits for a burst's room).
* PipeWire honours a raised `avail_min`: a wait for 1,920 frames came back with 2,048 free after
  39.5-42.7 ms. With `avail_min` forced back to a period behind the backend's back (a host that
  ignores it), a wait for 3,840 frames came back with 3,840 after 77-80 ms using 19-31 us of CPU.
* Windows (WASAPI, 44.1 kHz, period 448) and macOS (Core Audio, 48 kHz, period 512): the same
  feeder test 0.00 % (below the 15.6 ms tick) and 0.16 %; the live AAudio test on Windows 0.78 %
  (one tick in 2 s), 98.5 callbacks/s for 98.4.

**Mutation rows** (all run on the committed tree): Windows `feed-A1..A4, B1, B2` **6/6 caught**
(`python tools/mutate.py --only feed-`: the paced double, the `frames_wanted` unit test, and the
WASAPI live tests); Linux `lnx-feed-A1..A3, B1, B2` **5/5 caught** (the two ALSA live unit tests
above), and `lnx-audio` re-run with A5 and B4 re-anchored **18/18 caught**; macOS `mac-feed-A1`
**1/1 caught**. `lnx-feed-A4` (drop the wait's floor of a period) was **NOT CAUGHT** and was
removed: alsa-lib raises an `avail_min` below the period to the period itself (MEASURED: a
request of 0 read back as 480), so it is an equivalent mutant.

## Shared edits (for "Merge notes")

* `crates/omni-platform/src/audio/mod.rs`: backend selection -- `unix.rs` now for
  `all(unix, not(target_os = "linux"))`, `linux.rs` for `target_os = "linux"`; a Linux-only
  `pub use linux::Recoveries` and `AudioOutput::recoveries()` (`#[cfg(target_os = "linux")]`).
  Windows compiles exactly what it did.
* `crates/omni-platform/src/audio/error.rs`: new variant `AudioError::Alsa { operation, api,
  errno, description }`, because `Os` prints its code as an HRESULT. Additive; nothing in the
  workspace matches `AudioError` exhaustively (grep).
* `unix.rs` (ours): header text only -- it no longer serves Linux.

## Open

* ~~`omni-android --test aaudio` not run on Linux~~: it runs (9 passed, 2026-09-25), including
  `fmods_stream_on_the_host_device_is_fed_at_its_rate_without_spinning`, FMOD's sequence into
  this backend through `PlatformOutput`.
* FMOD probes with capacity 0 (answered 960) and then opens with 9,120, keeping 1,440 queued
  (30 ms). The session report counted 1 xrun; whether that is audible is unmeasured.
* The graph rate is assumed 48 kHz by request, not read from PipeWire (see "Why ALSA").
* Suspend recovery is unit-tested against a scripted double only; no live suspend was triggered.
