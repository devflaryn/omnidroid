//! What an app session (`omnidroid aosp`: the cold session `tests/r_app.rs` and the warm one,
//! `omnidroid`'s `warm.rs`) does beyond installing the APK and starting its launcher -- none of it
//! by default, all of it named by whoever starts the session (a plugin, `plugins/README.md`). The
//! launcher itself knows no app: every APK is installed, started from its launcher Activity and
//! watched until its window is on the display.
//!
//! | variable | meaning |
//! |---|---|
//! | `OMNI_R_COOKIE` | a cookie file put into the app's WebView cookie store before its first start (`tools/plant_cookie.py`) |
//! | `OMNI_R_LINK` | the `am start` arguments of a link the app takes; `{id}` is replaced with the link's id |
//! | `OMNI_R_LINK_ID` | the id to open once the app is ready (`OMNI_R_LINK_READY`, else `OMNI_R_SIGNED_IN`, else at once) |
//! | `OMNI_R_LINK_READY` | a log text: the earliest moment the link is taken (e.g. the app's main Activity started) |
//! | `OMNI_R_SIGNED_IN` | a log text: the app has signed in |
//! | `OMNI_R_LINK_TAKEN` | a log text: the app took the link (a link not answered by it is sent again, at most four times; unset: sent once) |
//! | `OMNI_R_LOADED` | a log text followed by the loaded id's digits: what the link opened is loaded |
//! | `OMNI_R_DISCONNECTED` | a log text: the app's server ended the session (a standby device opens the link again) |
//! | `OMNI_R_LOCALE` | the device's language (default `en-US`) |

/// The session's options, read once from the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppSession {
    /// `OMNI_R_LINK`: the link's `am start` arguments, `{id}` not yet replaced.
    pub link: Option<String>,
    /// `OMNI_R_LINK_ID`.
    pub link_id: Option<String>,
    /// `OMNI_R_LINK_READY`.
    pub link_ready: Option<String>,
    /// `OMNI_R_SIGNED_IN`.
    pub signed_in: Option<String>,
    /// `OMNI_R_LINK_TAKEN`.
    pub link_taken: Option<String>,
    /// `OMNI_R_LOADED`.
    pub loaded: Option<String>,
    /// `OMNI_R_DISCONNECTED`.
    pub disconnected: Option<String>,
}

/// The device's language when nothing names one.
pub const DEFAULT_LOCALE: &str = "en-US";

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `OMNI_R_LOCALE`, else [`DEFAULT_LOCALE`].
#[must_use]
pub fn locale() -> String {
    var("OMNI_R_LOCALE").unwrap_or_else(|| DEFAULT_LOCALE.to_string())
}

impl AppSession {
    /// From the environment (`OMNI_R_*`, the module's table).
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            link: var("OMNI_R_LINK"),
            link_id: var("OMNI_R_LINK_ID"),
            link_ready: var("OMNI_R_LINK_READY"),
            signed_in: var("OMNI_R_SIGNED_IN"),
            link_taken: var("OMNI_R_LINK_TAKEN"),
            loaded: var("OMNI_R_LOADED"),
            disconnected: var("OMNI_R_DISCONNECTED"),
        }
    }

    /// The `am start` command that opens the link with `id` (a value or a shell variable such as
    /// `$id`), when the session has a link.
    #[must_use]
    pub fn link_command(&self, id: &str) -> Option<String> {
        self.link.as_ref().map(|l| format!("am start {}", l.replace("{id}", id)))
    }

    /// The id loaded, when `line` says what the link opened has loaded (`OMNI_R_LOADED`).
    #[must_use]
    pub fn loaded_id(&self, line: &str) -> Option<String> {
        let marker = self.loaded.as_deref()?;
        let at = line.find(marker)?;
        Some(line[at + marker.len()..].chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect())
    }
}

/// Whether `line` holds `marker`, when there is one.
#[must_use]
pub fn says(line: &str, marker: Option<&str>) -> bool {
    marker.is_some_and(|m| line.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_is_the_session_s_own_and_takes_its_id() {
        let none = AppSession::default();
        assert_eq!(none.link_command("1"), None, "no link unless one is named");
        let s = AppSession { link: Some("-a android.intent.action.VIEW -d \"app://open?id={id}\" -n p/.Link".into()), ..AppSession::default() };
        assert_eq!(s.link_command("42").unwrap(), "am start -a android.intent.action.VIEW -d \"app://open?id=42\" -n p/.Link");
        assert_eq!(s.link_command("$id").unwrap(), "am start -a android.intent.action.VIEW -d \"app://open?id=$id\" -n p/.Link");
    }

    #[test]
    fn the_loaded_id_follows_its_marker() {
        let s = AppSession { loaded: Some("loaded placeId:".into()), ..AppSession::default() };
        assert_eq!(s.loaded_id("I/App(12): loaded placeId: 8737899170, ok").as_deref(), Some("8737899170"));
        assert_eq!(s.loaded_id("something else"), None);
        assert_eq!(AppSession::default().loaded_id("loaded placeId: 1"), None);
        assert!(says("a b c", Some("b")) && !says("a b c", None) && !says("a", Some("b")));
    }
}
