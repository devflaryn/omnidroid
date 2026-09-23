//! The pure halves of the X11 backend: what an X event's numbers mean in the seam's terms, with no
//! X call in any of them, so that each rule is a unit test rather than a claim about a live server.

use super::super::PointerButton;

/// One notch of a wheel in the seam's unit: Windows' `WHEEL_DELTA`, which
/// [`WindowEvent::Wheel`](super::super::WindowEvent::Wheel) is specified in and `omni-android`'s
/// mouse path divides by.
pub const WHEEL_NOTCH: i32 = 120;

/// What a core X pointer button number is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// A button the seam names.
    Pointer(PointerButton),
    /// One notch of the wheel, as `(dx, dy)` in [`WHEEL_NOTCH`] units.
    Wheel(i32, i32),
}

/// The meaning of core button `button` -- the number *after* the server's pointer mapping, so a
/// left-handed user's swap has already happened and `1` is the primary button whichever side it
/// is on (the seam's `PointerButton` is named by role for the same reason).
///
/// The numbering is the X server's and every toolkit's: 1 primary, 2 middle, 3 secondary; 4 and 5
/// the wheel rolled **up (away from the user)** and down, which the seam signs `+` and `-`; 6 and
/// 7 the wheel tilted **left** and right, `-` and `+`; 8 back and 9 forward. Anything else -- the
/// buttons past 9 some mice have -- is `None`.
#[must_use]
pub const fn button(button: u32) -> Option<Button> {
    Some(match button {
        1 => Button::Pointer(PointerButton::Primary),
        2 => Button::Pointer(PointerButton::Middle),
        3 => Button::Pointer(PointerButton::Secondary),
        4 => Button::Wheel(0, WHEEL_NOTCH),
        5 => Button::Wheel(0, -WHEEL_NOTCH),
        6 => Button::Wheel(-WHEEL_NOTCH, 0),
        7 => Button::Wheel(WHEEL_NOTCH, 0),
        8 => Button::Pointer(PointerButton::Back),
        9 => Button::Pointer(PointerButton::Forward),
        _ => return None,
    })
}

/// The characters of `committed` that are text, one string each, in order: every character except
/// the C0 control codes and DEL, which [`WindowEvent::Text`](super::super::WindowEvent::Text)
/// defines as keys rather than text (Backspace `0x08`, Tab `0x09`, Enter `0x0D`, Escape `0x1B`,
/// Ctrl+letter `0x01`-`0x1A`, Delete `0x7F` -- all of which `Xutf8LookupString` does return).
///
/// One event per character rather than one per commit, so that a Linux `Text` is the single
/// character a Windows one is and a consumer sees one shape from both.
pub fn text_events(committed: &str) -> impl Iterator<Item = String> + '_ {
    committed.chars().filter(|&c| !(c < ' ' || c == '\u{7F}')).map(String::from)
}

/// `Xft.dpi` from the text of the root window's `RESOURCE_MANAGER` property: the value of the
/// last line whose resource name is exactly `Xft.dpi`, if it is a positive number.
///
/// **Why this is the first answer.** It is the display scale the user chose -- GNOME, KDE and Xfce
/// each write it when the scaling setting changes, and Xwayland under GNOME carries it for the
/// compositor's scale -- which is the same logical figure Windows' `GetDpiForWindow` is (96 at
/// 100%). Resource lines are `name:<whitespace>value`; later lines win, as `xrdb` merges them.
#[must_use]
pub fn xft_dpi(resources: &str) -> Option<f64> {
    let mut found = None;
    for line in resources.lines() {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.trim() != "Xft.dpi" {
            continue;
        }
        match value.trim().parse::<f64>() {
            Ok(dpi) if dpi.is_finite() && dpi > 0.0 => found = Some(dpi),
            _ => {}
        }
    }
    found
}

/// The screen's own DPI from its size in pixels and in millimetres (what the server reports as
/// `DisplayWidth`/`DisplayWidthMM`), rounded; `None` when the server reports no physical size.
///
/// The fallback when there is no `Xft.dpi`, and a **physical** figure rather than a chosen one:
/// Xvfb derives its millimetres from a 100 DPI default unless told otherwise, and Xorg commonly
/// reports whatever the monitor's EDID says.
#[must_use]
pub fn screen_dpi(pixels: i32, millimetres: i32) -> Option<u32> {
    if pixels <= 0 || millimetres <= 0 {
        return None;
    }
    let dpi = f64::from(pixels) * 25.4 / f64::from(millimetres);
    Some(dpi.round() as u32)
}

/// One axis of one pointing device, as XInput 2's raw events report it, turned into whole device
/// counts of motion.
///
/// * **Relative** (`XIModeRelative`, every mouse, and the XTEST device `xdotool` drives): a raw
///   value *is* the motion, unaccelerated.
/// * **Absolute** (`XIModeAbsolute`, a tablet or a virtual machine's pointer): a raw value is a
///   position in `min..=max`, so the motion is the difference from the last one, scaled onto
///   `extent` screen pixels. The first report has nothing to differ from and is no motion -- the
///   rule the Windows backend applies to `MOUSE_MOVE_ABSOLUTE`.
///
/// Fractions are **kept** rather than dropped: a high-resolution device reports parts of a count,
/// and truncating each would lose all of a slow movement. What is handed out is the whole part of
/// everything accumulated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Axis {
    /// `Some((min, max, extent))` for an absolute axis.
    absolute: Option<(f64, f64, f64)>,
    last: Option<f64>,
    residual: f64,
}

impl Axis {
    /// A relative axis.
    #[must_use]
    pub const fn relative() -> Axis {
        Axis { absolute: None, last: None, residual: 0.0 }
    }

    /// An absolute axis whose values run `min..=max` across `extent` screen pixels.
    #[must_use]
    pub const fn absolute(min: f64, max: f64, extent: f64) -> Axis {
        Axis { absolute: Some((min, max, extent)), last: None, residual: 0.0 }
    }

    /// The whole counts of motion one raw `value` adds. Total: a degenerate range is one unit,
    /// a non-finite value is no motion, and the result saturates.
    pub fn motion(&mut self, value: f64) -> i32 {
        if !value.is_finite() {
            return 0;
        }
        let delta = match self.absolute {
            None => value,
            Some((min, max, extent)) => {
                let span = if max > min { max - min } else { 1.0 };
                let now = (value - min) / span * extent;
                match self.last.replace(now) {
                    Some(was) => now - was,
                    None => 0.0,
                }
            }
        };
        self.residual += delta;
        let whole = self.residual.trunc();
        self.residual -= whole;
        whole.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All five buttons by role, and the four wheel directions signed as the seam signs them:
    /// `dy` positive away from the user (button 4, "up"), `dx` positive to the right (button 7).
    #[test]
    fn every_button_and_every_wheel_direction() {
        assert_eq!(button(1), Some(Button::Pointer(PointerButton::Primary)));
        assert_eq!(button(2), Some(Button::Pointer(PointerButton::Middle)));
        assert_eq!(button(3), Some(Button::Pointer(PointerButton::Secondary)));
        assert_eq!(button(8), Some(Button::Pointer(PointerButton::Back)));
        assert_eq!(button(9), Some(Button::Pointer(PointerButton::Forward)));
        assert_eq!(button(4), Some(Button::Wheel(0, 120)), "rolled away from the user");
        assert_eq!(button(5), Some(Button::Wheel(0, -120)), "rolled towards the user");
        assert_eq!(button(6), Some(Button::Wheel(-120, 0)), "tilted left");
        assert_eq!(button(7), Some(Button::Wheel(120, 0)), "tilted right");
        for other in [0, 10, 11, 255] {
            assert_eq!(button(other), None, "button {other}");
        }
    }

    /// Text is every character but the C0 controls and DEL, one event each and in order.
    #[test]
    fn control_codes_are_keys_and_everything_else_is_text() {
        let texts = |s: &str| text_events(s).collect::<Vec<_>>();
        assert_eq!(texts("a"), ["a"]);
        assert_eq!(texts("é水😀"), ["é", "水", "😀"]);
        assert_eq!(texts(" ~"), [" ", "~"], "space and the unit below DEL are text");
        for unit in (0x00..=0x1Fu8).chain([0x7F]) {
            let s = char::from(unit).to_string();
            assert!(texts(&s).is_empty(), "{unit:#04x} must not be text");
        }
        assert_eq!(texts("a\rb\u{8}\u{7F}c"), ["a", "b", "c"]);
        assert!(texts("").is_empty());
    }

    /// `Xft.dpi` is read from the resource text, the last one winning, and a malformed or
    /// non-positive value is no answer.
    #[test]
    fn the_resource_dpi_is_the_last_well_formed_xft_dpi() {
        assert_eq!(xft_dpi("Xft.dpi:\t96\n"), Some(96.0));
        assert_eq!(xft_dpi("Xcursor.size:\t24\nXft.dpi:\t144\nXft.antialias:\t1"), Some(144.0));
        assert_eq!(xft_dpi("Xft.dpi: 96\nXft.dpi: 120"), Some(120.0), "later lines win");
        assert_eq!(xft_dpi("Xft.dpi:\t120.5"), Some(120.5));
        assert_eq!(xft_dpi("Xft.dpix:\t96"), None, "another resource");
        assert_eq!(xft_dpi("Xft.dpi:\t0"), None);
        assert_eq!(xft_dpi("Xft.dpi:\t-5"), None);
        assert_eq!(xft_dpi("Xft.dpi:\tlarge"), None);
        assert_eq!(xft_dpi(""), None);
    }

    /// The physical fallback: 1920 pixels across 488 mm is Xvfb's default 100 DPI.
    #[test]
    fn the_screen_dpi_is_pixels_per_inch_of_the_reported_size() {
        assert_eq!(screen_dpi(1920, 488), Some(100));
        assert_eq!(screen_dpi(1920, 508), Some(96));
        assert_eq!(screen_dpi(1920, 0), None, "no physical size reported");
        assert_eq!(screen_dpi(0, 488), None);
    }

    /// Relative raw values are the motion; fractions accumulate instead of vanishing.
    #[test]
    fn relative_axes_pass_counts_and_keep_fractions() {
        let mut axis = Axis::relative();
        assert_eq!(axis.motion(10.0), 10);
        assert_eq!(axis.motion(-5.0), -5);
        assert_eq!([axis.motion(0.4), axis.motion(0.4), axis.motion(0.4)], [0, 0, 1]);
        assert_eq!(axis.motion(f64::NAN), 0);
        assert_eq!(axis.motion(1e300), i32::MAX, "saturates");
    }

    /// Absolute raw values are positions: the first is no motion, then differences scaled onto
    /// the screen extent.
    #[test]
    fn absolute_axes_are_differences_of_scaled_positions() {
        let mut axis = Axis::absolute(0.0, 65535.0, 1920.0);
        assert_eq!(axis.motion(32767.5), 0, "the first position is not a motion");
        assert_eq!(axis.motion(65535.0), 960);
        assert_eq!(axis.motion(0.0), -1920);
        let mut degenerate = Axis::absolute(5.0, 5.0, 100.0);
        assert_eq!(degenerate.motion(5.0), 0);
        assert_eq!(degenerate.motion(6.0), 100, "a degenerate range is one unit");
    }
}
