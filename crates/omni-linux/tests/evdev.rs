//! Input devices (`crate::evdev`) under the real bionic: the `evdev` fixture lists `/dev/input`,
//! opens the two devices and asks each what it is, as Android's `EventHub` does, then waits in
//! epoll for what this test sends from a host thread -- the way the display window's keyboard and
//! mouse reach system_server's `InputReader`.
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use omni_linux::evdev::{self, Spec, EV_KEY, EV_REL, REL_X, REL_Y};

#[test]
fn the_fixture_meets_a_keyboard_and_a_mouse_as_eventhub_does() {
    let keyboard = evdev::register(Spec::keyboard("omnidroid keyboard"));
    let mouse = evdev::register(Spec::mouse("omnidroid mouse"));
    assert_eq!((keyboard.number, mouse.number), (0, 1), "this test binary's first devices");
    // Sent until the fixture has read them: it opens its descriptors first, and a packet sent before
    // an open is not that open's.
    let stop = Arc::new(AtomicBool::new(false));
    let sender = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                keyboard.send(&[(EV_KEY, 30, 1)]);
                keyboard.send(&[(EV_KEY, 30, 0)]);
                mouse.send(&[(EV_REL, REL_X, 5), (EV_REL, REL_Y, -3)]);
            }
        })
    };
    let Some((status, out, err)) = common::run_fixture("evdev", &[]) else { return };
    stop.store(true, Ordering::Relaxed);
    let _ = sender.join();
    assert!(!out.contains("FAIL"), "{out}\n{err}");
    for line in ["ok event0 and event1 listed", "ok EVIOCGNAME", "ok keyboard: letters, no mouse buttons", "ok mouse: REL_X, REL_Y, REL_WHEEL", "ok empty: EAGAIN", "ok KEY_A down arrives", "ok KEY_A up arrives", "ok REL_X +5 arrives"] {
        assert!(out.contains(line), "missing {line:?}:\n{out}\n{err}");
    }
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "{out}\n{err}");
}
