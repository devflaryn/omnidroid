"""lnx-audio-: the ALSA backend behind the audio seam (crates/omni-platform/src/audio/linux.rs).

Row format (the same seven fields as `tools/mutate.py`'s table):
    (id, direction "A" revert-a-fix | "B" over-correct, description, path, old, new, argv)
`old` must match the file exactly once; `argv` must pass on the unmutated tree.

Every row runs the same command: the audio unit tests, the gated live unit tests in linux.rs, the
Linux live test and the cross-platform contract test (`tests/audio_live.rs`), each to its own
exit code, so that the caught list names every test that noticed. The live ones open the host's
real default playback device and write silence only; they need a device, as the rows' mutations
do. `hw_card_*` is skipped: it needs the card free of the sound server, which the tests before it
have just been using (see its doc comment).
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import with_env  # noqa: E402

LINUX_RS = "crates/omni-platform/src/audio/linux.rs"

_CARGO = "cargo test -p omni-platform --release --no-fail-fast"
_SCRIPT = "; ".join([
    f"{_CARGO} --lib audio:: -- --test-threads=1; a=$?",
    f"{_CARGO} --lib audio::linux -- --ignored --skip hw_card --test-threads=1; b=$?",
    f"{_CARGO} --test audio_live_linux -- --ignored --test-threads=1; c=$?",
    f"{_CARGO} --test audio_live -- --ignored --test-threads=1; d=$?",
    "exit $((a | b | c | d))",
])
AUDIO = with_env({"OMNI_AUDIO_LIVE_TESTS": "1"}, ["sh", "-c", _SCRIPT])

ROWS = [
    ("lnx-audio-A1", "A", "writable_frames ignores snd_pcm_avail and answers the whole buffer",
     LINUX_RS,
     """        Ok(frames_u32(Uframes::try_from(avail).unwrap_or(0)).min(self.buffer_frames))""",
     """        Ok(self.buffer_frames)""",
     AUDIO),

    ("lnx-audio-A2", "A", "an xrun is recovered but not counted",
     LINUX_RS,
     """        check(operation, "snd_pcm_prepare", pcm.prepare())?;
        now.xruns += 1;""",
     """        check(operation, "snd_pcm_prepare", pcm.prepare())?;""",
     AUDIO),

    ("lnx-audio-A3", "A", "an xrun is counted and swallowed without snd_pcm_prepare",
     LINUX_RS,
     """        check(operation, "snd_pcm_prepare", pcm.prepare())?;
        now.xruns += 1;""",
     """        now.xruns += 1;""",
     AUDIO),

    ("lnx-audio-A4", "A", "the format reported is the one asked for, not the one granted",
     LINUX_RS,
     """            sample_rate: rate,
            channels: u16::try_from(channels).unwrap_or(u16::MAX),""",
     """            sample_rate: request.rate,
            channels: u16::try_from(request.channels).unwrap_or(u16::MAX),""",
     AUDIO),

    ("lnx-audio-A5", "A", "a running wait ignores its timeout (snd_pcm_wait -1, for ever)",
     LINUX_RS,
     """snd_pcm_wait(self.pcm.raw(), wait_millis(timeout))""",
     """snd_pcm_wait(self.pcm.raw(), -1)""",
     AUDIO),

    ("lnx-audio-A6", "A", "an unstarted wait returns at once instead of running to its timeout",
     LINUX_RS,
     """            std::thread::sleep(timeout);
            return self.writable_frames("wait_writable");""",
     """            return self.writable_frames("wait_writable");""",
     AUDIO),

    ("lnx-audio-A7", "A", "stop does not pause, so a stopped stream keeps draining",
     LINUX_RS,
     """            check("stop", "snd_pcm_pause(1)", unsafe { snd_pcm_pause(self.pcm.raw(), 1) })?;""",
     """            {}""",
     AUDIO),

    ("lnx-audio-A8", "A", "a write does not restart a running stream left prepared by a recovery",
     LINUX_RS,
     """        if self.running && self.pcm.state() == SND_PCM_STATE_PREPARED {""",
     """        if false && self.running && self.pcm.state() == SND_PCM_STATE_PREPARED {""",
     AUDIO),

    ("lnx-audio-A9", "A", "a suspend is recovered but not counted",
     LINUX_RS,
     """        now.suspends += 1;""",
     """        let _ = &mut now;""",
     AUDIO),

    ("lnx-audio-A10", "A", "the wait timeout rounds down, so a sub-millisecond wait is a poll",
     LINUX_RS,
     """    let millis = timeout.as_nanos().div_ceil(1_000_000);""",
     """    let millis = timeout.as_nanos() / 1_000_000;""",
     AUDIO),

    ("lnx-audio-A11", "A", "start_threshold left at 1: the first write starts an unstarted stream",
     LINUX_RS,
     """snd_pcm_sw_params_set_start_threshold(p, s, boundary)""",
     """snd_pcm_sw_params_set_start_threshold(p, s, 1)""",
     AUDIO),

    ("lnx-audio-B1", "B", "every ALSA failure is treated as a recoverable xrun",
     LINUX_RS,
     """    } else {
        return Err(alsa_error(operation, api, code));
    }
    counts.set(now);""",
     """    } else {
        check(operation, "snd_pcm_prepare", pcm.prepare())?;
        now.xruns += 1;
    }
    counts.set(now);""",
     AUDIO),

    ("lnx-audio-B2", "B", "an xrun is counted before its recovery, so a failed prepare still counts",
     LINUX_RS,
     """        check(operation, "snd_pcm_prepare", pcm.prepare())?;
        now.xruns += 1;""",
     """        now.xruns += 1;
        counts.set(now);
        check(operation, "snd_pcm_prepare", pcm.prepare())?;""",
     AUDIO),

    ("lnx-audio-B3", "B", "start calls snd_pcm_start even with nothing queued",
     LINUX_RS,
     """            SND_PCM_STATE_PREPARED if self.writable_frames("start")? < self.buffer_frames => {""",
     """            SND_PCM_STATE_PREPARED => {""",
     AUDIO),

    ("lnx-audio-B4", "B", "writable_frames keeps a period of headroom back from the caller",
     LINUX_RS,
     """.min(self.buffer_frames))""",
     """.min(self.buffer_frames - self.period_frames))""",
     AUDIO),

    ("lnx-audio-B5", "B", "a running wait also sleeps its whole timeout out",
     LINUX_RS,
     """        if !self.running {
            std::thread::sleep(timeout);""",
     """        if true {
            std::thread::sleep(timeout);""",
     AUDIO),

    ("lnx-audio-B6", "B", "a suspend is prepared even after snd_pcm_resume succeeded",
     LINUX_RS,
     """        if resumed < 0 {""",
     """        if resumed <= 0 {""",
     AUDIO),
]
