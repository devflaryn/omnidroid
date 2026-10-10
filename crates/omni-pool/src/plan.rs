//! **Where a launch goes, and which devices idle out** -- pure decisions over a snapshot of the
//! pool, so every rule is a test.
//!
//! An APK is known by its bytes (SHA-256), not its package and version: two builds of one package
//! and version are two APKs. One device holds one build of a package (PackageManager keeps one code
//! path per package name), and runs it as many times as it has Android users for it -- user 0 and
//! one more user per further instance, each on a display of its own. So a launch goes, in order:
//!
//! 1. to a device that has **this build** installed: one more instance of it there;
//! 2. to a device that has **another build** of the package installed **and running none of it**:
//!    that build is replaced by this one;
//! 3. to a device that has **no build** of the package: installed beside its other apps;
//! 4. else to **a new device**, booted for it (a device running another build of the package is
//!    never touched).
//!
//! A ready device is preferred over one still booting in each step.

use std::collections::BTreeMap;
use std::time::Duration;

/// A device's id in the pool.
pub type DeviceId = u32;

/// What a placement needs to know of one device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceView {
    pub id: DeviceId,
    /// Up and taking apps (else still booting).
    pub ready: bool,
    /// Package -> the SHA-256 of the build installed (or being installed).
    pub installed: BTreeMap<String, String>,
    /// The packages its instances run, one entry per instance (launches under way included).
    pub running: Vec<String>,
    /// Being closed: takes nothing new.
    pub closing: bool,
}

/// Where a launch goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// This build is installed there: one more instance.
    Run(DeviceId),
    /// Another build is installed there and not running: replaced by this one.
    Replace(DeviceId),
    /// No build of the package there: installed beside its other apps.
    Install(DeviceId),
    /// A new device, booted for it.
    Boot,
}

/// Where a launch of `package` built as `sha` goes (see the module's order).
#[must_use]
pub fn place(devices: &[DeviceView], package: &str, sha: &str) -> Place {
    // Ready devices first, then booting ones, each in pool order.
    let mut order: Vec<&DeviceView> = devices.iter().filter(|d| !d.closing).collect();
    order.sort_by_key(|d| (!d.ready, d.id));
    if let Some(d) = order.iter().find(|d| d.installed.get(package).is_some_and(|s| s == sha)) {
        return Place::Run(d.id);
    }
    if let Some(d) = order.iter().find(|d| d.installed.get(package).is_some_and(|s| s != sha) && !d.running.iter().any(|p| p == package)) {
        return Place::Replace(d.id);
    }
    if let Some(d) = order.iter().find(|d| !d.installed.contains_key(package)) {
        return Place::Install(d.id);
    }
    Place::Boot
}

/// What the idle rule needs to know of one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleView {
    pub id: DeviceId,
    pub ready: bool,
    /// Its instances (launches under way included).
    pub running: usize,
    /// How long it has run no app (`None` while it runs one).
    pub idle_for: Option<Duration>,
}

/// The devices to close now: those idle `idle_minutes` or longer (never, when 0), longest idle
/// first, as long as `keep_warm` idle devices stay.
#[must_use]
pub fn to_close(devices: &[IdleView], idle_minutes: u64, keep_warm: u32) -> Vec<DeviceId> {
    if idle_minutes == 0 {
        return Vec::new();
    }
    let idle: Vec<&IdleView> = devices.iter().filter(|d| d.ready && d.running == 0).collect();
    let spare = idle.len().saturating_sub(keep_warm as usize);
    let mut overdue: Vec<&IdleView> = idle.into_iter().filter(|d| d.idle_for.is_some_and(|t| t >= Duration::from_secs(idle_minutes * 60))).collect();
    overdue.sort_by(|a, b| b.idle_for.cmp(&a.idle_for).then(a.id.cmp(&b.id)));
    overdue.into_iter().take(spare).map(|d| d.id).collect()
}

/// How many devices to boot now so that `keep_warm` idle ones are up or coming (none past
/// `max_devices`, when it is not 0). Booting ones count as coming.
#[must_use]
pub fn to_warm(devices: &[IdleView], keep_warm: u32, max_devices: u32) -> u32 {
    let coming = devices.iter().filter(|d| !d.ready || d.running == 0).count() as u32;
    let want = keep_warm.saturating_sub(coming);
    if max_devices == 0 {
        want
    } else {
        want.min(max_devices.saturating_sub(devices.len() as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: DeviceId, ready: bool, installed: &[(&str, &str)], running: &[&str]) -> DeviceView {
        DeviceView {
            id,
            ready,
            installed: installed.iter().map(|(p, s)| ((*p).to_string(), (*s).to_string())).collect(),
            running: running.iter().map(|p| (*p).to_string()).collect(),
            closing: false,
        }
    }

    #[test]
    fn another_package_goes_into_the_warm_device() {
        let pool = [dev(1, true, &[("game.a", "A1")], &["game.a"])];
        assert_eq!(place(&pool, "game.b", "B1"), Place::Install(1));
    }

    #[test]
    fn the_same_build_runs_again_on_the_device_that_has_it() {
        let pool = [dev(1, true, &[("game.b", "B1")], &[]), dev(2, true, &[("game.a", "A1")], &["game.a", "game.a"])];
        assert_eq!(place(&pool, "game.a", "A1"), Place::Run(2), "a third instance beside the two");
    }

    #[test]
    fn another_build_running_needs_another_device_but_one_not_running_is_replaced() {
        // The same package and version, other bytes: another APK.
        let running = [dev(1, true, &[("game.a", "A1")], &["game.a"])];
        assert_eq!(place(&running, "game.a", "A2"), Place::Boot, "the running build is never touched");
        let idle = [dev(1, true, &[("game.a", "A1")], &[])];
        assert_eq!(place(&idle, "game.a", "A2"), Place::Replace(1));
        let both = [dev(1, true, &[("game.a", "A1")], &["game.a"]), dev(2, true, &[("other", "O")], &[])];
        assert_eq!(place(&both, "game.a", "A2"), Place::Install(2), "a device without the package before a new one");
    }

    #[test]
    fn three_of_build_a_and_five_of_build_b_take_two_devices() {
        let mut pool: Vec<DeviceView> = Vec::new();
        let mut next = 1;
        let mut launch = |pool: &mut Vec<DeviceView>, sha: &str| {
            let at = match place(pool, "game", sha) {
                Place::Run(id) | Place::Install(id) | Place::Replace(id) => id,
                Place::Boot => {
                    pool.push(dev(next, true, &[], &[]));
                    next += 1;
                    next - 1
                }
            };
            let d = pool.iter_mut().find(|d| d.id == at).unwrap();
            d.installed.insert("game".into(), sha.into());
            d.running.push("game".into());
        };
        for sha in ["A", "A", "A", "B", "B", "B", "B", "B"] {
            launch(&mut pool, sha);
        }
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.iter().map(|d| d.running.len()).collect::<Vec<_>>(), [3, 5]);
    }

    #[test]
    fn a_ready_device_is_taken_before_a_booting_one_and_a_closing_one_never() {
        let pool = [dev(1, false, &[], &[]), dev(2, true, &[], &[])];
        assert_eq!(place(&pool, "x", "X"), Place::Install(2));
        let mut closing = dev(3, true, &[("x", "X")], &[]);
        closing.closing = true;
        assert_eq!(place(&[closing], "x", "X"), Place::Boot);
        assert_eq!(place(&[dev(1, false, &[], &[])], "x", "X"), Place::Install(1), "the device warming now");
    }

    fn idle(id: DeviceId, minutes: Option<u64>, running: usize) -> IdleView {
        IdleView { id, ready: true, running, idle_for: minutes.map(|m| Duration::from_secs(m * 60)) }
    }

    #[test]
    fn idle_devices_close_after_the_timeout_but_one_stays_warm() {
        let pool = [idle(1, Some(9), 0), idle(2, Some(6), 0), idle(3, None, 2)];
        assert_eq!(to_close(&pool, 5, 1), [1], "the longest idle goes, one idle stays");
        assert_eq!(to_close(&pool, 5, 0), [1, 2]);
        assert_eq!(to_close(&pool, 10, 0), Vec::<DeviceId>::new(), "not yet");
        assert_eq!(to_close(&pool, 0, 0), Vec::<DeviceId>::new(), "0: never");
        assert_eq!(to_close(&[idle(1, Some(60), 0)], 5, 1), Vec::<DeviceId>::new(), "the last one stays");
    }

    #[test]
    fn a_device_is_warmed_when_none_is_idle() {
        assert_eq!(to_warm(&[], 1, 0), 1, "the app opens");
        assert_eq!(to_warm(&[idle(1, None, 3)], 1, 0), 1, "the only device is busy");
        assert_eq!(to_warm(&[idle(1, Some(1), 0)], 1, 0), 0);
        assert_eq!(to_warm(&[IdleView { id: 1, ready: false, running: 0, idle_for: None }], 1, 0), 0, "one is booting");
        assert_eq!(to_warm(&[idle(1, None, 3)], 1, 1), 0, "the limit");
    }
}
