//! Ownership on the writable mounts, as ext4 keeps it: a file made by a process is its user's,
//! with the mode it asked for less its umask; `chmod` changes the mode; only root gives a file
//! away. installd checks an app directory's owner and mode before it uses it.
mod common;

use omni_linux::ExitStatus;

const OK: ExitStatus = ExitStatus::Exited(0);

#[test]
fn a_new_file_is_its_creators_with_its_mode_less_the_umask() {
    let Some(runs) = common::run_each(&[
        &["/system/bin/mkdir", "-m", "751", "/data/local/tmp/d"],
        &["/system/bin/touch", "/data/local/tmp/d/f"],
        &["/system/bin/stat", "-c", "%u %g %a", "/data/local/tmp/d", "/data/local/tmp/d/f"],
        &["/system/bin/chmod", "600", "/data/local/tmp/d/f"],
        &["/system/bin/stat", "-c", "%a", "/data/local/tmp/d/f"],
        &["/system/bin/chown", "0:0", "/data/local/tmp/d/f"],
    ]) else {
        return;
    };
    for (i, (status, out, err)) in runs[..5].iter().enumerate() {
        assert_eq!(*status, OK, "step {i}: {out}\n{err}");
    }
    // An app (uid 10000), umask 022: the directory 0751 as asked, the file 0666 less 022.
    assert_eq!(runs[2].1, "10000 10000 751\n10000 10000 644\n", "{:?}", runs[2]);
    assert_eq!(runs[4].1.trim(), "600");
    assert_ne!(runs[5].0, OK, "an app gives no file away: {:?}", runs[5]);
}

/// `access` answers by the permission bits the caller's ids select -- an owner by the owner's bits
/// alone, whatever the others' allow (ART refuses an app's dex file the app could write).
#[test]
fn access_answers_by_the_owners_bits() {
    let Some((status, out, err)) = common::run(&[
        "/system/bin/sh",
        "-c",
        "f=/data/local/tmp/f; echo x > $f; chmod 444 $f; test -w $f || echo not-writable; chmod 066 $f; test -r $f || echo not-readable; chmod 644 $f; test -r $f && test -w $f && echo owner-rw; test -x /system/bin/sh && echo executable",
    ]) else {
        return;
    };
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "{out}\n{err}");
    assert_eq!(out, "not-writable\nnot-readable\nowner-rw\nexecutable\n", "{err}");
}
