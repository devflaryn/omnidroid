//! **The app's cookie store: what Android's `CookieManager` keeps for the Roblox app.**
//!
//! DECODED from `classes2.dex` (2026-09-24), and the reason a sign-in did not survive a restart:
//!
//! * **The engine pushes its cookies to Java.** Constructing `CookieProtocol` (`NativeHelper.Q`
//!   @`0x0046` -> `jk.k0.w` -> `CookieProtocol.<init>` -> `tm.b.a`) hands a
//!   `CookieProtocol$OnSetCookieHandlerImpl` to the exported native
//!   `JNICookieProtocol.updateOnSetCookieHandler` (link `0x230a9f4`, which keeps only that handler
//!   and ignores `thiz`). The engine then calls `onSetCookie(String[] cookies, String url)` on it
//!   whenever its cookies change; the Java body posts each one to the main thread, which calls
//!   `fl.j.g` -> `android.webkit.CookieManager.setCookie(url, cookie, callback)` and `flush()`.
//!   The engine can also call the static `CookieProtocol.setCookie(url, cookie)` -> `fl.j.f` ->
//!   `CookieManager.setCookie(url, cookie)`.
//! * **Java hands them back at the next start.** `bh.x0.W0` (the settings bootstrap), right after
//!   `nativeSetUserId`, calls `bh.x0.S0`: `nativeSetMultipleCookies(g(), fl.j.b(g()) ?: "")` with
//!   `g()` = `"https://" + host` -- the same URL `nativeSetBaseUrl` gets -- and `fl.j.b` is
//!   `CookieManager.getCookie(url)` (or the app's native cookie store, when a flag says so; the
//!   two hold the same cookies).
//!
//! Before this module the handler was never registered, `onSetCookie` was a sink, and the startup
//! call was not made: the engine kept its session cookie in memory only, and every relaunch came
//! up logged out (`cachedUserId` known, every authenticated call `401`, `DID_LOG_OUT`).
//!
//! What is implemented is the part of RFC 6265 `CookieManager` applies for these calls: storage
//! (§5.3) with `Domain`, `Path`, `Expires`, `Max-Age`, `Secure` and `HttpOnly`, deletion by expiry,
//! and retrieval (§5.4) as the `"name=value; name=value"` string `getCookie` returns. Like Android's
//! WebView store it **persists** -- one file in the app's own data directory, written on every
//! change, so a process that ends any way at all keeps what it had been given -- and it keeps
//! session cookies (no expiry) until they are removed, as WebView's does.
//!
//! **No cookie value is ever logged or printed.** Errors name the cookie by name only.

use std::path::{Path, PathBuf};

/// The file's first line: the format, so a future change is a refusal rather than a misread.
const HEADER: &str = "omnidroid-cookies v1";

/// One stored cookie (RFC 6265 §5.3's record, less what `getCookie` never consults).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cookie {
    name: String,
    value: String,
    /// Lower case, no leading dot.
    domain: String,
    /// No `Domain` attribute: only the exact host that set it gets it back.
    host_only: bool,
    path: String,
    /// Milliseconds since the Unix epoch; `None` for a session cookie.
    expires_ms: Option<i64>,
    secure: bool,
    http_only: bool,
}

/// The store. See the module documentation.
#[derive(Debug, Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
    file: Option<PathBuf>,
}

/// Why a `Set-Cookie` string was not stored, by cookie **name** only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieRefused(pub String);

impl core::fmt::Display for CookieRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

struct Url<'a> {
    secure: bool,
    host: String,
    path: &'a str,
}

fn parse_url(url: &str) -> Option<Url<'_>> {
    let (scheme, rest) = url.split_once("://")?;
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return None,
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority.rsplit('@').next()?.split(':').next()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let tail = &rest[end..];
    let path = tail.split(['?', '#']).next().unwrap_or("");
    Some(Url { secure, host, path })
}

/// RFC 6265 §5.1.4: the directory of the request path.
fn default_path(path: &str) -> String {
    if !path.starts_with('/') {
        return "/".to_string();
    }
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(at) => path[..at].to_string(),
    }
}

/// RFC 6265 §5.1.3.
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain || (host.ends_with(domain) && host[..host.len() - domain.len()].ends_with('.'))
}

/// RFC 6265 §5.1.4.
fn path_matches(request: &str, cookie: &str) -> bool {
    let request = if request.is_empty() { "/" } else { request };
    request == cookie
        || (request.starts_with(cookie) && (cookie.ends_with('/') || request[cookie.len()..].starts_with('/')))
}

/// RFC 6265 §5.1.1's date algorithm, for the formats servers send (`Wed, 10 May 2000 23:59:59 GMT`,
/// `Wednesday, 10-May-00 23:59:59 GMT`, `Wed May 10 23:59:59 2000`): milliseconds since the epoch.
fn parse_cookie_date(text: &str) -> Option<i64> {
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);
    for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == ':')).filter(|t| !t.is_empty()) {
        if time.is_none() {
            let parts: Vec<&str> = token.split(':').collect();
            if parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 2 && p.bytes().all(|b| b.is_ascii_digit())) {
                let n: Vec<i64> = parts.iter().map(|p| p.parse().unwrap_or(99)).collect();
                time = Some((n[0], n[1], n[2]));
                continue;
            }
        }
        let digits = token.bytes().take_while(u8::is_ascii_digit).count();
        if day.is_none() && (1..=2).contains(&digits) && digits == token.len() {
            day = token.parse::<i64>().ok();
            continue;
        }
        if month.is_none() && token.len() >= 3 {
            let months = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
            if let Some(index) = months.iter().position(|m| token[..3].eq_ignore_ascii_case(m)) {
                month = Some(index as i64 + 1);
                continue;
            }
        }
        if year.is_none() && (2..=4).contains(&digits) && digits == token.len() {
            year = token.parse::<i64>().ok();
        }
    }
    let (hour, minute, second) = time?;
    let (day, month, mut year) = (day?, month?, year?);
    if (70..=99).contains(&year) {
        year += 1900;
    } else if (0..=69).contains(&year) {
        year += 2000;
    }
    if !(1..=31).contains(&day) || year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    // Days from civil (proleptic Gregorian), Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400) + hour * 3600 + minute * 60 + second) * 1000)
}

impl CookieJar {
    /// An empty store that lives only in memory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The store kept in `file`: what is there is loaded, and every change is written back.
    /// A missing file is an empty store.
    ///
    /// # Errors
    ///
    /// A file that exists and cannot be read, or is not this format.
    pub fn with_file(file: &Path) -> Result<Self, String> {
        let mut jar = Self { cookies: Vec::new(), file: Some(file.to_path_buf()) };
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(jar),
            Err(error) => return Err(format!("the cookie store {} could not be read: {error}", file.display())),
        };
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(format!("the cookie store {} is not `{HEADER}`", file.display()));
        }
        for (number, line) in lines.enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            let [name, value, domain, host_only, path, expires, secure, http_only] = f[..] else {
                return Err(format!("the cookie store {}: record {} is malformed", file.display(), number + 1));
            };
            jar.cookies.push(Cookie {
                name: name.to_string(),
                value: value.to_string(),
                domain: domain.to_string(),
                host_only: host_only == "1",
                path: path.to_string(),
                expires_ms: if expires == "-" { None } else { expires.parse().ok() },
                secure: secure == "1",
                http_only: http_only == "1",
            });
        }
        Ok(jar)
    }

    /// How many cookies are held -- a count, never their contents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// `CookieManager.setCookie(url, value)`: store one `Set-Cookie` string received for `url`, at
    /// `now_ms`. An expired one deletes the cookie it names (how the Java side's own
    /// `removeSecurityCookie` signs out).
    ///
    /// # Errors
    ///
    /// A URL that is not http(s), a string with no `name=`, a `Domain` the URL's host does not
    /// match (RFC 6265 §5.3 step 6 ignores such a cookie; here it is said), or a store file that
    /// could not be written.
    pub fn set(&mut self, url: &str, header: &str, now_ms: i64) -> Result<(), CookieRefused> {
        let refuse = |why: String| Err(CookieRefused(why));
        let Some(url) = parse_url(url) else {
            return refuse("a cookie for a URL that is not http(s)".to_string());
        };
        let mut parts = header.split(';');
        let pair = parts.next().unwrap_or("");
        let Some((name, value)) = pair.split_once('=') else {
            return refuse("a Set-Cookie string with no `name=value`".to_string());
        };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() || name.contains(['\t', '\n', '\r']) || value.contains(['\t', '\n', '\r']) {
            return refuse(format!("a cookie named `{}` this store cannot hold", name.escape_default()));
        }
        let (mut domain, mut path, mut expires_ms, mut max_age) = (None, None, None, None);
        let (mut secure, mut http_only) = (false, false);
        for attribute in parts {
            let (key, val) = attribute.split_once('=').unwrap_or((attribute, ""));
            let (key, val) = (key.trim().to_ascii_lowercase(), val.trim());
            match key.as_str() {
                "expires" => expires_ms = parse_cookie_date(val).or(expires_ms),
                "max-age" => {
                    if let Ok(seconds) = val.parse::<i64>() {
                        max_age = Some(seconds);
                    }
                }
                "domain" if !val.is_empty() => {
                    domain = Some(val.trim_start_matches('.').to_ascii_lowercase());
                }
                "path" => path = Some(val.to_string()),
                "secure" => secure = true,
                "httponly" => http_only = true,
                _ => {}
            }
        }
        // §5.3 step 3: Max-Age wins over Expires.
        let expires_ms = match max_age {
            Some(seconds) if seconds <= 0 => Some(i64::MIN),
            Some(seconds) => Some(now_ms.saturating_add(seconds.saturating_mul(1000))),
            None => expires_ms,
        };
        let (domain, host_only) = match domain {
            Some(domain) if domain_matches(&url.host, &domain) => (domain, false),
            Some(_) => {
                return refuse(format!("the cookie `{name}` names a domain its URL's host does not match"));
            }
            None => (url.host.clone(), true),
        };
        let path = match path {
            Some(path) if path.starts_with('/') => path,
            _ => default_path(url.path),
        };
        self.cookies.retain(|c| !(c.name == name && c.domain == domain && c.path == path));
        if expires_ms.is_none_or(|at| at > now_ms) {
            self.cookies.push(Cookie {
                name: name.to_string(),
                value: value.to_string(),
                domain,
                host_only,
                path,
                expires_ms,
                secure,
                http_only,
            });
        }
        self.save().map_err(CookieRefused)
    }

    /// `CookieManager.getCookie(url)` at `now_ms`: every live cookie the URL gets, longest path
    /// first, as `"name=value; name=value"` -- the empty string when there is none (the Java caller
    /// maps `null` to `""` too).
    #[must_use]
    pub fn get(&self, url: &str, now_ms: i64) -> String {
        let Some(url) = parse_url(url) else {
            return String::new();
        };
        let mut matching: Vec<(usize, &Cookie)> = self
            .cookies
            .iter()
            .enumerate()
            .filter(|(_, c)| c.expires_ms.is_none_or(|at| at > now_ms))
            .filter(|(_, c)| if c.host_only { url.host == c.domain } else { domain_matches(&url.host, &c.domain) })
            .filter(|(_, c)| path_matches(url.path, &c.path))
            .filter(|(_, c)| !c.secure || url.secure)
            .collect();
        matching.sort_by(|(a_at, a), (b_at, b)| b.path.len().cmp(&a.path.len()).then(a_at.cmp(b_at)));
        matching.iter().map(|(_, c)| format!("{}={}", c.name, c.value)).collect::<Vec<_>>().join("; ")
    }

    /// Write the whole store, replacing the file only once the new one is complete.
    fn save(&self) -> Result<(), String> {
        let Some(file) = &self.file else { return Ok(()) };
        let mut text = String::from(HEADER);
        text.push('\n');
        for c in &self.cookies {
            let flag = |b: bool| if b { "1" } else { "0" };
            let expires = c.expires_ms.map_or("-".to_string(), |at| at.to_string());
            text.push_str(&[
                c.name.as_str(),
                c.value.as_str(),
                c.domain.as_str(),
                flag(c.host_only),
                c.path.as_str(),
                expires.as_str(),
                flag(c.secure),
                flag(c.http_only),
            ].join("\t"));
            text.push('\n');
        }
        let fail = |error: std::io::Error| format!("the cookie store {} could not be written: {error}", file.display());
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(fail)?;
        }
        let partial = file.with_extension("partial");
        std::fs::write(&partial, text).map_err(fail)?;
        std::fs::rename(&partial, file).map_err(fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://www.roblox.com";
    /// 2026-09-24T00:00:00Z.
    const NOW: i64 = 1_790_208_000_000;

    #[test]
    fn a_domain_cookie_set_for_the_site_comes_back_for_the_site_and_its_subdomains() {
        let mut jar = CookieJar::new();
        jar.set(URL, "SESSION=abc; domain=.roblox.com; path=/; expires=Fri, 01 Jan 2100 00:00:00 GMT; HttpOnly; secure", NOW)
            .expect("stored");
        jar.set(URL, "Other=1", NOW).expect("stored");
        assert_eq!(jar.get(URL, NOW), "SESSION=abc; Other=1");
        assert_eq!(jar.get("https://apis.roblox.com/v1/x", NOW), "SESSION=abc", "domain cookie, not the host-only one");
        assert_eq!(jar.get("http://www.roblox.com", NOW), "Other=1", "a secure cookie is not sent over http");
        assert_eq!(jar.get("https://notroblox.com", NOW), "", "domain match is on a label boundary");
    }

    #[test]
    fn an_expired_cookie_deletes_the_one_it_names_and_max_age_wins() {
        let mut jar = CookieJar::new();
        jar.set(URL, "A=1; domain=.roblox.com; path=/", NOW).expect("stored");
        // `fl.j.e`, the Java side's own sign-out string.
        jar.set(URL, "A=;expires=Wed, 10 May 2000 23:59:59 GMT;path=/;domain=.roblox.com", NOW).expect("deleted");
        assert_eq!(jar.get(URL, NOW), "");
        // Removed, not merely unanswered: a dead record kept would be written to disk with the
        // session cookie's name on it, which is what signing out is meant to end.
        assert_eq!(jar.len(), 0, "the expired cookie deleted the one it names and was not stored");
        jar.set(URL, "B=2; max-age=60; expires=Wed, 10 May 2000 23:59:59 GMT", NOW).expect("stored");
        assert_eq!(jar.get(URL, NOW + 59_000), "B=2", "Max-Age overrides Expires");
        assert_eq!(jar.get(URL, NOW + 61_000), "", "and it expires");
    }

    #[test]
    fn a_domain_the_url_does_not_match_is_refused_and_paths_and_dates_follow_rfc_6265() {
        let mut jar = CookieJar::new();
        assert!(jar.set(URL, "X=1; domain=evil.com", NOW).is_err());
        jar.set("https://www.roblox.com/a/b", "P=1", NOW).expect("default path /a");
        assert_eq!(jar.get("https://www.roblox.com/a/c", NOW), "P=1");
        assert_eq!(jar.get("https://www.roblox.com/ab", NOW), "", "/a does not match /ab");
        assert_eq!(parse_cookie_date("Wed, 10 May 2000 23:59:59 GMT"), Some(958_003_199_000));
        assert_eq!(parse_cookie_date("Wednesday, 10-May-00 23:59:59 GMT"), Some(958_003_199_000));
        assert_eq!(parse_cookie_date("Wed May 10 23:59:59 2000"), Some(958_003_199_000));
    }

    #[test]
    fn the_store_outlives_the_process_that_filled_it() {
        let dir = std::env::temp_dir().join(format!("omni-cookies-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("app_webview").join("omnidroid-cookies");
        {
            let mut jar = CookieJar::with_file(&file).expect("an empty store");
            jar.set(URL, "SESSION=abc; domain=.roblox.com; path=/; secure", NOW).expect("stored");
            jar.set(URL, "Gone=1; max-age=1", NOW).expect("stored");
        }
        let jar = CookieJar::with_file(&file).expect("the store, read back");
        assert_eq!(jar.len(), 2);
        assert_eq!(jar.get(URL, NOW + 5_000), "SESSION=abc", "the session cookie survives, the expired one does not");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
