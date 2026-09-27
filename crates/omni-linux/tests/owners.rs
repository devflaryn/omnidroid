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
