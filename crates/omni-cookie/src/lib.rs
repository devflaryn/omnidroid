//! An account's Roblox session cookie put into an Android app's own WebView cookie store -- the
//! Chromium `Cookies` database that `android.webkit.CookieManager` keeps at
//! `/data/data/<package>/app_webview/Default/Cookies` -- as a device that had signed in holds it.
//!
//! `tools/plant_cookie.py`, ported so an install (`omni_linux::install`: the bundle the desktop app
//! ships, with no Python) can plant one: the same schema, the same row, the same refusal and the
//! same line said, run the same way (`omni-plant-cookie [--new] <Cookies database> <cookie file>`).
//! The two are kept in step: a change to one is a change to both.
//!
//! The cookie file holds the `.ROBLOSECURITY` value: bare, as `.ROBLOSECURITY=<value>`, or as a
//! Netscape `cookies.txt`. **The value is never printed**, nor put in an error: it is an account's
//! whole session. The app must not be running (`am force-stop`): the database is changed in place,
//! without a journal file, so the files the app owns stay the app's. `--new` makes the database
//! first, as the device's WebView makes it (its schema, version 21) -- a store put into an app's data
//! before its first start, which then starts signed in.
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// The cookie's name.
pub const NAME: &str = ".ROBLOSECURITY";

/// The device WebView's cookie store (Chromium's `net/extras/sqlite`, version 21), as it writes it.
const SCHEMA: &[&str] = &[
    "CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR)",
    "CREATE TABLE cookies(creation_utc INTEGER NOT NULL,host_key TEXT NOT NULL,top_frame_site_key TEXT NOT NULL,\
     name TEXT NOT NULL,value TEXT NOT NULL,encrypted_value BLOB NOT NULL,path TEXT NOT NULL,expires_utc INTEGER NOT NULL,\
     is_secure INTEGER NOT NULL,is_httponly INTEGER NOT NULL,last_access_utc INTEGER NOT NULL,has_expires INTEGER NOT NULL,\
     is_persistent INTEGER NOT NULL,priority INTEGER NOT NULL,samesite INTEGER NOT NULL,source_scheme INTEGER NOT NULL,\
     source_port INTEGER NOT NULL,last_update_utc INTEGER NOT NULL)",
    "CREATE UNIQUE INDEX cookies_unique_index ON cookies(host_key, top_frame_site_key, name, path, source_scheme, source_port)",
];

/// A year in Chromium's microseconds: how long the planted cookie lasts.
const YEAR_US: i64 = 365 * 24 * 3600 * 1_000_000;

/// The `.ROBLOSECURITY` value in a cookie file's `text`: a `cookies.txt` row naming it, else a
/// `.ROBLOSECURITY=<value>` line (up to a `;`), else the first line as it is. Comment lines are
/// skipped, but not `#HttpOnly_` rows (a `cookies.txt` marks an HttpOnly cookie's row so).
#[must_use]
pub fn value_of(text: &str) -> String {
    let text = text.trim();
    let lines = || text.split(['\n', '\r']).filter(|l| !l.is_empty());
    for line in lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') && !line.starts_with("#HttpOnly_") {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() >= 7 && fields[5] == NAME {
            return fields[6].trim().to_string();
        }
        if let Some(rest) = line.strip_prefix(NAME).and_then(|r| r.strip_prefix('=')) {
            return rest.split(';').next().unwrap_or_default().trim().to_string();
        }
    }
    lines().next().unwrap_or_default().trim().to_string()
}

/// Whether `value` is a session cookie's: every `.ROBLOSECURITY` starts with Roblox's warning.
#[must_use]
pub fn is_session(value: &str) -> bool {
    value.starts_with("_|WARNING")
}

/// Now in Chromium's `base::Time`: microseconds since 1601-01-01.
#[must_use]
pub fn chromium_now() -> i64 {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    ((unix + 11_644_473_600.0) * 1_000_000.0) as i64
}

/// `journal_mode=OFF` on `c`: no `-journal` file beside a store the app owns. (The pragma answers
/// with the mode, so it is a query.)
fn no_journal(c: &Connection) -> rusqlite::Result<()> {
    c.query_row("PRAGMA journal_mode=OFF", [], |_| Ok(()))
}

/// Make a new, empty store at `db` (a file there is replaced), as the device's WebView makes it.
///
/// # Errors
/// The file could not be removed, made or written.
pub fn create(db: &Path) -> Result<(), String> {
    if db.exists() {
        std::fs::remove_file(db).map_err(|e| format!("{}: {e}", db.display()))?;
    }
    let c = Connection::open(db).map_err(|e| format!("{}: {e}", db.display()))?;
    no_journal(&c).map_err(|e| e.to_string())?;
    for statement in SCHEMA {
        c.execute(statement, []).map_err(|e| e.to_string())?;
    }
    for (key, value) in [("mmap_status", "-1"), ("version", "21"), ("last_compatible_version", "21")] {
        c.execute("INSERT INTO meta VALUES (?1, ?2)", [key, value]).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Put `value` into the store at `db` (which must be there) as the only `.ROBLOSECURITY` cookie of
/// `roblox.com`. What to say: the store's version and how many such rows it holds -- never the value.
///
/// # Errors
/// The store is not there, or is not a WebView cookie store.
pub fn plant(db: &Path, value: &str) -> Result<String, String> {
    let now = chromium_now();
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let c = Connection::open_with_flags(db, flags).map_err(|e| format!("{}: {e}", db.display()))?;
    no_journal(&c).map_err(|e| e.to_string())?;
    let version: Option<String> = c.query_row("SELECT CAST(value AS TEXT) FROM meta WHERE key='version'", [], |r| r.get(0)).ok();
    c.execute_batch("BEGIN").map_err(|e| e.to_string())?;
    c.execute("DELETE FROM cookies WHERE name=?1 AND host_key LIKE '%roblox.com'", [NAME]).map_err(|e| e.to_string())?;
    c.execute(
        "INSERT INTO cookies (creation_utc, host_key, top_frame_site_key, name, value, encrypted_value, \
         path, expires_utc, is_secure, is_httponly, last_access_utc, has_expires, is_persistent, \
         priority, samesite, source_scheme, source_port, last_update_utc) \
         VALUES (?1, '.roblox.com', '', ?2, ?3, x'', '/', ?4, 1, 1, ?5, 1, 1, 1, -1, 2, 443, ?6)",
        rusqlite::params![now, NAME, value, now + YEAR_US, now, now],
    )
    .map_err(|e| e.to_string())?;
    c.execute_batch("COMMIT").map_err(|e| e.to_string())?;
    let n: i64 = c.query_row("SELECT count(*) FROM cookies WHERE name=?1", [NAME], |r| r.get(0)).map_err(|e| e.to_string())?;
    Ok(format!("plant_cookie: {NAME} planted for .roblox.com (store version {}, {n} row)", version.as_deref().unwrap_or("?")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALUE: &str = "_|WARNING:-DO-NOT-SHARE-THIS.--test-value";

    /// Every shape a cookie is kept in gives the same value: bare, `NAME=`, and a `cookies.txt` row
    /// (an HttpOnly one too), with comments and blank lines around it.
    #[test]
    fn a_cookie_is_read_from_every_shape_it_is_kept_in() {
        assert_eq!(value_of(&format!("  {VALUE}\n")), VALUE);
        assert_eq!(value_of(&format!(".ROBLOSECURITY={VALUE}; path=/\r\n")), VALUE);
        let txt = format!("# Netscape HTTP Cookie File\n\n#HttpOnly_.roblox.com\tTRUE\t/\tTRUE\t0\t.ROBLOSECURITY\t{VALUE}\n");
        assert_eq!(value_of(&txt), VALUE);
        assert_eq!(value_of(""), "");
        assert!(is_session(VALUE) && !is_session("not-a-cookie"));
    }

    /// A new store holds the schema and meta the WebView writes, and planting twice leaves one row,
    /// holding the value, for `.roblox.com`; a store that is not there is not made.
    #[test]
    fn a_new_store_holds_one_cookie_however_often_it_is_planted() {
        let dir = std::env::temp_dir().join(format!("omni-cookie-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("Cookies");
        assert!(plant(&db, VALUE).is_err() && !db.exists(), "plant opens a store, never makes one");
        create(&db).unwrap();
        let said = plant(&db, VALUE).unwrap();
        assert_eq!(plant(&db, VALUE).unwrap(), said);
        assert_eq!(said, "plant_cookie: .ROBLOSECURITY planted for .roblox.com (store version 21, 1 row)");
        assert!(!said.contains(VALUE));
        let c = Connection::open(&db).unwrap();
        let (host, value, port): (String, String, i64) =
            c.query_row("SELECT host_key, value, source_port FROM cookies", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((host.as_str(), value.as_str(), port), (".roblox.com", VALUE, 443));
        let meta: i64 = c.query_row("SELECT count(*) FROM meta", [], |r| r.get(0)).unwrap();
        assert_eq!(meta, 3);
        assert!(!dir.join("Cookies-journal").exists());
        drop(c);
        create(&db).unwrap();
        let rows: i64 = Connection::open(&db).unwrap().query_row("SELECT count(*) FROM cookies", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 0, "--new starts again from an empty store");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
