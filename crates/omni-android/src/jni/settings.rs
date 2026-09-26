//! **The client-settings document `nativeInitClientSettings` is handed: fetched as the Java side
//! fetches it, kept, and chosen.**
//!
//! DECODED from `classes2.dex` (the modified 2.739.691 build, 2026-09-24). The downcall's caller
//! is the `AsyncTask` `fi.e$f`:
//!
//! ```text
//! fi.e$f.a (doInBackground):
//!   0000: invoke-static com.roblox.client.startup.a.t()   -> b$b, the prefetched response
//!   0004: b$b.d() -> v0 (the BODY)        0008: b$b.e() -> v5 (its X-Signature-Ed25519)
//!   000c: if v0 == null || v0.isEmpty() -> 006d: return 1       <- no native call at all
//!   004d: ci.i.G5() ? nativeInitClientSettingsSigned(v0, v5, v1, bh.x0.M())
//!                   : nativeInitClientSettings(v0, v1, bh.x0.M())
//! fi.e$f.b (onPostExecute):
//!   0004: if result != 0 -> skip
//!   000a: jk.l2.a(context) -> nativePostClientSettingsLoadedInitialization3(list)
//! ```
//!
//! The body is what OkHttp got from `clientsettingscdn.roblox.com/v2/settings/application/`
//! followed by `bh.x0.M()` (`"GoogleAndroidApp"`) -- plus `/bucket/<bh.x0.W()>` when that channel
//! is non-empty, and it is `""` from `bh.x0.<clinit>` until `setChannel` (`O0`) is given one,
//! which this host never does. The Java side also keeps a flag cache of its own
//! (`FlagCacheUtils`: zstd-compressed, under the app's files and cache directories, with an
//! expiry); this layer keeps the body verbatim in a file of its own format instead
//! ([`ClientSettings::load`]), and uses it only when a fetch fails.
//!
//! **Why it matters, MEASURED.** Handed `{"applicationSettings":{}}` instead, every fast flag
//! keeps its compiled default through the engine's whole initialisation, which a device never
//! does. One consequence found: `FIntPerformanceControlCrashMetricAlgorithmType2` is 0 when
//! `InferredCrash::initialize` (`0x228fc00`) runs, so it never sets its member at `+0xc8`; the
//! flag's live value is `4` by the time the inferred-crash report runs, and the report
//! dereferences the null member (`MemoryFault` at `0x23a03ec`) and a worker dies.
//!
//! The network request itself is the embedding's, as it is the Java side's on a device: this
//! module takes it as a function and never reaches the network or runs a process.

use std::path::{Path, PathBuf};

use super::pool::MAX_PINNED_BYTES;
use super::script::{CHANNEL_PLATFORM_NAME, CLIENT_SETTINGS};

/// Where the Java side fetches the document, without a channel's `/bucket/<name>`: see the module
/// documentation.
#[must_use]
pub fn settings_url() -> String {
    format!("https://clientsettingscdn.roblox.com/v2/settings/application/{CHANNEL_PLATFORM_NAME}")
}

/// Where the document [`ClientSettings::load`] chose came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The fetch answered a usable document. `cached` is whether it was kept for a later run.
    Fetched {
        /// `Err` names why the cache could not be written.
        cached: Result<(), String>,
    },
    /// The fetch failed and the copy an earlier run kept was usable.
    Cached {
        /// Why the fetch failed.
        fetch_failed: String,
        /// How long ago the copy was written, in whole minutes, when the file says.
        age_minutes: Option<u64>,
    },
    /// Neither: the engine is handed [`CLIENT_SETTINGS`], an empty document, and every flag keeps
    /// its compiled default -- **not** what a device does (it makes no downcall at all then).
    Empty {
        /// Why the fetch failed.
        fetch_failed: String,
        /// Why the cache was not used.
        cache_unusable: String,
    },
}

/// The document to hand `nativeInitClientSettings`, and where it came from.
#[derive(Debug, Clone)]
pub struct ClientSettings {
    /// The document, verbatim.
    pub document: String,
    /// How many flags its `applicationSettings` holds.
    pub flags: usize,
    /// Where it came from.
    pub source: Source,
    /// The URL that was fetched.
    pub url: String,
    /// The cache file consulted.
    pub cache: PathBuf,
}

impl ClientSettings {
    /// **Choose the document as the Java side would have it**: what `fetch(url)` answers when it
    /// is usable ([`flag_count`]), written to `cache` for a later run; else the copy in `cache`
    /// when that is usable; else [`CLIENT_SETTINGS`], and [`Source::Empty`] says so.
    ///
    /// `fetch` is the embedding's HTTPS GET of `url`, answering the body or why there is none.
    pub fn load(url: &str, cache: &Path, fetch: impl FnOnce(&str) -> Result<String, String>) -> Self {
        let fetched = fetch(url).and_then(|body| match flag_count(&body) {
            Ok(flags) => Ok((body, flags)),
            Err(why) => Err(format!("its answer is unusable: {why}")),
        });
        let source;
        let (document, flags) = match fetched {
            Ok((body, flags)) => {
                source = Source::Fetched { cached: write_cache(cache, &body) };
                (body, flags)
            }
            Err(fetch_failed) => match read_cache(cache) {
                Ok((body, flags, age_minutes)) => {
                    source = Source::Cached { fetch_failed, age_minutes };
                    (body, flags)
                }
                Err(cache_unusable) => {
                    source = Source::Empty { fetch_failed, cache_unusable };
                    (CLIENT_SETTINGS.to_string(), 0)
                }
            },
        };
        ClientSettings { document, flags, source, url: url.to_string(), cache: cache.to_path_buf() }
    }
}

/// One line for the log: where the document came from, its size and its flag count -- never its
/// contents. [`Source::Empty`] says what it costs, because a run on default flags otherwise looks
/// exactly like a run on the real ones.
impl core::fmt::Display for ClientSettings {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let size = format!("{} bytes, {} flags", self.document.len(), self.flags);
        match &self.source {
            Source::Fetched { cached: Ok(()) } => {
                write!(f, "CLIENT SETTINGS: fetched {} -- {size}; kept in {}", self.url, self.cache.display())
            }
            Source::Fetched { cached: Err(why) } => {
                write!(f, "CLIENT SETTINGS: fetched {} -- {size}; NOT kept ({why})", self.url)
            }
            Source::Cached { fetch_failed, age_minutes } => {
                let age = age_minutes.map_or_else(String::new, |m| format!(", written {m} min ago"));
                write!(
                    f,
                    "CLIENT SETTINGS: the kept copy {}{age} -- {size}; the fetch of {} failed: {fetch_failed}",
                    self.cache.display(),
                    self.url
                )
            }
            Source::Empty { fetch_failed, cache_unusable } => write!(
                f,
                "CLIENT SETTINGS: NONE -- the fetch of {} failed ({fetch_failed}) and the kept copy \
                 is unusable ({cache_unusable}); the engine is handed an EMPTY document ({size}) \
                 and runs on compiled-in flag defaults, which a device never does",
                self.url
            ),
        }
    }
}

/// How many flags a settings document's `applicationSettings` object holds -- and whether it is
/// one this layer will hand the engine at all: well-formed JSON (RFC 8259), an object at the top
/// with an `applicationSettings` member that is an object, and small enough for the pinned pool
/// to hand the engine through `GetStringUTFChars` ([`MAX_PINNED_BYTES`]; the live document was
/// 1,361,440 bytes on 2026-09-24).
///
/// # Errors
///
/// Why it is not usable -- never quoting it.
pub fn flag_count(document: &str) -> Result<usize, String> {
    if document.len() >= MAX_PINNED_BYTES {
        return Err(format!(
            "{} bytes is more than the pinned pool can hand the engine ({MAX_PINNED_BYTES})",
            document.len()
        ));
    }
    let mut parser = Parser { bytes: document.as_bytes(), at: 0, depth: 0 };
    parser.blank();
    let flags = parser.top()?;
    parser.blank();
    if parser.at != parser.bytes.len() {
        return Err(format!("trailing bytes after the document, at byte {}", parser.at));
    }
    flags.ok_or_else(|| "no `applicationSettings` object".to_string())
}

/// Write `body` to `cache` through a sibling file and a rename, so a run that ends mid-write
/// leaves the previous copy rather than half of one.
fn write_cache(cache: &Path, body: &str) -> Result<(), String> {
    let fail = |error: std::io::Error| format!("{}: {error}", cache.display());
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    let partial = cache.with_extension("partial");
    std::fs::write(&partial, body).map_err(fail)?;
    std::fs::rename(&partial, cache).map_err(fail)
}

/// The copy in `cache`, its flag count and its age -- when it is usable by [`flag_count`]'s rule.
fn read_cache(cache: &Path) -> Result<(String, usize, Option<u64>), String> {
    let body = match std::fs::read_to_string(cache) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("there is none".to_string()),
        Err(error) => return Err(format!("{}: {error}", cache.display())),
    };
    let flags = flag_count(&body)?;
    let age = std::fs::metadata(cache)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|written| written.elapsed().ok())
        .map(|elapsed| elapsed.as_secs() / 60);
    Ok((body, flags, age))
}

/// Nesting deeper than this is refused rather than recursed into: the document is untrusted bytes
/// from the network or a file, and a recursive parser's stack is the host's.
const MAX_DEPTH: usize = 64;

/// A validating RFC 8259 reader that keeps nothing but the one count [`flag_count`] wants.
struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: usize,
}

impl Parser<'_> {
    fn fail<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("not JSON: {what} at byte {}", self.at))
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn blank(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            self.fail(&format!("expected `{}`", byte as char))
        }
    }

    /// The top-level value, which must be an object: the member count of its
    /// `applicationSettings`, when that is present and an object.
    fn top(&mut self) -> Result<Option<usize>, String> {
        if self.peek() != Some(b'{') {
            return self.fail("the document is not an object");
        }
        let mut flags = None;
        self.object(|parser, key| {
            if key == "applicationSettings" {
                if parser.peek() != Some(b'{') {
                    return parser.fail("`applicationSettings` is not an object");
                }
                flags = Some(parser.object(|parser, _| parser.value())?);
                Ok(())
            } else {
                parser.value()
            }
        })?;
        Ok(flags)
    }

    /// An object, with `member` reading each value after its key; answers the member count.
    fn object(&mut self, mut member: impl FnMut(&mut Self, &str) -> Result<(), String>) -> Result<usize, String> {
        self.enter()?;
        self.expect(b'{')?;
        self.blank();
        let mut count = 0;
        if self.peek() == Some(b'}') {
            self.at += 1;
            self.depth -= 1;
            return Ok(0);
        }
        loop {
            self.blank();
            let key = self.string()?;
            self.blank();
            self.expect(b':')?;
            self.blank();
            member(self, &key)?;
            count += 1;
            self.blank();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    self.depth -= 1;
                    return Ok(count);
                }
                _ => return self.fail("expected `,` or `}`"),
            }
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.fail(&format!("nested more than {MAX_DEPTH} deep"));
        }
        Ok(())
    }

    fn value(&mut self) -> Result<(), String> {
        match self.peek() {
            Some(b'{') => self.object(|parser, _| parser.value()).map(drop),
            Some(b'[') => {
                self.enter()?;
                self.at += 1;
                self.blank();
                if self.peek() == Some(b']') {
                    self.at += 1;
                } else {
                    loop {
                        self.blank();
                        self.value()?;
                        self.blank();
                        match self.peek() {
                            Some(b',') => self.at += 1,
                            Some(b']') => {
                                self.at += 1;
                                break;
                            }
                            _ => return self.fail("expected `,` or `]`"),
                        }
                    }
                }
                self.depth -= 1;
                Ok(())
            }
            Some(b'"') => self.string().map(drop),
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => self.fail("expected a value"),
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(())
        } else {
            self.fail("expected `true`, `false` or `null`")
        }
    }

    fn digits(&mut self) -> usize {
        let from = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - from
    }

    fn number(&mut self) -> Result<(), String> {
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return self.fail("expected a digit"),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if self.digits() == 0 {
                return self.fail("expected a digit after `.`");
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return self.fail("expected an exponent");
            }
        }
        Ok(())
    }

    /// A string, decoded far enough to compare keys: escapes are checked, and a `\u` escape is
    /// kept as written (no key this reader compares contains one).
    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let from = self.at;
        loop {
            match self.peek() {
                None => return self.fail("an unterminated string"),
                Some(b'"') => break,
                Some(b'\\') => {
                    self.at += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.at += 1,
                        Some(b'u') => {
                            self.at += 1;
                            let hex = self.bytes.get(self.at..self.at + 4);
                            if !hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) {
                                return self.fail("a `\\u` escape without four hex digits");
                            }
                            self.at += 4;
                        }
                        _ => return self.fail("an unknown escape"),
                    }
                }
                Some(0x00..=0x1f) => return self.fail("a control character in a string"),
                Some(_) => self.at += 1,
            }
        }
        let text = String::from_utf8_lossy(&self.bytes[from..self.at]).into_owned();
        self.at += 1;
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own, removed first.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omni-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    const DOCUMENT: &str =
        r#"{"applicationSettings":{"FFlagA":"True","FIntB":"4","DFStringC":"x\"yé","FLogD":"12"}}"#;

    #[test]
    fn settings_flags_are_the_members_of_application_settings() {
        assert_eq!(flag_count(DOCUMENT), Ok(4));
        assert_eq!(flag_count(CLIENT_SETTINGS), Ok(0), "the empty document is usable and has none");
        // Other members, nested values and whitespace are read past, not counted.
        let wider = " {\"other\": [1, -2.5e3, {\"k\": null}, true],\n \"applicationSettings\": {\"A\": \"1\", \"B\": {\"c\": [false]}}} ";
        assert_eq!(flag_count(wider), Ok(2));
        // A key that merely contains the name is not it.
        assert!(flag_count(r#"{"xapplicationSettings":{"A":"1"}}"#).is_err());
    }

    #[test]
    fn settings_a_document_that_is_not_the_settings_shape_is_refused() {
        for bad in [
            "",
            "null",
            r#"{"errors":[{"code":1,"message":"The application name is invalid."}]}"#,
            r#"{"applicationSettings":[]}"#,
            r#"{"applicationSettings":"x"}"#,
            r#"{"applicationSettings":{"A":"1"}"#,
            r#"{"applicationSettings":{"A":"1",}}"#,
            r#"{"applicationSettings":{"A":01}}"#,
            r#"{"applicationSettings":{"A":"\q"}}"#,
            r#"{"applicationSettings":{"A":tru}}"#,
            r#"{"applicationSettings":{}} x"#,
            "<html><body>502 Bad Gateway</body></html>",
        ] {
            assert!(flag_count(bad).is_err(), "{bad:?} was accepted");
        }
        let deep = format!("{{\"applicationSettings\":{{\"A\":{}{}}}}}", "[".repeat(100), "]".repeat(100));
        assert!(flag_count(&deep).unwrap_err().contains("nested"), "a deep document is refused, not recursed into");
        let huge = format!("{{\"applicationSettings\":{{\"A\":\"{}\"}}}}", "x".repeat(MAX_PINNED_BYTES));
        assert!(flag_count(&huge).unwrap_err().contains("pinned pool"));
    }

    #[test]
    fn settings_a_fetched_document_is_used_and_kept() {
        let dir = scratch("fetched");
        let cache = dir.join("files").join("omnidroid-clientsettings.json");
        let mut asked = None;
        let chosen = ClientSettings::load("https://example.invalid/x", &cache, |url| {
            asked = Some(url.to_string());
            Ok(DOCUMENT.to_string())
        });
        assert_eq!(asked.as_deref(), Some("https://example.invalid/x"));
        assert_eq!((chosen.document.as_str(), chosen.flags), (DOCUMENT, 4));
        assert_eq!(chosen.source, Source::Fetched { cached: Ok(()) });
        assert_eq!(std::fs::read_to_string(&cache).expect("kept"), DOCUMENT);
        let line = chosen.to_string();
        assert!(line.contains("fetched https://example.invalid/x") && line.contains("4 flags"), "{line}");
        assert!(!line.contains("FFlagA"), "the line never carries the document: {line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_a_failed_fetch_falls_back_to_the_kept_copy() {
        let dir = scratch("cached");
        let cache = dir.join("omnidroid-clientsettings.json");
        std::fs::create_dir_all(&dir).expect("a directory");
        std::fs::write(&cache, DOCUMENT).expect("a kept copy");
        let chosen = ClientSettings::load("https://example.invalid/x", &cache, |_| Err("curl: (6) no host".to_string()));
        assert_eq!((chosen.document.as_str(), chosen.flags), (DOCUMENT, 4));
        assert!(
            matches!(&chosen.source, Source::Cached { fetch_failed, .. } if fetch_failed == "curl: (6) no host"),
            "{:?}",
            chosen.source
        );
        // A fetch that answers something unusable is a failed fetch, and does not replace the copy.
        let chosen = ClientSettings::load("https://example.invalid/x", &cache, |_| Ok("<html>".to_string()));
        assert!(matches!(chosen.source, Source::Cached { .. }), "{:?}", chosen.source);
        assert_eq!(std::fs::read_to_string(&cache).expect("still kept"), DOCUMENT);
        assert!(chosen.to_string().contains("the kept copy"), "{chosen}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_with_neither_the_engine_gets_the_empty_document_and_the_line_says_so() {
        let dir = scratch("empty");
        let cache = dir.join("omnidroid-clientsettings.json");
        let chosen = ClientSettings::load("https://example.invalid/x", &cache, |_| Err("offline".to_string()));
        assert_eq!((chosen.document.as_str(), chosen.flags), (CLIENT_SETTINGS, 0));
        assert!(matches!(&chosen.source, Source::Empty { cache_unusable, .. } if cache_unusable == "there is none"));
        // A kept copy that is not a settings document is not used either.
        std::fs::create_dir_all(&dir).expect("a directory");
        std::fs::write(&cache, "{\"applicationSettings\":").expect("a torn copy");
        let chosen = ClientSettings::load("https://example.invalid/x", &cache, |_| Err("offline".to_string()));
        assert_eq!(chosen.document, CLIENT_SETTINGS);
        let line = chosen.to_string();
        assert!(line.contains("NONE") && line.contains("EMPTY") && line.contains("offline"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_url_is_the_java_sides_for_this_apk() {
        assert_eq!(settings_url(), "https://clientsettingscdn.roblox.com/v2/settings/application/GoogleAndroidApp");
    }
}
