//! **The device keyboard's own key character map, made from the host's current layout**, so that a
//! key types in the guest what it types on the host (US, Turkish F, German, ...).
//!
//! # Why the guest needs one
//!
//! The window's keyboard is an evdev device (`crate::evdev`, `Spec::keyboard("omnidroid
//! keyboard")`), so Android receives **physical keys** and makes characters itself: the key layout
//! (`.kl`) turns `KEY_*` into an Android keycode, the key character map (`.kcm`) turns keycode and
//! modifiers into a character. With no map of the device's own, EventHub takes `Generic.kl` and
//! `Generic.kcm` -- US -- and Android 14+'s KeyboardLayoutManager lays an overlay on top chosen from
//! the current IME's subtype locale: with `persist.sys.locale=tr-TR` and LatinIME, Turkish Q, on
//! every host, whatever the person types with (2026-10-07). The device now boots without an IME
//! (`device::apps_left_out`), so no overlay is chosen and the device's own map is the one used;
//! this module writes that map from [`omni_platform::keyboard::current_layout`].
//!
//! # Where Android finds it
//!
//! The device has vendor and product 0 (`Spec::keyboard`), so `KeyMap::load` skips the
//! `Vendor_XXXX_Product_XXXX` names and probes by **name**: `InputDeviceIdentifier::getCanonicalName`
//! (every character but `[A-Za-z0-9_-]` becomes `_`: `omnidroid_keyboard`), looked up by
//! `getInputDeviceConfigurationFilePathByName` (AOSP 15 `libs/input/InputDevice.cpp`) under
//! `/product/usr/`, `/system_ext/usr/`, `/odm/usr/`, `/vendor/usr/`, the config APEX if
//! `input_device.config_file.apex` names one, `$ANDROID_ROOT/usr/`, and last
//! `$ANDROID_DATA/system/devices/` -- each `keylayout/<name>.kl` or `keychars/<name>.kcm`, the first
//! one `access(R_OK)` allows. The image has neither name, so the data partition's is found, and
//! that is the right place for it: its content is the **host's**, known only at run time, while
//! the device overlay (`device/`, pinned by `SHA256SUMS`) is fixed at build. The instance's
//! `<instance>/data` is the guest's `/data`, so [`install`] writes
//! `<instance>/data/system/devices/{keychars,keylayout}/omnidroid_keyboard.*` with owners
//! (`crate::owners`) that let system_server (uid 1000) read them -- a host file with no recorded
//! owner reads as root's, mode 0600. It runs in the system's host process before the program does,
//! so before system_server's EventHub opens the device.
//!
//! # A key layout too: the ISO key
//!
//! `Generic.kl` maps **two** physical keys to `BACKSLASH`: `key 43` (US `\`, ISO `#`/`,`) and
//! `key 86` (`KEY_102ND`, the ISO key beside left Shift). On an ISO layout they type different
//! characters -- Turkish Q: `,`/`;` and `<`/`>`, German: `#` and `<` -- and one keycode has one KCM
//! entry. So the device gets a key layout of its own as well: `Generic.kl` from the image, with
//! `key 86` mapped to `PLUS` -- a keycode `Generic.kl` gives no key, with no system meaning, and the
//! one AOSP's own ISO layouts move the key to (`map key 86 PLUS` in `InputDevices`' German, French,
//! Turkish, ... maps). Every other key keeps `Generic.kl`'s keycode.
//!
//! # What the map holds
//!
//! `Generic.kcm` from the image, whole -- the keypad and its fallbacks, `SPACE`'s and `ENTER`'s
//! entries, the gamepad buttons, `ESCAPE`'s and `DEL`'s fallbacks -- with the entry of each
//! **typing key** ([`TYPING_KEYS`]: the digit row, the three letter rows and the ISO key) replaced
//! by the host's: `label`, `base`, and each modifier set the host was read under. A `type FULL` map
//! has to be complete: it replaces `Generic.kcm` rather than overlaying it.
//!
//! * **Modifier sets** are written one a line, least specific first: a KCM behaviour declared later
//!   is tried first (Generic's own `shift+capslock` after `shift, capslock` relies on it), and
//!   `alt`/`ralt`/`ctrl` must match exactly while `shift` and `capslock` need only be present -- so
//!   with every set written, the exact one wins. A set the host types nothing on is `none` where a
//!   less specific set would otherwise answer for it.
//! * **The third level** is `alt` on macOS (either Option) and `ralt, ctrl+ralt` on Windows (AltGr
//!   arrives as the left Ctrl and the right Alt).
//! * **A dead key** is its combining character (`'\u0301'` for `´`), which `KeyCharacterMap` reports
//!   as `COMBINING_ACCENT` and a text field composes with the next key; an accent Android has no
//!   combining form for types its spacing form instead.
//! * A character outside the BMP, a control character, or a ligature of several (a KCM behaviour
//!   holds one UTF-16 unit) is left out.
//!
//! `OMNI_HOST_KEYMAP=0` writes nothing and removes a map an earlier run wrote: Android's own
//! layout. A layout switched on the host during a session is not followed.
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use omni_platform::keyboard::{level, HostKey, HostLayout, KeyOutput, ThirdLevel};

use crate::owners::{Owner, Owners};
use crate::vfs::Sysroot;

/// The device's file name: `omnidroid keyboard` as `getCanonicalName` spells it.
pub const FILE_NAME: &str = "omnidroid_keyboard";

/// `KEY_102ND`, the ISO key left of `Z`, and the keycode it is moved to (see the module header).
pub const KEY_102ND: u16 = 86;
/// The Android keycode [`KEY_102ND`] types as on this device.
pub const KEY_102ND_KEYCODE: &str = "PLUS";

/// The keys whose characters come from the host: `KEY_1`..`KEY_EQUAL` (2-13), `KEY_Q`..
/// `KEY_RIGHTBRACE` (16-27), `KEY_A`..`KEY_GRAVE` (30-41), `KEY_BACKSLASH`..`KEY_SLASH` (43-53) and
/// `KEY_102ND` (86). Space, Enter, Tab and the keypad type the same on every layout and keep
/// Generic's entries, whose fallbacks (`SPACE`'s `SEARCH`, the keypad's arrows) a host does not have.
pub const TYPING_KEYS: &[std::ops::RangeInclusive<u16>] = &[2..=13, 16..=27, 30..=41, 43..=53, KEY_102ND..=KEY_102ND];

fn is_typing_key(code: u16) -> bool {
    TYPING_KEYS.iter().any(|r| r.contains(&code))
}

/// `InputDeviceIdentifier::getCanonicalName`: every character but ASCII letters, digits, `-` and
/// `_` becomes `_`.
#[must_use]
pub fn canonical_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// The `key <scancode> <KEYCODE>` lines of a key layout, by scancode (`key usage` lines and the
/// rest of the file are not keys of this device).
#[must_use]
pub fn key_names(kl: &str) -> HashMap<u16, String> {
    kl.lines()
        .filter_map(|line| {
            let mut words = line.split('#').next()?.split_whitespace();
            (words.next()? == "key").then_some(())?;
            let code = words.next()?.parse().ok()?;
            Some((code, words.next()?.to_string()))
        })
        .collect()
}

/// The device's key layout: `generic` with [`KEY_102ND`] moved to [`KEY_102ND_KEYCODE`].
///
/// # Errors
/// A layout that already gives [`KEY_102ND_KEYCODE`] a key: the ISO key would share it.
pub fn key_layout(generic: &str) -> Result<String, String> {
    if key_names(generic).values().any(|n| n == KEY_102ND_KEYCODE) {
        return Err(format!("the image's Generic.kl already maps a key to {KEY_102ND_KEYCODE}"));
    }
    let mut out = format!(
        "# omnidroid: the image's Generic.kl, with key {KEY_102ND} (KEY_102ND, the ISO key beside left Shift)\n\
         # moved from BACKSLASH to {KEY_102ND_KEYCODE}, so that it and key 43 can type different characters\n\
         # (omni-linux's keymap module).\n\n"
    );
    let mut moved = false;
    for line in generic.lines() {
        let words: Vec<&str> = line.split('#').next().unwrap_or("").split_whitespace().collect();
        if words.len() >= 3 && words[0] == "key" && words[1].parse() == Ok(KEY_102ND) {
            let flags = words[3..].iter().fold(String::new(), |s, w| s + " " + w);
            let _ = writeln!(out, "key {KEY_102ND}    {KEY_102ND_KEYCODE}{flags}");
            moved = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !moved {
        let _ = writeln!(out, "key {KEY_102ND}    {KEY_102ND_KEYCODE}");
    }
    Ok(out)
}

/// A character as a KCM literal: printable ASCII as itself (`'`, `\` escaped), the rest of the
/// BMP as `'\uXXXX'`. `None` for a control character or one outside the BMP (a KCM character is
/// one UTF-16 unit).
#[must_use]
pub fn literal(c: char) -> Option<String> {
    match c {
        '\\' => Some("'\\\\'".into()),
        '\'' => Some("'\\''".into()),
        ' '..='~' => Some(format!("'{c}'")),
        '\u{a0}'..='\u{ffff}' => Some(format!("'\\u{:04x}'", c as u32)),
        _ => None,
    }
}

/// The combining character Android's `KeyCharacterMap` composes for a dead key whose spacing form
/// is `spacing`, from the accents it knows (`KeyCharacterMap.java`'s `addCombining` table).
#[must_use]
pub fn combining(spacing: char) -> Option<char> {
    Some(match spacing {
        '`' | '\u{02cb}' => '\u{0300}',
        '\u{00b4}' | '\u{02ca}' | '\'' => '\u{0301}',
        '^' | '\u{02c6}' => '\u{0302}',
        '~' | '\u{02dc}' => '\u{0303}',
        '\u{00af}' | '\u{02c9}' => '\u{0304}',
        '\u{02d8}' => '\u{0306}',
        '\u{02d9}' => '\u{0307}',
        '\u{00a8}' | '"' => '\u{0308}',
        '\u{02da}' | '\u{00b0}' => '\u{030a}',
        '\u{02dd}' => '\u{030b}',
        '\u{02c7}' => '\u{030c}',
        '\u{00b8}' => '\u{0327}',
        '\u{02db}' => '\u{0328}',
        _ => return None,
    })
}

/// The one character a behaviour types for `out`, if it can be written (see the module header).
fn character(out: &KeyOutput) -> Option<char> {
    match out {
        KeyOutput::None => None,
        KeyOutput::Text(s) => {
            let mut chars = s.chars();
            let c = chars.next()?;
            (chars.next().is_none() && literal(c).is_some()).then_some(c)
        }
        KeyOutput::Dead(spacing) => combining(*spacing).or(Some(*spacing)).filter(|c| literal(*c).is_some()),
    }
}

/// The KCM spellings of modifier set `set` (several, comma-separated, where one set is two
/// spellings: Windows' AltGr).
fn spelling(set: usize, third: ThirdLevel) -> String {
    let mut base = Vec::new();
    if set & level::SHIFT != 0 {
        base.push("shift");
    }
    if set & level::CAPS != 0 {
        base.push("capslock");
    }
    if set & level::ALT == 0 {
        return if base.is_empty() { "base".into() } else { base.join("+") };
    }
    let alts: &[&str] = match third {
        ThirdLevel::EitherAlt => &["alt"],
        ThirdLevel::AltGr => &["ralt", "ctrl+ralt"],
    };
    alts.iter().map(|alt| base.iter().copied().chain([*alt]).collect::<Vec<_>>().join("+")).collect::<Vec<_>>().join(", ")
}

/// One key's entry.
fn entry(name: &str, key: &HostKey, third: ThirdLevel) -> String {
    let chars: Vec<Option<char>> = key.out.iter().map(character).collect();
    let mut out = format!("key {name} {{\n");
    let mut line = |property: &str, value: &str| {
        let _ = writeln!(out, "    {:<36}{value}", format!("{property}:"));
    };
    // The cap: the letter as Caps Lock types it (`A`), else the unshifted character, else a dead
    // key's spacing form.
    let label = match (chars[0], chars[level::CAPS]) {
        (Some(base), Some(caps)) if caps != base && base.is_alphabetic() => Some(caps),
        (Some(base), _) if !is_combining(base) => Some(base),
        _ => match &key.out[0] {
            KeyOutput::Dead(spacing) => Some(*spacing).filter(|c| literal(*c).is_some()),
            _ => None,
        },
    };
    if let Some(l) = label.and_then(literal) {
        line("label", &l);
    }
    for set in 0..level::COUNT {
        let value = match chars[set] {
            Some(c) => literal(c),
            // Nothing on this set: said, where a less specific set (same third level: `alt` must
            // match exactly) would answer for it; for the base, always.
            None if set == 0 || (0..set).any(|less| less & set == less && less & level::ALT == set & level::ALT && chars[less].is_some()) => Some("none".into()),
            None => None,
        };
        if let Some(v) = value {
            line(&spelling(set, third), &v);
        }
    }
    out.push_str("}\n");
    out
}

fn is_combining(c: char) -> bool {
    ('\u{0300}'..='\u{036f}').contains(&c)
}

/// The device's key character map: `generic_kcm` with each typing key's entry the host's. `kl` is
/// the device's key layout ([`key_layout`]), which names each key's keycode. Answers the map and
/// how many keys it took from the host.
///
/// # Errors
/// A `generic_kcm` that is not a `type FULL` map or whose `key` entries do not parse, a typing key
/// the layout gives no keycode, or two typing keys given one keycode.
pub fn key_character_map(generic_kcm: &str, kl: &str, layout: &HostLayout) -> Result<(String, usize), String> {
    if !generic_kcm.lines().any(|l| l.split('#').next().unwrap_or("").split_whitespace().eq(["type", "FULL"])) {
        return Err("the image's Generic.kcm is not a `type FULL` map".into());
    }
    let names = key_names(kl);
    let mut taken: HashMap<&str, u16> = HashMap::new();
    let mut entries = Vec::new();
    for key in layout.keys.iter().filter(|k| is_typing_key(k.code)) {
        let name = names.get(&key.code).ok_or_else(|| format!("the key layout has no keycode for key {}", key.code))?;
        if let Some(other) = taken.insert(name, key.code) {
            return Err(format!("keys {other} and {} are both {name} in the key layout", key.code));
        }
        entries.push(entry(name, key, layout.third_level));
    }
    // Generic's entries, less the ones replaced.
    let mut kept = String::new();
    let mut skipping = false;
    for line in generic_kcm.lines() {
        let words: Vec<&str> = line.split('#').next().unwrap_or("").split_whitespace().collect();
        if skipping {
            if words.contains(&"}") {
                skipping = false;
            }
            continue;
        }
        if let ["key", name, "{", ..] = words[..] {
            if taken.contains_key(name) {
                if !words.contains(&"}") {
                    skipping = true;
                }
                continue;
            }
        }
        kept.push_str(line);
        kept.push('\n');
    }
    if skipping {
        return Err("the image's Generic.kcm ends inside a `key` entry".into());
    }
    let mut out = format!(
        "# omnidroid: the key character map of the device `omnidroid keyboard`, generated at the session's\n\
         # start from the host's keyboard layout ({}) by omni-linux's keymap module. The image's\n\
         # Generic.kcm, with the {} typing keys' entries replaced by what each types on the host.\n\n",
        layout.name.replace('\n', " "),
        entries.len()
    );
    out.push_str(&kept);
    out.push_str("\n### The host's layout ###\n\n");
    out.push_str(&entries.join("\n"));
    Ok((out, entries.len()))
}

/// The two files [`install`] writes, under the guest's `/data/system/devices`.
fn files(instance: &Path) -> [PathBuf; 2] {
    let devices = instance.join("data/system/devices");
    [devices.join("keychars").join(format!("{FILE_NAME}.kcm")), devices.join("keylayout").join(format!("{FILE_NAME}.kl"))]
}

/// **Write the device keyboard's key layout and key character map from the host's current layout**
/// into `instance`'s data partition, readable by system_server -- or, when the host's layout cannot
/// be read or `OMNI_HOST_KEYMAP=0`, remove any an earlier run wrote, so Android's own applies. Says
/// which in one line. Call it before the system's program runs, only when the device has the
/// window's keyboard.
pub fn install(instance: &Path, sysroot: &Sysroot) {
    let paths = files(instance);
    let remove = || {
        for p in &paths {
            if std::fs::remove_file(p).is_ok() {
                Owners::of(instance).forget(p);
            }
        }
    };
    if std::env::var("OMNI_HOST_KEYMAP").as_deref() == Ok("0") {
        remove();
        eprintln!("[keyboard] off (OMNI_HOST_KEYMAP=0): Android's own layout (Generic.kcm)");
        return;
    }
    match generate(sysroot).and_then(|(kcm, kl, name, keys)| write(instance, &paths, &kcm, &kl).map(|()| (name, keys))) {
        Ok((name, keys)) => eprintln!("[keyboard] the host's layout: {name}, {keys} keys"),
        Err(e) => {
            remove();
            eprintln!("[keyboard] the host's layout is not used ({e}): Android's own layout (Generic.kcm)");
        }
    }
}

/// The map and the layout, the host layout's name and how many keys it gave.
fn generate(sysroot: &Sysroot) -> Result<(String, String, String, usize), String> {
    let layout = omni_platform::keyboard::current_layout().map_err(|e| e.to_string())?;
    let read = |path: &str| sysroot.read(path.as_bytes()).map(|b| String::from_utf8_lossy(&b).into_owned()).ok_or_else(|| format!("the image has no {path}"));
    let kl = key_layout(&read("/system/usr/keylayout/Generic.kl")?)?;
    let (kcm, keys) = key_character_map(&read("/system/usr/keychars/Generic.kcm")?, &kl, &layout)?;
    Ok((kcm, kl, layout.name, keys))
}

/// Write `kcm` and `kl` to `paths` (each whole: beside, then renamed), the directories and files
/// owned by `system`, as init makes `/data/system`'s.
fn write(instance: &Path, paths: &[PathBuf; 2], kcm: &str, kl: &str) -> Result<(), String> {
    let system = instance.join("data/system");
    if !system.is_dir() {
        return Err(format!("{} is not there yet", system.display()));
    }
    let owners = Owners::of(instance);
    let dir = |mode| Owner { uid: 1000, gid: 1000, mode };
    for (path, text) in paths.iter().zip([kcm, kl]) {
        let parent = path.parent().expect("a file in a directory");
        for d in [parent.parent().expect("devices"), parent] {
            if !d.is_dir() {
                std::fs::create_dir(d).map_err(|e| format!("{}: {e}", d.display()))?;
                owners.set(d, dir(0o755));
            }
        }
        let tmp = path.with_extension(format!("part{}", std::process::id()));
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
        owners.set(path, dir(0o644));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> KeyOutput {
        KeyOutput::Text(s.into())
    }

    /// A key from its eight sets, in `level` order: base, shift, caps, shift+caps, alt, shift+alt,
    /// caps+alt, shift+caps+alt.
    fn key(code: u16, out: [&str; 8]) -> HostKey {
        HostKey { code, out: out.map(|s| if s.is_empty() { KeyOutput::None } else { text(s) }) }
    }

    const KL: &str = "# a comment\nkey 2     1\nkey 30    A\nkey 39    SEMICOLON\nkey 40    APOSTROPHE\nkey 43    BACKSLASH\nkey 57    SPACE\nkey 86    BACKSLASH\nkey 115   VOLUME_UP\nkey usage 0x0c006F BRIGHTNESS_UP\n";
    const KCM: &str = "# Generic\n\ntype FULL\n\nkey A {\n    label:   'A'\n    base:    'a'\n}\n\nkey SPACE {\n    label: ' '\n    base: ' '\n    alt, meta: fallback SEARCH\n}\n\nkey BACKSLASH {\n    base: '\\\\'\n}\n\nkey PLUS {\n    label: '+'\n    base: '+'\n}\n";

    fn layout(keys: Vec<HostKey>, third_level: ThirdLevel) -> HostLayout {
        HostLayout { name: "Test".into(), third_level, keys }
    }

    #[test]
    fn the_canonical_name_is_eventhubs() {
        assert_eq!(canonical_name("omnidroid keyboard"), FILE_NAME);
        assert_eq!(canonical_name("a.b-c_d/é"), "a_b-c_d__");
    }

    #[test]
    fn literals_are_escaped_as_the_kcm_tokenizer_reads_them() {
        assert_eq!(literal('a').as_deref(), Some("'a'"));
        assert_eq!(literal('\'').as_deref(), Some("'\\''"));
        assert_eq!(literal('\\').as_deref(), Some("'\\\\'"));
        assert_eq!(literal('"').as_deref(), Some("'\"'"));
        assert_eq!(literal('ş').as_deref(), Some("'\\u015f'"));
        assert_eq!(literal('€').as_deref(), Some("'\\u20ac'"));
        assert_eq!(literal('\u{0301}').as_deref(), Some("'\\u0301'"));
        for c in ['\n', '\t', '\u{7f}', '\u{85}', '😀'] {
            assert_eq!(literal(c), None, "{c:?}");
        }
    }

    #[test]
    fn the_iso_key_gets_a_keycode_of_its_own() {
        let kl = key_layout(KL).unwrap();
        let names = key_names(&kl);
        assert_eq!(names[&43], "BACKSLASH");
        assert_eq!(names[&KEY_102ND], KEY_102ND_KEYCODE);
        assert_eq!(names[&115], "VOLUME_UP", "the rest is Generic's");
        assert!(kl.contains("key usage 0x0c006F BRIGHTNESS_UP"), "usage lines kept");
        assert!(key_layout(&format!("{KL}key 200 PLUS\n")).is_err(), "PLUS already taken");

        // German: `#'` on key 43, `<>|` on the ISO key -- two entries.
        let keys = vec![key(43, ["#", "'", "#", "'", "", "", "", ""]), key(KEY_102ND, ["<", ">", "<", ">", "|", "", "|", ""])];
        let (kcm, n) = key_character_map(KCM, &kl, &layout(keys.clone(), ThirdLevel::AltGr)).unwrap();
        assert_eq!(n, 2);
        assert!(kcm.contains("key BACKSLASH {\n    label:                              '#'\n    base:                               '#'\n    shift:                              '\\''"), "{kcm}");
        assert!(kcm.contains("key PLUS {\n    label:                              '<'\n    base:                               '<'\n    shift:                              '>'"), "{kcm}");
        assert!(kcm.contains("    ralt, ctrl+ralt:                    '|'"), "AltGr, as Windows reports it: {kcm}");
        assert!(kcm.contains("    shift+ralt, shift+ctrl+ralt:        none"), "Shift+AltGr types nothing, not AltGr's |: {kcm}");
        assert_eq!(kcm.matches("key BACKSLASH {").count(), 1, "Generic's entry replaced");
        assert_eq!(kcm.matches("key PLUS {").count(), 1, "Generic's entry replaced");
        assert!(kcm.contains("    base: ' '\n    alt, meta: fallback SEARCH"), "SPACE is Generic's");

        // With Generic's own layout, the two keys are one keycode: refused, not merged.
        assert!(key_character_map(KCM, KL, &layout(keys, ThirdLevel::AltGr)).unwrap_err().contains("both BACKSLASH"));
    }

    #[test]
    fn a_letter_takes_the_hosts_case_and_option_layer() {
        // Turkish Q's `i` key (on KEY_APOSTROPHE's position here only for the test's layout): the
        // dotless/dotted pair Caps Lock types, Option's characters on macOS.
        let keys = vec![key(30, ["a", "A", "A", "A", "å", "Å", "Å", "Å"]), key(40, ["i", "İ", "İ", "i", "", "", "", ""])];
        let (kcm, _) = key_character_map(KCM, &key_layout(KL).unwrap(), &layout(keys, ThirdLevel::EitherAlt)).unwrap();
        let a = &kcm[kcm.find("key A {").unwrap()..];
        let a = &a[..a.find('}').unwrap()];
        for want in ["label:                              'A'", "base:                               'a'", "shift:                              'A'", "capslock:                           'A'", "shift+capslock:                     'A'", "alt:                                '\\u00e5'", "shift+capslock+alt:                 '\\u00c5'"] {
            assert!(a.contains(want), "{want} in\n{a}");
        }
        let order: Vec<usize> = ["    base:", "    shift:", "    capslock:", "    shift+capslock:", "    alt:"].iter().map(|p| a.find(p).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "least specific first: {a}");
        assert_eq!(kcm.matches("key A {").count(), 1);
        assert!(kcm.contains("key APOSTROPHE {\n    label:                              '\\u0130'\n    base:                               'i'"), "{kcm}");
    }

    #[test]
    fn a_dead_key_is_its_combining_accent() {
        // A German keyboard's `´` key (KEY_EQUAL's position on PC; key 2 here, the test's layout).
        let mut k = key(2, ["", "", "", "", "", "", "", ""]);
        k.out[0] = KeyOutput::Dead('\u{00b4}');
        k.out[level::SHIFT] = KeyOutput::Dead('`');
        k.out[level::CAPS] = KeyOutput::Dead('\u{00b4}');
        k.out[level::ALT] = KeyOutput::Dead('\u{2603}'); // an accent Android has no combining form of
        let (kcm, _) = key_character_map(KCM, &key_layout(KL).unwrap(), &layout(vec![k], ThirdLevel::EitherAlt)).unwrap();
        let e = &kcm[kcm.find("key 1 {").unwrap()..];
        assert!(e.starts_with("key 1 {\n    label:                              '\\u00b4'\n    base:                               '\\u0301'\n    shift:                              '\\u0300'"), "{e}");
        assert!(e.contains("    alt:                                '\\u2603'"), "{e}");
        assert!(e.contains("    shift+capslock:                     none"), "{e}");
    }

    #[test]
    fn a_key_the_host_types_nothing_with_types_nothing() {
        let (kcm, _) = key_character_map(KCM, &key_layout(KL).unwrap(), &layout(vec![key(KEY_102ND, [""; 8])], ThirdLevel::EitherAlt)).unwrap();
        assert!(kcm.contains("key PLUS {\n    base:                               none\n}"), "{kcm}");
    }

    #[test]
    fn a_map_that_is_not_full_or_does_not_parse_is_refused() {
        let kl = key_layout(KL).unwrap();
        let one = layout(vec![key(30, ["a", "A", "A", "a", "", "", "", ""])], ThirdLevel::EitherAlt);
        assert!(key_character_map("type OVERLAY\n", &kl, &one).is_err());
        assert!(key_character_map("type FULL\nkey A {\n  base: 'a'\n", &kl, &one).is_err());
        let unknown = layout(vec![key(16, ["q", "Q", "Q", "q", "", "", "", ""])], ThirdLevel::EitherAlt);
        assert!(key_character_map(KCM, &kl, &unknown).unwrap_err().contains("no keycode for key 16"));
    }

    /// The real thing: this host's layout over the image's Generic files, printed
    /// (`cargo test -p omni-linux --lib keymap -- --nocapture`). Skipped without a sysroot or a
    /// host layout (Linux, a CI machine without a session).
    #[test]
    fn the_hosts_layout_over_the_images_generic_files() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sysroot/aosp-35");
        let Ok(sysroot) = Sysroot::open(&dir) else {
            eprintln!("skipped: no sysroot at {}", dir.display());
            return;
        };
        let (kcm, kl, name, keys) = match generate(&sysroot) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("skipped: {e}");
                return;
            }
        };
        // Every typing key the window delivers, each its own keycode; Generic's other entries kept.
        assert_eq!(keys, TYPING_KEYS.iter().map(|r| r.len()).sum::<usize>(), "{kcm}");
        assert_eq!(key_names(&kl)[&KEY_102ND], KEY_102ND_KEYCODE);
        for kept in ["key SPACE {", "key ENTER {", "key NUMPAD_0 {", "key ESCAPE {", "key DEL {", "key BUTTON_A {", "key AT {"] {
            assert_eq!(kcm.matches(kept).count(), 1, "{kept}");
        }
        eprintln!("[keyboard] the host's layout: {name}, {keys} keys\n{kcm}");
    }
}
