//! Pieces of `r_roblox`'s guest scripts that decide how long a new device takes to reach the game,
//! each behind a switch (default: as before), kept here so a test that boots nothing can check them
//! (`tests/r_scripts.rs`).
//!
//! Timeline of a new device before them (five runs, 2026-10-09, `[t]` seconds; `omni-linux-r-*.log`):
//! boot_completed 45-50, settings + idle apps disabled 61-66, `pm install` 66-72, the app's first
//! start 70-74, its main Activity 94-101, the cookie dance (stop, plant, start again) to the second
//! main Activity 124-134, DID_LOG_IN 129-139, **a fixed 45 s**, the link 176-186, "Joining game"
//! 182-195, onGameLoaded 198-216.

/// `OMNI_R_LINK_DELAY=<seconds>`: how long a new device waits after the account has signed in
/// before it sends the place's link (default 45, the old fixed wait). On the warm device the link
/// is sent at the main Activity, before the sign-in, and joins as soon
/// (`docs/MORNING-2026-10-02-warm-join.md`, "Tried, not kept"); the join script resends it up to
/// three times if the app settles on Home instead.
#[must_use]
pub fn link_delay() -> u32 {
    std::env::var("OMNI_R_LINK_DELAY").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(45)
}

/// `OMNI_R_PLANT_FIRST=1`: a new device signs in with the cookie store put into the app's data
/// **before its first start** (`tools/plant_cookie.py --new`, as `omnidroid aosp` does on the warm
/// device) -- not the old start, wait for the app's own store, stop, plant, start again, which costs
/// a whole second cold start (~30-35 s: first main Activity ~100 s, second ~134 s).
#[must_use]
pub fn plant_first() -> bool {
    std::env::var("OMNI_R_PLANT_FIRST").as_deref() == Ok("1")
}

/// `OMNI_R_FAST_SETUP=1`: a new device's setup overlaps the idle apps' disabling with the APK's
/// install (18 `cmd package disable-user`, ~7 s, then `pm install`, ~5 s, one after the other
/// before), and keeps the screen on with the setting `svc power stayon true` writes -- `svc` is a
/// Java tool, and its VM aborts on every new device here ("Failed anonymous mmap(0x0, 67108864):
/// Out of memory" in the shell's `app_process`, every run of 2026-10-09), so it never took effect.
#[must_use]
pub fn fast_setup() -> bool {
    std::env::var("OMNI_R_FAST_SETUP").as_deref() == Ok("1")
}

/// Keeping the screen on: `svc power stayon true` (a Java tool), or the setting it writes
/// (`Settings.Global.STAY_ON_WHILE_PLUGGED_IN`: AC, USB and wireless, 7) through `settings`, a
/// native command.
#[must_use]
pub fn stay_on(fast: bool) -> &'static str {
    if fast {
        "settings put global stay_on_while_plugged_in 7; "
    } else {
        "svc power stayon true; "
    }
}

/// Disabling `apps` and saying so: in the foreground, or (`fast`) in a background subshell that
/// [`lean_wait`] joins.
#[must_use]
pub fn lean(apps: &[&str], fast: bool) -> String {
    let body = format!(
        "for a in {}; do r=$(cmd package disable-user --user 0 $a 2>&1); case \"$r\" in *disabled*) ;; *) echo \"[r] not disabled: $a: $r\";; esac; done; echo \"[r] idle apps disabled: {}\"; ",
        apps.join(" "),
        apps.len()
    );
    if fast {
        format!("( {body}) & lean_pid=$!; ")
    } else {
        body
    }
}

/// Where a script with a background [`lean`] waits for it (nothing when it ran in the foreground).
#[must_use]
pub fn lean_wait(fast: bool) -> &'static str {
    if fast {
        "[ -n \"$lean_pid\" ] && wait $lean_pid; "
    } else {
        ""
    }
}

/// The guest shell that puts a cookie store made on the host (`store`, a guest path) into `$pkg`'s
/// WebView data, owned by the app, before its first start -- `omnidroid`'s `warm::plant`. Says
/// `[r] cookie planted: ok` (or `failed`), as the old dance does.
#[must_use]
pub fn plant(store: &str) -> String {
    format!(
        "a=/data/data/$pkg; uid=$(stat -c %u $a); d=$a/app_webview/Default; \
         if mkdir -p $d && cp {store} $d/Cookies && chown -R $uid:$uid $a/app_webview && chmod 700 $a/app_webview $d && chmod 600 $d/Cookies; \
         then echo \"[r] cookie planted: ok\"; else echo \"[r] cookie planted: failed\"; fi; rm -f {store}; "
    )
}
