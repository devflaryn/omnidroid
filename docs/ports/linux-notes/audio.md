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
| `wait_writable(timeout)` | started: `snd_pcm_wait(ms)`, timeout rounded up, capped at `INT_MAX`, never `-1`. Not started: the timeout is slept out (see "Differences") |
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
* **A running wait returns at once if a period is already free** (poll semantics), where WASAPI
  waits for the next period event. In the feeding loop (write what is free, then wait) the two are
  the same: after a full write less than a period is free.
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

* `omni-android --test aaudio` needs the vm and fault backends (MEMORY worker, `lnx-mem`). At the
  time of writing `lnx-mem` has the vm commit (`6db973e`) and no fault work, so it was not merged
  and the test was not run: **pending**. It drives a recording double, not this
  backend; the path from FMOD to this backend has not run on Linux.
* FMOD opens with capacity 0, which this backend answers with 960 frames (two 10 ms periods) --
  20 ms of buffer for a guest callback running under translation. Whether that underruns in the
  game is unmeasured; the xrun count will say.
* The graph rate is assumed 48 kHz by request, not read from PipeWire (see "Why ALSA").
* Suspend recovery is unit-tested against a scripted double only; no live suspend was triggered.
