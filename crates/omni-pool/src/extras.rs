//! **What a launch does beyond the install and the start** -- a plugin's part (the omnidroid app
//! asks its plugins, as `omnidroid aosp` does: `--cookie`, `--place` of the `roblox` plugin), done
//! for the instance's own user on its own display:
//!
//! * a **cookie store** -- the WebView `Cookies` database a signed-in app holds, made on the host --
//!   put in the app's data before it starts, that user's earlier data of it emptied first, so the app starts
//!   signed in as that account (whatever another launch left in that user);
//! * a **link** (`am start` arguments) opened on the instance's display once the device's log says
//!   the app is ready for it (or has signed in), sent again while nothing says it was taken.
//!
//! The pool knows no app: the markers and the link are the plugin's, as `omni_linux::app_session`
//! names them for a session of its own.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// A link not answered by [`Extras::link_taken`] in this long is sent again, at most
/// [`LINK_TRIES`] times (an app can answer a link by settling on its home screen).
pub const LINK_RETRY: Duration = Duration::from_secs(60);
pub const LINK_TRIES: u32 = 4;
/// How long a launch's link waits for its app to be ready, at most.
pub const LINK_WAIT: Duration = Duration::from_secs(15 * 60);

/// What makes a launch's [`Extras`] again for a restart: the plugin asked anew. A cookie store goes
/// once the instance's start has taken its copy (it holds an account's session), so a restart needs
/// a new one.
pub type Remake = Arc<dyn Fn() -> Result<Extras, String> + Send + Sync>;

/// What a launch does beyond the install and the start; nothing by default.
#[derive(Clone, Default)]
pub struct Extras {
    /// A cookie store (a host file) put in the app's WebView data before its first start. It holds
    /// an account's session: removed once every instance of the launch has taken its copy.
    pub cookie_store: Option<Arc<TempFile>>,
    /// The link's `am start` arguments (its id already in), opened once the app is ready.
    pub link: Option<String>,
    /// The log line that says the app can take the link (`OMNI_R_LINK_READY`).
    pub link_ready: Option<String>,
    /// The log line that says the app signed in (`OMNI_R_SIGNED_IN`): ready for the link too.
    pub signed_in: Option<String>,
    /// The log line that says the link was taken (`OMNI_R_LINK_TAKEN`); without one it is sent once.
    pub link_taken: Option<String>,
    /// What the plugins said of the launch, for the pool's log.
    pub said: Vec<String>,
    /// Makes these extras again for a restart ([`Remake`]); without it a restart does them again
    /// less the cookie store.
    pub remake: Option<Remake>,
}

/// By hand: a [`Remake`] is a closure, shown as there or not.
impl std::fmt::Debug for Extras {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extras")
            .field("cookie_store", &self.cookie_store)
            .field("link", &self.link)
            .field("link_ready", &self.link_ready)
            .field("signed_in", &self.signed_in)
            .field("link_taken", &self.link_taken)
            .field("said", &self.said)
            .field("remake", &self.remake.is_some())
            .finish()
    }
}

impl Extras {
    /// Whether there is anything to do beyond the install and the start.
    #[must_use]
    pub fn any(&self) -> bool {
        self.cookie_store.is_some() || self.link.is_some()
    }

    /// What an instance keeps of these for a restart: all but the cookie store -- an account's
    /// session, removed once the launch's starts took their copies, never held beyond that.
    #[must_use]
    pub fn kept(&self) -> Self {
        Self { cookie_store: None, ..self.clone() }
    }
}

/// A host file removed when the last holder lets it go.
#[derive(Debug)]
pub struct TempFile(PathBuf);

impl TempFile {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The guest shell (as root) that gives `user`'s `package` the cookie store handed over at `store`:
/// that user's data of it emptied (another account's sign-in must not stay beside it), the store
/// put where its WebView keeps cookies, owned by the app. Says `cookie store planted`; the store is
/// removed whatever happens.
///
/// Emptied by hand, not `pm clear --user`: clearing an app's data force-stops it for *every* user
/// (`clearApplicationUserData`, "user=-1"), which killed the same app's instance beside this one
/// (2026-10-13). `am force-stop --user` stops this user's alone.
#[must_use]
pub fn plant(package: &str, user: u32, store: &str) -> String {
    format!(
        "am force-stop --user {user} {package}; a=/data/user/{user}/{package}; \
         rm -rf $a/* $a/.[!.]* /data/user_de/{user}/{package}/* /data/user_de/{user}/{package}/.[!.]* 2>/dev/null; \
         uid=$(stat -c %u $a); d=$a/app_webview/Default; mkdir -p $d && \
         cp {store} $d/Cookies && chown -R $uid:$uid $a/app_webview && chmod 700 $a/app_webview $d && \
         chmod 600 $d/Cookies && echo \"cookie store planted\"; rm -f {store}; true"
    )
}

/// The command that opens `link` for `user` on the Android display `display`.
#[must_use]
pub fn link_command(link: &str, user: u32, display: i32) -> String {
    format!("am start --user {user} --display {display} {link}")
}

/// The user a device's log line names: an Activity's record's (`ActivityRecord{<hash> u<user> ...`),
/// an Activity's start's (`START u<user> {cmp=...}`) or an app's data's (`/data/user/<user>/`). Most
/// of an app's own lines name none.
#[must_use]
pub fn line_user(line: &str) -> Option<u32> {
    if let Some(rec) = line.split("ActivityRecord{").nth(1) {
        return rec.split_whitespace().nth(1)?.strip_prefix('u')?.parse().ok();
    }
    if let Some(rest) = line.split("START u").nth(1) {
        return rest.split_whitespace().next()?.parse().ok();
    }
    let rest = line.split("/data/user/").nth(1)?;
    rest.split('/').next()?.parse().ok()
}

/// Whether the device's log `line` holds `marker`, said of `user`'s app: a line naming another user
/// is another instance's; one naming none is this one's only when it is `alone` -- the one instance
/// of its app on the device a link is followed for (two Robloxes' sign-ins are one line each, and
/// the second took the first's for its own, 2026-10-13).
#[must_use]
pub fn says(line: &str, marker: Option<&str>, user: u32, alone: bool) -> bool {
    if !marker.is_some_and(|m| line.contains(m)) {
        return false;
    }
    line_user(line).map_or(alone, |u| u == user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cookie_goes_into_the_instance_s_own_user() {
        let s = plant("com.a", 11, "/data/local/tmp/omni-apk/c.db");
        assert!(s.contains("am force-stop --user 11 com.a"));
        assert!(!s.contains("pm clear"), "pm clear stops the app for every user");
        assert!(s.contains("a=/data/user/11/com.a;"));
        assert!(s.contains("rm -rf $a/* "));
        assert!(s.contains("cp /data/local/tmp/omni-apk/c.db $d/Cookies"));
        assert!(s.contains("rm -f /data/local/tmp/omni-apk/c.db"));
        assert!(s.find("rm -rf").unwrap() < s.find("cp ").unwrap(), "emptied before planted");
    }

    #[test]
    fn a_link_opens_on_the_instance_s_display_as_its_user() {
        assert_eq!(link_command("-a v -d \"x://1\" -n p/.L", 10, 3), "am start --user 10 --display 3 -a v -d \"x://1\" -n p/.L");
    }

    #[test]
    fn a_marker_in_another_user_s_activity_is_not_this_instance_s() {
        let line = "I/ActivityTaskManager( 1): Displayed ActivityRecord{1a2b u10 com.a/.Main t4}";
        assert!(says(line, Some("com.a/.Main"), 10, false));
        assert!(!says(line, Some("com.a/.Main"), 11, true));
        assert!(says("I/rbx: DID_LOG_IN", Some("DID_LOG_IN"), 11, true), "a line naming no user: the one following's");
        assert!(!says("I/rbx: DID_LOG_IN", Some("DID_LOG_IN"), 11, false), "... and no other's");
        assert!(says("I/Roblox(1): written to /data/user/11/com.a/x DID_LOG_IN", Some("DID_LOG_IN"), 11, false));
        assert!(!says("I/Roblox(1): written to /data/user/10/com.a/x DID_LOG_IN", Some("DID_LOG_IN"), 11, true));
        assert!(!says("I/rbx: DID_LOG_IN", None, 11, true));
        assert!(!says("I/rbx: other", Some("DID_LOG_IN"), 11, true));
        assert_eq!(line_user("x ActivityRecord{1a u0 p/.A t1}"), Some(0));
        assert_eq!(line_user("nothing here"), None);
        let start = "I/ActivityTaskManager( 1049): START u10 {cmp=com.roblox.client/.ActivityNativeMain} with LAUNCH_SINGLE_TASK";
        assert_eq!(line_user(start), Some(10));
        assert!(says(start, Some("com.roblox.client/.ActivityNativeMain}"), 10, false), "the ready line names its user");
    }

    #[test]
    fn a_temporary_file_goes_with_its_last_holder() {
        let path = std::env::temp_dir().join(format!("omni-pool-extras-{}", std::process::id()));
        std::fs::write(&path, b"secret").unwrap();
        let one = Arc::new(TempFile::new(path.clone()));
        let two = Arc::clone(&one);
        drop(one);
        assert!(path.exists());
        drop(two);
        assert!(!path.exists());
    }
}
