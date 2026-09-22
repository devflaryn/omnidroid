//! **The substitution log: every extension name this layer changed, in both directions.**
//!
//! # The defect this file exists to make impossible
//!
//! The guest is an Android binary and will look for `VK_KHR_android_surface`. The host is not
//! Android and has something else — `VK_KHR_win32_surface` here, and the host is what names it
//! ([`VulkanHost::platform_surface_extension`](super::VulkanHost::platform_surface_extension)).
//! The engine will not go near a Vulkan surface unless the Android extension is *advertised*, so
//! this layer has to say the Android name where the driver said the platform one, and has to send
//! the platform name to the driver where the engine wrote the Android one.
//!
//! That is a **rewrite, not a passthrough**, and the handoff's stage 2 list says so in the same
//! breath as naming it: *"a silent rename is the defect class Global Constraint 1 exists for —
//! each needs its own recorded rewrite log."* A rename that leaves no trace is worse than a
//! refusal, because every symptom of it appears somewhere else: an engine that enabled an
//! extension whose real semantics it has never been told about, a surface created through a code
//! path it did not choose, and a report that says "Vulkan initialised" either way.
//!
//! So the rewrite is a value. [`apply_advertised`] and [`apply_enabled`] are pure functions that
//! return the rewritten list **and** a [`Rewrite`] for every name they touched;
//! [`Vulkan::rewrites`](super::Vulkan::rewrites) is how a host reads them, exactly the way
//! [`Vulkan::requests`](super::Vulkan::requests) exposes the census;
//! [`Vulkan::report`](super::Vulkan::report) prints them; and the tests assert on them rather than
//! on the returned list alone — a list that came out right by coincidence would pass the first
//! check and fail the second.
//!
//! # Replacement rather than addition, and why that is the honest one
//!
//! [`apply_advertised`] **replaces** the host name with the guest name. It does not append the
//! guest name to the driver's list. The alternative was tempting and is wrong: with both present,
//! the guest can see and enable `VK_KHR_win32_surface` by its real name, which is a second and
//! completely different code path through the engine's own platform abstraction, reached by
//! accident. Replacing keeps the invariant that matters — **one advertisement per real host
//! capability** — so the count the guest reads back is the driver's own count and the extension it
//! finds is the one it knows how to use.
//!
//! # What is carried through rather than invented
//!
//! `specVersion` is the **driver's**, unchanged, and the [`Rewrite`] records it. `VK_KHR_surface`
//! is version 25 and both platform surface extensions are version 6 today, so the number happens
//! to be right — but "happens to be right" is not a claim this file makes. What it does is record
//! the number beside both names, so that a reader can check whether the version the guest saw
//! belongs to the name the guest saw. Inventing the Android extension's own published version
//! would be this layer asserting a fact about a driver it is not.

use omni_mem::GuestAddr;

use super::host::HostExtension;

/// Which direction a substitution went.
///
/// Two named variants rather than a boolean, for [`WindowBacking`](crate::ndk::WindowBacking)'s
/// reason: the two are easy to confuse in a log and impossible to confuse in a type. They are also
/// not symmetrical — one changes what the guest is *told exists*, the other changes what the
/// driver is *asked to enable* — and a report that could not tell them apart would leave a reader
/// unable to say whether the engine ever acted on what it was told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RewriteSite {
    /// `vkEnumerateInstanceExtensionProperties`: the driver's name was replaced by the guest's on
    /// the way **out**, so the engine finds the extension it is looking for.
    Advertised,
    /// `vkCreateInstance`: the guest's name was replaced by the driver's on the way **in**, so the
    /// driver is asked for the extension it actually has.
    Enabled,
    /// `vkGetInstanceProcAddr`: the guest asked for `vkCreateAndroidSurfaceKHR`, the driver does
    /// not have it, and this layer handed out a thunk anyway — because **this layer** implements
    /// it, through the host call named in [`Rewrite::to`].
    ///
    /// # Why a third site rather than an extra [`RewriteSite::Advertised`]
    ///
    /// Because it is a different kind of claim and it is the one that is hardest to see. The two
    /// rename sites change a *string* in a list; this one hands the engine a **function pointer**
    /// for a command the driver answered `false` for, and the engine will branch to it. That is
    /// the only place in this layer where an answer contradicts the driver, and a log that spelled
    /// it like an extension rename would bury the one line a reviewer most needs to find.
    ///
    /// [`Rewrite::spec_version`] is `None`: a command has no version.
    Resolved,
    /// `vkCreateAndroidSurfaceKHR`: the guest's **call** was satisfied by a different platform's
    /// call, over the host window behind the guest's `ANativeWindow *`.
    ///
    /// # Why this is in the same log as the two renames
    ///
    /// Because it is the thing the two renames were *for*, and it is the only one of the three
    /// that creates an object. The advertised rename makes the engine believe
    /// `VK_KHR_android_surface` exists; the enabled rename makes the driver enable something else;
    /// and this is where a `VkSurfaceKHR` comes into existence through an entry point the engine
    /// never named. A reader following the substitution from "the extension was advertised" to
    /// "the surface exists" should read three consecutive ordinals, not two and an absence.
    ///
    /// `system` is [`RawWindow::system_name`](omni_platform::window::RawWindow::system_name) —
    /// `"win32"` here — and it is carried because [`Rewrite::to`] alone does not say *which
    /// window system's* window the surface ended up over, and a host with two backends could
    /// answer the same call name over either.
    SurfaceCall {
        /// The windowing system the `ANativeWindow *` resolved to.
        system: &'static str,
    },
}

/// One name this layer changed, with both spellings and where it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    /// Its position in the whole ordered sequence, counting rewrites that were dropped once the
    /// log was full. Not an index into [`Vulkan::rewrites`](super::Vulkan::rewrites) — a truncated
    /// log that renumbered would look complete.
    pub order: usize,
    /// Which call did it, and in which direction.
    pub site: RewriteSite,
    /// The name that was there.
    pub from: String,
    /// The name that was put in its place.
    pub to: String,
    /// The `specVersion` that travelled with the name, for [`RewriteSite::Advertised`].
    ///
    /// `None` for [`RewriteSite::Enabled`], where the guest supplies a bare name and there is no
    /// version to carry. This module's header says why it is recorded rather than adjusted.
    pub spec_version: Option<u32>,
    /// The guest address the call would have returned to. [`ImportCall::caller`]'s standing: it is
    /// a guest value, and it is here so a report can point at the instruction that caused this.
    ///
    /// [`ImportCall::caller`]: crate::boundary::ImportCall::caller
    pub caller: GuestAddr,
}

impl core::fmt::Display for Rewrite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let arrow = match self.site {
            RewriteSite::Advertised => "advertised to the guest as".to_string(),
            RewriteSite::Enabled => "sent to the driver as".to_string(),
            RewriteSite::Resolved => {
                "resolved to a thunk this layer satisfies with the host's".to_string()
            }
            RewriteSite::SurfaceCall { system } => {
                format!("satisfied on this host's {system} window by")
            }
        };
        write!(f, "[{order}] \"{from}\" {arrow} \"{to}\"", order = self.order, from = self.from, to = self.to)?;
        if let Some(version) = self.spec_version {
            write!(f, " (specVersion {version}, the driver's)")?;
        }
        write!(f, " (from {caller:#x})", caller = self.caller)
    }
}

/// One substitution, as a pair of names.
///
/// Built at run time from [`GUEST_SURFACE_EXTENSION`](super::GUEST_SURFACE_EXTENSION) and whatever
/// the host answered for
/// [`platform_surface_extension`](super::VulkanHost::platform_surface_extension), rather than
/// being a `const` table here — this crate must name no OS, and `"VK_KHR_win32_surface"` is an OS
/// name. See that method's documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Substitution {
    /// What the guest says and looks for.
    pub guest: String,
    /// What the host has instead.
    pub host: String,
}

/// Rewrite the driver's extension list into the one the guest is shown.
///
/// Returns the list in the **driver's order**, with any entry whose name is
/// [`Substitution::host`] renamed to [`Substitution::guest`], and one [`Rewrite`] per rename.
///
/// # The case where nothing is rewritten, and why it is not an error
///
/// If the driver already reports [`Substitution::guest`] — a real Android device would, and so
/// would a host whose loader exposes the Android surface extension — then nothing is substituted
/// and **no rewrite is recorded**, because none happened. The substitution is skipped entirely in
/// that case rather than applied and then deduplicated: applying it would produce the guest name
/// twice, and a guest that read the list back would find one extension advertised under one name
/// with two different `specVersion`s.
pub fn apply_advertised(
    driver: &[HostExtension],
    substitution: &Substitution,
    caller: GuestAddr,
    next_order: &mut usize,
) -> (Vec<HostExtension>, Vec<Rewrite>) {
    let already = driver.iter().any(|extension| extension.name == substitution.guest);
    let mut out = Vec::with_capacity(driver.len());
    let mut rewrites = Vec::new();
    for extension in driver {
        if already || extension.name != substitution.host {
            out.push(extension.clone());
            continue;
        }
        rewrites.push(Rewrite {
            order: *next_order,
            site: RewriteSite::Advertised,
            from: extension.name.clone(),
            to: substitution.guest.clone(),
            spec_version: Some(extension.spec_version),
            caller,
        });
        *next_order += 1;
        out.push(HostExtension {
            name: substitution.guest.clone(),
            spec_version: extension.spec_version,
        });
    }
    (out, rewrites)
}

/// Rewrite the list the guest asked to enable into the one the driver is asked for.
///
/// Returns the list in the **guest's order** — the driver is told to enable the extensions in the
/// order the engine wrote them, because an engine that logs what it enabled and a driver that logs
/// what it was asked for should produce lists a reader can put side by side.
///
/// Every occurrence is rewritten, not only the first: a guest that named the extension twice would
/// otherwise send the driver one name it has and one it does not, and the driver would answer
/// `VK_ERROR_EXTENSION_NOT_PRESENT` for a request this layer had already agreed to honour.
pub fn apply_enabled(
    requested: &[String],
    substitution: &Substitution,
    caller: GuestAddr,
    next_order: &mut usize,
) -> (Vec<String>, Vec<Rewrite>) {
    let mut out = Vec::with_capacity(requested.len());
    let mut rewrites = Vec::new();
    for name in requested {
        if name != &substitution.guest {
            out.push(name.clone());
            continue;
        }
        rewrites.push(Rewrite {
            order: *next_order,
            site: RewriteSite::Enabled,
            from: name.clone(),
            to: substitution.host.clone(),
            spec_version: None,
            caller,
        });
        *next_order += 1;
        out.push(substitution.host.clone());
    }
    (out, rewrites)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn substitution() -> Substitution {
        Substitution {
            guest: "VK_KHR_android_surface".to_string(),
            host: "VK_KHR_win32_surface".to_string(),
        }
    }

    fn ext(name: &str, spec_version: u32) -> HostExtension {
        HostExtension { name: name.to_string(), spec_version }
    }

    /// **The driver's list comes out in the driver's order, with one name changed and the change
    /// recorded.**
    ///
    /// Order and membership rather than a length (`VERIFICATION.md` entry 1): a rewrite that
    /// reordered the list would pass a count and would break a guest that indexes the array it was
    /// handed.
    #[test]
    fn the_platform_surface_extension_is_advertised_under_the_guest_s_name() {
        let driver = [
            ext("VK_KHR_surface", 25),
            ext("VK_KHR_win32_surface", 6),
            ext("VK_EXT_debug_utils", 2),
        ];
        let mut order = 0;
        let (shown, rewrites) = apply_advertised(&driver, &substitution(), 0x2595, &mut order);

        let names: Vec<&str> = shown.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["VK_KHR_surface", "VK_KHR_android_surface", "VK_EXT_debug_utils"]);
        assert_eq!(shown.len(), driver.len(), "replacement, not addition");
        assert!(
            !names.contains(&"VK_KHR_win32_surface"),
            "the host name must not also be advertised: a guest that enabled it would take a \
             different path through its own platform layer, by accident"
        );

        assert_eq!(rewrites.len(), 1, "one name changed, one record: {rewrites:?}");
        assert_eq!(rewrites[0].site, RewriteSite::Advertised);
        assert_eq!(rewrites[0].from, "VK_KHR_win32_surface");
        assert_eq!(rewrites[0].to, "VK_KHR_android_surface");
        assert_eq!(rewrites[0].spec_version, Some(6), "the driver's version, carried not invented");
        assert_eq!(rewrites[0].caller, 0x2595);
        assert_eq!(order, 1, "the ordinal advances for the next rewrite");

        // The version travelled with the name rather than being left on the old row.
        let renamed = shown.iter().find(|e| e.name == "VK_KHR_android_surface").expect("renamed");
        assert_eq!(renamed.spec_version, 6);
    }

    /// **A driver that already has the guest's extension is left alone, and nothing is recorded.**
    ///
    /// The clause that stops the log from claiming a rewrite that did not happen — which matters
    /// because a test asserting "one rewrite" would otherwise pass on a real Android device, where
    /// there is nothing to rewrite at all.
    #[test]
    fn a_driver_that_already_has_the_guest_extension_is_not_rewritten() {
        let driver = [ext("VK_KHR_surface", 25), ext("VK_KHR_android_surface", 6)];
        let mut order = 0;
        let (shown, rewrites) = apply_advertised(&driver, &substitution(), 0, &mut order);
        assert_eq!(shown, driver, "nothing changed");
        assert!(rewrites.is_empty(), "and nothing is claimed: {rewrites:?}");
        assert_eq!(order, 0);
    }

    /// **A host list with neither name passes through untouched**, so the rewrite is scoped to the
    /// one pair and not to "whatever looked like a surface extension".
    #[test]
    fn an_unrelated_list_is_untouched() {
        let driver = [ext("VK_KHR_surface", 25), ext("VK_KHR_xlib_surface", 6)];
        let mut order = 7;
        let (shown, rewrites) = apply_advertised(&driver, &substitution(), 0, &mut order);
        assert_eq!(shown, driver);
        assert!(rewrites.is_empty());
        assert_eq!(order, 7, "an untouched list does not consume an ordinal");
    }

    /// **What the guest asks to enable is sent to the driver under the driver's name, every
    /// occurrence, in the guest's order.**
    #[test]
    fn the_guest_s_enabled_list_is_sent_to_the_driver_in_host_spelling() {
        let requested = [
            "VK_KHR_surface".to_string(),
            "VK_KHR_android_surface".to_string(),
            "VK_KHR_android_surface".to_string(),
        ];
        let mut order = 3;
        let (sent, rewrites) = apply_enabled(&requested, &substitution(), 0x25951c8, &mut order);
        assert_eq!(
            sent,
            vec![
                "VK_KHR_surface".to_string(),
                "VK_KHR_win32_surface".to_string(),
                "VK_KHR_win32_surface".to_string(),
            ],
            "the guest's order, with both occurrences rewritten"
        );
        assert_eq!(rewrites.len(), 2, "two occurrences, two records");
        assert_eq!(rewrites[0].order, 3, "the ordinal continues the shared sequence");
        assert_eq!(rewrites[1].order, 4);
        assert_eq!(order, 5);
        assert!(rewrites.iter().all(|r| r.site == RewriteSite::Enabled));
        assert!(rewrites.iter().all(|r| r.spec_version.is_none()), "a bare name has no version");
    }

    /// **A guest that asks for the host's own name by hand is not rewritten**, because there is
    /// nothing to change — and the log says so by being empty rather than by recording a no-op.
    #[test]
    fn a_guest_that_names_the_host_extension_itself_is_passed_through() {
        let requested = ["VK_KHR_win32_surface".to_string()];
        let mut order = 0;
        let (sent, rewrites) = apply_enabled(&requested, &substitution(), 0, &mut order);
        assert_eq!(sent, requested);
        assert!(rewrites.is_empty());
    }

    /// The two directions **print differently**, because a report that could not tell them apart
    /// would leave a reader unable to say whether the engine acted on what it was told.
    #[test]
    fn the_two_directions_print_differently() {
        let advertised = Rewrite {
            order: 0,
            site: RewriteSite::Advertised,
            from: "VK_KHR_win32_surface".to_string(),
            to: "VK_KHR_android_surface".to_string(),
            spec_version: Some(6),
            caller: 0x2595,
        };
        let enabled = Rewrite {
            order: 1,
            site: RewriteSite::Enabled,
            from: "VK_KHR_android_surface".to_string(),
            to: "VK_KHR_win32_surface".to_string(),
            spec_version: None,
            caller: 0x2596,
        };
        let surface = Rewrite {
            order: 2,
            site: RewriteSite::SurfaceCall { system: "win32" },
            from: "vkCreateAndroidSurfaceKHR".to_string(),
            to: "vkCreateWin32SurfaceKHR".to_string(),
            spec_version: None,
            caller: 0x2597,
        };
        let a = advertised.to_string();
        let b = enabled.to_string();
        let c = surface.to_string();
        assert!(a.contains("advertised to the guest as"), "{a}");
        assert!(a.contains("specVersion 6"), "{a}");
        assert!(b.contains("sent to the driver as"), "{b}");
        assert!(!b.contains("specVersion"), "{b}");
        assert!(c.contains("satisfied on this host's win32 window by"), "{c}");
        assert!(c.contains("vkCreateWin32SurfaceKHR"), "{c}");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    /// **The surface substitution is a third kind, not a second `Enabled`.**
    ///
    /// The three sites answer three different questions — what the guest was *told exists*, what
    /// the driver was *asked to enable*, and what call was *made instead* — and a reader who could
    /// not tell the third from the second would be unable to say whether a surface had ever been
    /// created at all. `RewriteSite` is `Copy` and carries a `&'static str`, so putting the
    /// window system in it costs nothing a log has to allocate.
    #[test]
    fn the_surface_site_is_distinct_from_both_rename_sites() {
        let surface = RewriteSite::SurfaceCall { system: "win32" };
        assert_ne!(surface, RewriteSite::Advertised);
        assert_ne!(surface, RewriteSite::Enabled);
        assert_eq!(surface, RewriteSite::SurfaceCall { system: "win32" });
        assert_ne!(surface, RewriteSite::SurfaceCall { system: "wayland" });
    }
}
