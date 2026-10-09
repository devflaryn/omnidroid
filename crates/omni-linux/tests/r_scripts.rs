//! `r_roblox`'s time-to-game switches (`common::r_scripts`), checked without booting anything: the
//! defaults are the fast scripts (the old ones one variable away), and each switch's script says
//! what the old one said, so the host
//! side (which waits for those lines) and the run's reader see the same milestones.
mod common;

use common::r_scripts;

#[test]
fn the_defaults_are_the_fast_scripts_and_the_old_ones_stay_reachable() {
    // One test sets and reads the variables, so parallel tests cannot race on them.
    std::env::remove_var("OMNI_R_LINK_DELAY");
    std::env::remove_var("OMNI_R_PLANT_FIRST");
    std::env::remove_var("OMNI_R_FAST_SETUP");
    assert_eq!(r_scripts::link_delay(), 3);
    assert!(r_scripts::plant_first());
    assert!(!r_scripts::fast_setup(), "opt-in");
    std::env::set_var("OMNI_R_LINK_DELAY", "45");
    std::env::set_var("OMNI_R_PLANT_FIRST", "0");
    std::env::set_var("OMNI_R_FAST_SETUP", "1");
    assert_eq!(r_scripts::link_delay(), 45);
    assert!(!r_scripts::plant_first());
    assert!(r_scripts::fast_setup());
    std::env::set_var("OMNI_R_LINK_DELAY", "soon");
    assert_eq!(r_scripts::link_delay(), 3, "an unreadable value is the default");
    std::env::remove_var("OMNI_R_LINK_DELAY");
    std::env::remove_var("OMNI_R_PLANT_FIRST");
    std::env::remove_var("OMNI_R_FAST_SETUP");
}

#[test]
fn the_idle_apps_are_disabled_in_the_background_and_waited_for() {
    let apps = ["com.a", "com.b"];
    let fg = r_scripts::lean(&apps, false);
    assert!(fg.starts_with("for a in com.a com.b; do"), "{fg}");
    assert!(fg.contains("echo \"[r] idle apps disabled: 2\""), "the line the run's reader looks for: {fg}");
    assert_eq!(r_scripts::lean_wait(false), "");
    let bg = r_scripts::lean(&apps, true);
    assert_eq!(bg, format!("( {fg}) & lean_pid=$!; "), "the same loop, in a subshell");
    assert!(r_scripts::lean_wait(true).contains("wait $lean_pid"));
}

#[test]
fn the_screen_is_kept_on_by_the_setting_svc_writes() {
    assert_eq!(r_scripts::stay_on(false), "svc power stayon true; ");
    assert_eq!(r_scripts::stay_on(true), "settings put global stay_on_while_plugged_in 7; ");
}

#[test]
fn the_planted_store_is_the_app_s_and_says_so() {
    let p = r_scripts::plant("/data/local/tmp/cookies.db");
    for part in [
        "a=/data/data/$pkg;",
        "uid=$(stat -c %u $a)",
        "cp /data/local/tmp/cookies.db $d/Cookies",
        "chown -R $uid:$uid $a/app_webview",
        "chmod 600 $d/Cookies",
        "echo \"[r] cookie planted: ok\"",
        "echo \"[r] cookie planted: failed\"",
        "rm -f /data/local/tmp/cookies.db",
    ] {
        assert!(p.contains(part), "{part:?} in {p}");
    }
    // The store holds the session's cookie: removed whether the plant worked or not.
    assert!(p.trim_end().ends_with("rm -f /data/local/tmp/cookies.db;"), "{p}");
}

/// The scripts are `sh`: checked by a host `sh -n` where there is one (Linux, macOS, Git Bash).
#[test]
fn the_scripts_parse() {
    let Ok(probe) = std::process::Command::new("sh").arg("-c").arg("true").status() else {
        eprintln!("SKIPPED: no sh on this host");
        return;
    };
    if !probe.success() {
        return;
    }
    let pkg = "pkg=com.roblox.client; ";
    for script in [
        format!("{pkg}{}", r_scripts::plant("/data/local/tmp/cookies.db")),
        format!("{}{}", r_scripts::lean(&["com.a"], true), r_scripts::lean_wait(true)),
        r_scripts::lean(&["com.a"], false),
        r_scripts::stay_on(true).to_string(),
    ] {
        let ok = std::process::Command::new("sh").arg("-n").arg("-c").arg(&script).status().expect("sh").success();
        assert!(ok, "sh -n refuses: {script}");
    }
}
