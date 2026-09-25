"""lnx-feed-: the ALSA wait honours the frames its caller asked for (crates/omni-platform/src/audio/linux.rs).

MEASURED on this host in the Pet Simulator 99 world: FMOD's data-callback thread at 88-96 % of a
core for the whole session. FMOD's stream is a 1,440-frame buffer size in a 9,120-frame buffer with
a 480-frame burst; `snd_pcm_wait` is level-triggered on `avail_min`, which was one period, and
more than a period is always free in that shape -- so every wait answered at once and the feeding
loop spun. The wait now installs the frames asked for as `avail_min`, and sleeps through a wake
that comes short of them instead of polling again. The `feed-` rows in `tools/mutate.py` are the
AAudio half (the loop asks for the room a burst needs).

Row format and command: `audio.py`'s. The detectors are the gated live unit tests
`a_wait_installs_the_frames_it_asks_for_as_avail_min` and
`a_wake_short_of_the_frames_asked_for_is_slept_through_not_polled` in linux.rs, and
`tests/audio_live.rs`'s `a_wait_for_more_than_a_period_returns_with_that_much_free`,
`a_wait_for_room_an_unstarted_stream_cannot_make_runs_to_its_timeout` and
`a_feeder_below_the_buffer_waits_for_its_burst_rather_than_spinning` (thread CPU time).
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from audio import AUDIO, LINUX_RS  # noqa: E402

ROWS = [
    ("lnx-feed-A1", "A", "the wait's frames are not installed as avail_min (the poll answers at a period)",
     LINUX_RS,
     """        self.set_avail_min(wanted.min(self.buffer_frames))?;""",
     """        let _ = Self::set_avail_min;""",
     AUDIO),

    ("lnx-feed-A2", "A", "a wake short of the frames asked for polls again at once (spins)",
     LINUX_RS,
     """            std::thread::sleep(play_time(wanted - free, self.format.sample_rate).min(left));""",
     """            let _ = play_time;""",
     AUDIO),

    ("lnx-feed-A3", "A", "the wait returns after one wake whatever is free",
     LINUX_RS,
     """            if free >= wanted || waited == 0 || left.is_zero() {""",
     """            if true {""",
     AUDIO),

    # lnx-feed-A4 (`let wanted = frames;`, dropping the floor of a period) was run and was NOT
    # CAUGHT, and cannot be: alsa-lib's `snd_pcm_sw_params_set_avail_min` itself raises a value
    # below the period to the period, and a wait asking for less than that is then answered at
    # the same wake either way. An equivalent mutant, removed rather than kept as a standing miss;
    # the floor stays in linux.rs so that `wanted` says what is installed.

    ("lnx-feed-B1", "B", "avail_min is not bounded by the buffer",
     LINUX_RS,
     """        self.set_avail_min(wanted.min(self.buffer_frames))?;""",
     """        self.set_avail_min(wanted)?;""",
     AUDIO),

    ("lnx-feed-B2", "B", "a short wake sleeps the rest of the timeout, not the missing frames' time",
     LINUX_RS,
     """            std::thread::sleep(play_time(wanted - free, self.format.sample_rate).min(left));""",
     """            std::thread::sleep(left);""",
     AUDIO),
]
