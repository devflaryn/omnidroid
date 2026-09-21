//! **Confinement.** How a guest path becomes a host path, and why it can only ever name a file
//! inside one directory.
//!
//! # The problem
//!
//! The guest is an Android ARM64 binary and the paths it uses are Android's: `/data/data/…`,
//! `/system/lib64/…`, `/proc/self/maps`. **None of them exists on this host**, and none of them
//! may be allowed to mean what it says: `open("/etc/passwd")` resolved against the host's own
//! root would hand the guest a host file. That is not a hypothetical. D6 records that the APK
//! under test is cheat-injected — a Luau executor was added to `libzstd-jni` by a third party —
//! and the whole design treats guest code as hostile. It is also the ordinary requirement:
//! "several isolated instances in one process" is non-negotiable, and two instances that can
//! reach each other's files are not isolated.
//!
//! # The policy, in one sentence
//!
//! **Every guest path is resolved to a host path inside one host directory, by rules applied
//! before any host call, and a path that cannot be resolved that way is refused by name rather
//! than answered.**
//!
//! There is no way to switch it off and no "escape hatch" argument: the root is supplied by the
//! host embedding when it builds the [`Filesystem`](super::Filesystem), and with no root supplied
//! there is no filesystem at all and every path-taking guest call refuses. Confinement is
//! therefore a property of the type rather than a check that can be forgotten.
//!
//! # The rules, in the order they are applied
//!
//! 1. **Length.** A path longer than [`PATH_MAX`] or a component longer than [`NAME_MAX`] is
//!    `ENAMETOOLONG` — the guest's own limits, so this is the answer a real device gives. It is
//!    also the bound that stops a 64 KiB guest string becoming a 64 KiB host path.
//! 2. **Encoding.** The bytes must be UTF-8. Android paths are; a host path is not built from
//!    bytes that are not, because the conversion would be lossy and two different guest paths
//!    could become one host file.
//! 3. **Lexical resolution, with no host call at all.** Split on `/`, drop `.` and empty
//!    components, and *pop* on `..`. A `..` at the top stays at the top, which is POSIX's own
//!    rule (`/..` is `/`). A relative path is resolved against the guest root, because this guest
//!    has no working directory to be relative to — see below. **After this step no `..` exists**,
//!    so none ever reaches the host.
//! 4. **Component hygiene**, which is where the host-specific hazards are refused. A component
//!    containing a path separator, a drive letter, a wildcard or a control character, a component
//!    that names a Windows character device, and a component with a trailing dot or space are all
//!    refused. Each is a way to name something outside the root, or to name two different guest
//!    paths as one host file, on at least one of the five targets.
//! 5. **Symlinks.** Every component of the resolved path is checked, and a symlink anywhere in it
//!    is refused — except a symlink as the *final* component of an `lstat`, which is exactly the
//!    call whose job is to describe one without following it.
//! 6. **A final containment assertion.** The built path must still start with the root. Steps 3
//!    and 4 already guarantee it; this is the check that notices if they ever stop doing so.
//!
//! # What this does and does not defend against
//!
//! It defends against everything the **guest** can do, and that is the threat D6 names. The guest
//! has no way to create a symlink: `symlink`, `symlinkat` and `link` are not in the reachable
//! import set and are not implemented, and `open(O_CREAT)` creates a regular file. So the set of
//! symlinks inside the root is fixed by whoever populated it, and rule 5 refuses those.
//!
//! It is **not** race-free against an adversary who can create symlinks inside the root *while*
//! the guest is running, because the check and the open are two calls rather than one. Closing
//! that needs `openat(2)` with `O_NOFOLLOW` per component, which Windows has no equivalent of and
//! which `std` does not expose on any target. Stated rather than papered over: the host operator
//! supplies the root, and a root a second adversary can write to is already a lost position.
//!
//! # The guest has no working directory, and that is a fact rather than a simplification
//!
//! `chdir`, `fchdir` and `getcwd` are **not** in the 188 statically-reachable imports, so nothing
//! this milestone runs can move or observe a working directory. A relative path is therefore
//! resolved against the guest's root, which is what an Android process that never called `chdir`
//! would see — `init` starts a zygote-forked app with `/` as its working directory.

use std::path::{Path, PathBuf};

use super::error::{FsError, FsErrorKind, FsResult};

/// Longest guest path this layer will resolve, including the terminating NUL's worth of slack.
///
/// Linux's `PATH_MAX`, which is what bionic's headers publish and what guest code sizes its own
/// buffers from. A longer path is `ENAMETOOLONG`, which is the answer a real device gives, and
/// the bound also keeps a guest's 64 KiB string (the boundary's `GuestMem::STRING_LIMIT`) from
/// becoming a 64 KiB host path.
pub const PATH_MAX: usize = 4096;

/// Longest single component, from Linux's `NAME_MAX`.
///
/// 255 is also what NTFS reports as its maximum component length, and the guest's own
/// `struct dirent` has a 256-byte `d_name` — so a longer name could not be returned by `readdir`
/// even if it could be created.
pub const NAME_MAX: usize = 255;

/// Whether a symlink is acceptable as the final component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalLink {
    /// Refuse it: the operation would follow the link, and this layer cannot confine where to.
    Refuse,
    /// Accept it without following: `lstat`'s whole purpose is to describe the link itself.
    Describe,
}

/// A guest path, resolved lexically into components with no host call made.
///
/// Separated from the host walk so that the *rules* can be tested exhaustively without a
/// filesystem, which is what makes the hostile cases cheap to enumerate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The components, `.`/`..`/empty already applied. Empty means the root itself.
    pub components: Vec<String>,
}

impl Resolved {
    /// The guest-visible absolute path these components spell, for a diagnostic.
    #[must_use]
    pub fn guest_path(&self) -> String {
        if self.components.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", self.components.join("/"))
        }
    }
}

/// Render guest path bytes for an error message without failing on them.
#[must_use]
pub fn display(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

/// Rules 1 to 4: turn guest path bytes into components, with no host call.
///
/// # Errors
///
/// [`FsError::Io`] with [`FsErrorKind::NameTooLong`] or [`FsErrorKind::InvalidInput`] for the
/// failures a real device also reports, and [`FsError::Confined`] for the ones that are a guest
/// trying to name something outside its root.
pub fn resolve_lexically(operation: &'static str, path: &[u8]) -> FsResult<Resolved> {
    let shown = display(path);
    if path.is_empty() {
        // POSIX: an empty path is ENOENT, not the working directory. Every call that takes one
        // reports it that way, so this is the contract rather than a refusal.
        return Err(FsError::kinded(
            operation,
            shown,
            FsErrorKind::NotFound,
            "an empty path names nothing",
        ));
    }
    if path.len() > PATH_MAX {
        return Err(FsError::kinded(
            operation,
            shown,
            FsErrorKind::NameTooLong,
            format!("{} bytes against a PATH_MAX of {PATH_MAX}", path.len()),
        ));
    }
    if path.contains(&0) {
        // Unreachable through the boundary, which reads a NUL-terminated string — kept because
        // this function is public and a caller with a byte slice is not obliged to have come
        // from there.
        return Err(FsError::confined(
            operation,
            shown,
            "the path contains an embedded NUL, which truncates differently in different layers",
        ));
    }
    let text = std::str::from_utf8(path).map_err(|_| {
        FsError::confined(
            operation,
            &shown,
            "the path is not UTF-8. Android paths are, and a host path built from bytes that are \
             not would have to be converted lossily — which can make two different guest paths \
             name one host file",
        )
    })?;

    let mut components: Vec<String> = Vec::new();
    for raw in text.split('/') {
        match raw {
            // `//` collapses and `.` is the current directory: POSIX drops both.
            "" | "." => continue,
            ".." => {
                // POSIX: `/..` is `/`. Popping at the top stays at the top, which is why no `..`
                // can ever leave the root — and why this is the *first* line of the defence
                // rather than a check made afterwards on a path the host already saw.
                components.pop();
            }
            name => {
                if let Some(why) = hostile_component(name) {
                    return Err(FsError::confined(operation, &shown, why));
                }
                if name.len() > NAME_MAX {
                    return Err(FsError::kinded(
                        operation,
                        &shown,
                        FsErrorKind::NameTooLong,
                        format!(
                            "the component `{name}` is {} bytes against a NAME_MAX of {NAME_MAX}",
                            name.len()
                        ),
                    ));
                }
                components.push(name.to_string());
            }
        }
    }
    Ok(Resolved { components })
}

/// Windows device names, which name a character device **whatever directory they appear in**.
///
/// `open("/data/NUL")` on a Windows host opens the null device rather than a file in the root,
/// and `CON`/`AUX`/`PRN` are worse: they are the console. The comparison is case-insensitive and
/// ignores any extension, because `nul.txt` is still the null device.
const WINDOWS_DEVICES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Why one path component may not be turned into a host path component, if it may not.
///
/// **Every rule here is a way to leave the root or to alias two guest paths onto one host file**,
/// on at least one of the five targets. They are applied on all five rather than under a `cfg`,
/// because a path that is refused on Windows and accepted on Linux would make the confinement
/// property depend on the host — and because `omni-platform` is the crate that owns this
/// knowledge, so the rules may as well be stated once.
#[must_use]
pub fn hostile_component(name: &str) -> Option<String> {
    if let Some(bad) = name.chars().find(|c| c.is_control()) {
        return Some(format!(
            "the component `{}` contains the control character {:#04x}",
            name.escape_debug(),
            u32::from(bad)
        ));
    }
    // `\` is a path separator on Windows, so `a\..\..\b` is a traversal the `/` split never sees.
    // `:` names a drive (`C:`) or an NTFS alternate data stream (`file:stream`). Both leave the
    // root outright.
    if let Some(bad) = name.chars().find(|c| matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')) {
        return Some(format!(
            "the component `{name}` contains `{bad}`, which is a path separator, a drive or \
             stream marker, or a wildcard on at least one of the five targets"
        ));
    }
    // Win32 strips a trailing dot or space from a path component, so `secret.` and `secret` are
    // the same file — two guest paths, one host file, and a guest that can create the second by
    // naming the first. A verbatim `\\?\` root does not strip them, which is a second layer, not
    // a reason to skip this one.
    if name.ends_with('.') || name.ends_with(' ') {
        return Some(format!(
            "the component `{name}` ends in a dot or a space, which Win32 strips — so it would \
             name the same host file as the component without it"
        ));
    }
    let stem = name.split('.').next().unwrap_or(name);
    if WINDOWS_DEVICES.iter().any(|device| stem.eq_ignore_ascii_case(device)) {
        return Some(format!(
            "the component `{name}` names the Windows character device `{}`, which is a device \
             in every directory rather than a file in this one",
            stem.to_ascii_uppercase()
        ));
    }
    None
}

/// Rules 5 and 6: walk the resolved components under `root`, refusing a symlink on the way.
///
/// `root` must already be canonical — [`Filesystem::new`](super::Filesystem::new) canonicalises
/// it once, at construction, so this is not re-done per call.
///
/// # Errors
///
/// [`FsError::Confined`] for a symlink, or if the built path somehow leaves the root.
pub fn locate(
    operation: &'static str,
    root: &Path,
    resolved: &Resolved,
    final_link: FinalLink,
) -> FsResult<PathBuf> {
    let mut host = root.to_path_buf();
    let last = resolved.components.len().saturating_sub(1);
    for (index, component) in resolved.components.iter().enumerate() {
        host.push(component);
        // `symlink_metadata` does not follow, so this asks about the component itself. A
        // component that does not exist yet is not an error here: `open(O_CREAT)`, `rename`'s
        // destination and `mkdir` all name something that is about to exist, and the call itself
        // reports `ENOENT` if a *parent* is missing.
        match std::fs::symlink_metadata(&host) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if index == last && final_link == FinalLink::Describe {
                    break;
                }
                return Err(FsError::confined(
                    operation,
                    resolved.guest_path(),
                    format!(
                        "`{component}` is a symbolic link. Following it would leave this layer \
                         unable to say where the path ends up, and nothing the guest can call \
                         creates one — so a link inside the root was put there by whoever \
                         populated it"
                    ),
                ));
            }
            _ => {}
        }
    }
    // Rules 3 and 4 already make this impossible. It is here because "already impossible" is what
    // every traversal defect in the literature was, and this costs one comparison.
    if !host.starts_with(root) {
        return Err(FsError::confined(
            operation,
            resolved.guest_path(),
            format!("the resolved host path `{}` is not inside the root", host.display()),
        ));
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Rule 6, reached directly — the one rule rules 3 and 4 are supposed to make unreachable.**
    ///
    /// A review found rules 5 and 6 had no mutation row and no test that executes on this host.
    /// This is rule 6's, and reaching it needs care, because the obvious attempt does not work:
    /// `starts_with` is **lexical**, so `root/../secret` still starts with `root` and a `..`
    /// component would sail straight past it.
    ///
    /// What rule 6 actually guards is `PathBuf::push` with an **absolute** component, which
    /// **replaces the whole path** rather than appending to it. That is a genuine Rust footgun:
    /// one absolute component anywhere in the list and the host path is no longer under the root
    /// at all. `hostile_component` rejects the shapes that produce one, so this builds the
    /// `Resolved` by hand — which is exactly the "rules 3 and 4 already make this impossible"
    /// case the comment on rule 6 describes, tested rather than asserted.
    #[test]
    fn an_absolute_component_is_refused_by_the_containment_check() {
        let root = std::path::Path::new(if cfg!(windows) { r"C:\omnidroid-root" } else { "/omnidroid-root" });
        let escape = if cfg!(windows) { r"C:\Windows" } else { "/etc" };

        // Built by hand: `hostile_component` would never let this through, which is the point.
        let resolved = Resolved { components: vec![escape.to_string(), "secret.txt".to_string()] };
        let error = locate("test", root, &resolved, FinalLink::Refuse)
            .expect_err("an absolute component replaces the path and leaves the root");
        assert!(
            matches!(error, FsError::Confined { .. }),
            "an escape must be Confined, not {error}",
        );

        // And the ordinary case still resolves, so the rule is not simply refusing everything.
        let ok = Resolved { components: vec!["data".to_string(), "f.txt".to_string()] };
        let host = locate("test", root, &ok, FinalLink::Refuse).expect("an ordinary path resolves");
        assert!(host.starts_with(root), "{host:?}");
    }

    fn components(path: &str) -> Vec<String> {
        resolve_lexically("test", path.as_bytes()).expect(path).components
    }

    /// The lexical rules, including every shape of `..` that would otherwise leave the root.
    ///
    /// **Enumerated rather than sampled.** A traversal defence tested with one `../` and no
    /// `....//` is the shape of every published path-traversal bug.
    #[test]
    fn no_arrangement_of_dot_dot_can_climb_above_the_root() {
        assert_eq!(components("/"), Vec::<String>::new());
        assert_eq!(components("/data/app"), vec!["data", "app"]);
        assert_eq!(components("data/app"), vec!["data", "app"], "relative resolves against the root");
        assert_eq!(components("/data//app"), vec!["data", "app"], "`//` collapses");
        assert_eq!(components("/data/./app"), vec!["data", "app"]);
        assert_eq!(components("/data/../app"), vec!["app"]);
        // Every one of these is a climb attempt, and every one lands at or below the root.
        for attempt in [
            "/..",
            "/../",
            "/../..",
            "/../../../../../../../../etc/passwd",
            "..",
            "../etc/passwd",
            "/data/../../etc/passwd",
            "/data/app/../../../../etc/passwd",
            "/./../.././../etc/passwd",
            "/data/..//../etc/passwd",
        ] {
            let got = components(attempt);
            assert!(
                !got.iter().any(|c| c == ".." || c == "."),
                "`{attempt}` resolved to {got:?}, which still contains a traversal component"
            );
            // And what it landed on is a path *inside* the root: the components are the ones
            // after the climb was absorbed, which is POSIX's own `/.. == /`.
            assert!(
                got.iter().all(|c| !c.is_empty()),
                "`{attempt}` resolved to {got:?}, which has an empty component"
            );
        }
        // The classic: it lands on `etc/passwd` *inside the root*, which is a file the host put
        // there or does not exist — never the host's own.
        assert_eq!(components("/../../../etc/passwd"), vec!["etc", "passwd"]);
        // `....//` is not a traversal component at all; it is an ordinary name, and rule 4
        // refuses it for the trailing dot.
        assert!(resolve_lexically("test", b"/....//x").is_err(), "a trailing-dot component");
    }

    /// The host-specific hazards, each refused with a reason.
    #[test]
    fn the_component_rules_refuse_every_way_to_name_something_outside_the_root() {
        // A backslash traversal the `/` split cannot see.
        assert!(hostile_component(r"..\..\windows").is_some());
        assert!(hostile_component(r"a\b").is_some());
        // Drive-relative and alternate data streams.
        assert!(hostile_component("C:").is_some());
        assert!(hostile_component("file:stream").is_some());
        // Wildcards, which some host APIs expand.
        for name in ["*", "?", "a*b", "a?b", "<", ">", "\"", "|"] {
            assert!(hostile_component(name).is_some(), "`{name}` was accepted");
        }
        // Windows character devices, in any directory and with any extension.
        for name in ["NUL", "nul", "Nul.txt", "CON", "aux", "COM1", "lpt9.log"] {
            assert!(hostile_component(name).is_some(), "`{name}` was accepted");
        }
        // But a name that merely starts like one is a file.
        for name in ["NULL", "console", "com", "com10", "auxiliary", "prnt"] {
            assert!(hostile_component(name).is_none(), "`{name}` was refused and is a real name");
        }
        // Trailing dot and space: Win32 strips them, so they alias.
        assert!(hostile_component("secret.").is_some());
        assert!(hostile_component("secret ").is_some());
        assert!(hostile_component("secret.txt").is_none());
        // Control characters.
        assert!(hostile_component("a\u{1}b").is_some());
        // Ordinary Android names are fine.
        for name in ["data", "libroblox.so", "com.roblox.client", "a-b_c.1", "ünïcode"] {
            assert!(hostile_component(name).is_none(), "`{name}` was refused");
        }
    }

    /// The length and encoding rules give the errno a real device gives.
    #[test]
    fn the_limits_are_the_guests_own_and_are_reported_as_such() {
        let long = vec![b'a'; PATH_MAX + 1];
        let error = resolve_lexically("test", &long).expect_err("longer than PATH_MAX");
        assert_eq!(error.kind(), Some(FsErrorKind::NameTooLong));
        let long_component = format!("/{}", "a".repeat(NAME_MAX + 1));
        let error = resolve_lexically("test", long_component.as_bytes()).expect_err("NAME_MAX");
        assert_eq!(error.kind(), Some(FsErrorKind::NameTooLong));
        assert_eq!(
            resolve_lexically("test", &[b'a'; NAME_MAX]).expect("NAME_MAX exactly").components.len(),
            1,
            "the limit is inclusive"
        );
        let empty = resolve_lexically("test", b"").expect_err("an empty path");
        assert_eq!(empty.kind(), Some(FsErrorKind::NotFound), "POSIX: an empty path is ENOENT");
        // Not UTF-8: a confinement refusal, because the conversion would be lossy.
        let error = resolve_lexically("test", &[b'/', 0xff, 0xfe]).expect_err("not UTF-8");
        assert!(matches!(error, FsError::Confined { .. }), "{error}");
        let error = resolve_lexically("test", b"/a\0b").expect_err("an embedded NUL");
        assert!(matches!(error, FsError::Confined { .. }), "{error}");
    }

    /// The guest-visible rendering of a resolved path, which every refusal quotes.
    #[test]
    fn a_resolved_path_renders_as_the_absolute_guest_path_it_became() {
        assert_eq!(Resolved { components: vec![] }.guest_path(), "/");
        assert_eq!(
            Resolved { components: vec!["data".into(), "x".into()] }.guest_path(),
            "/data/x"
        );
        assert_eq!(display(&[b'/', 0xff]), "/\u{fffd}", "a lossy rendering never fails");
    }
}
