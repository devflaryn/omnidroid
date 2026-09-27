//! Extended attributes, as the kernel keeps them: `setfattr` stores one and `getfattr` reads it
//! back, and a file's SELinux context (`security.selinux`) is what `restorecon` sets from the
//! image's file_contexts and `ls -Z` shows. apexd restorecons every APEX it decompresses into
//! /data/apex/decompressed, and gives the APEX up when it cannot.
mod common;

use omni_linux::ExitStatus;

const OK: ExitStatus = ExitStatus::Exited(0);

#[test]
fn a_user_attribute_is_stored_and_read_back() {
    let f = "/data/local/tmp/x";
    let Some(runs) = common::run_each(&[
        &["/system/bin/touch", f],
        &["/system/bin/setfattr", "-n", "user.omni", "-v", "hello", f],
        &["/system/bin/getfattr", "-d", "-n", "user.omni", f],
        &["/system/bin/setfattr", "-x", "user.omni", f],
        &["/system/bin/getfattr", "-d", "-n", "user.omni", f],
    ]) else {
        return;
    };
    for (status, out, err) in &runs[..4] {
        assert_eq!(*status, OK, "{out}\n{err}");
    }
    assert!(runs[2].1.contains("user.omni=\"hello\""), "{:?}", runs[2]);
    // Removed, it is gone (ENODATA; toybox's getfattr then prints only the file).
    assert!(!runs[4].1.contains("user.omni"), "{:?}", runs[4]);
}

#[test]
fn restorecon_labels_a_file_from_file_contexts() {
    let f = "/data/apex/decompressed/a.apex";
    let Some(runs) = common::run_each(&[
        &["/system/bin/mkdir", "-p", "/data/apex/decompressed"],
        &["/system/bin/touch", f],
        &["/system/bin/restorecon", f],
        &["/system/bin/ls", "-Z", f],
    ]) else {
        return;
    };
    for (status, out, err) in &runs {
        assert_eq!(*status, OK, "{out}\n{err}");
    }
    // plat_file_contexts: /data/apex/decompressed/(.*)? u:object_r:staging_data_file:s0
    assert!(runs[3].1.contains("u:object_r:staging_data_file:s0"), "{:?}", runs[3]);
}

/// `setfscreatecon`: a process writes the context of the files it will create to
/// `/proc/thread-self/attr/fscreate` and reads it back (vold, preparing user 0's storage). The
/// shell's builtins do it here: the shell cannot fork.
#[test]
fn a_process_sets_its_file_creation_context() {
    // The value reads back NUL-terminated, with no newline: `read` reports the end of file.
    let script = "echo u:object_r:system_data_file:s0 > /proc/thread-self/attr/fscreate && { read v < /proc/self/attr/fscreate; echo \"[$v]\"; } \
                  && echo > /proc/self/attr/fscreate && { read w < /proc/self/attr/fscreate; echo \"[$w]\"; }";
    let Some((status, out, err)) = common::run(&["/system/bin/sh", "-c", script]) else { return };
    assert_eq!(status, OK, "{out}\n{err}");
    assert_eq!(out, "[u:object_r:system_data_file:s0]\n[]\n", "{err}");
}

/// A recursive restorecon (the fts walk installd's `restorecon_pkgdir` makes over an app's data
/// directory) labels the tree.
#[test]
fn a_recursive_restorecon_labels_a_tree() {
    let Some(runs) = common::run_each(&[
        &["/system/bin/mkdir", "-p", "/data/misc/probe/a/b"],
        &["/system/bin/touch", "/data/misc/probe/a/b/f"],
        &["/system/bin/restorecon", "-R", "/data/misc/probe"],
        &["/system/bin/ls", "-Z", "/data/misc/probe/a/b/f"],
    ]) else {
        return;
    };
    for (status, out, err) in &runs {
        assert_eq!(*status, OK, "{out}\n{err}");
    }
    assert!(runs[3].1.contains("u:object_r:system_data_file:s0"), "{:?}", runs[3]);
}

/// selinuxfs's `context` transaction (`security_check_context`, which installd's app-data
/// restorecon asks): a well-formed context is accepted and read back; a malformed one is
/// `EINVAL`.
#[test]
fn selinuxfs_checks_a_context() {
    let script = "{ echo -n u:object_r:app_data_file:s0:c136,c256,c512,c768 >&3 && read -r v <&3; echo \"[$v]\"; } 3<>/sys/fs/selinux/context; \
                  { echo -n nonsense >&3 && echo accepted || echo refused; } 3<>/sys/fs/selinux/context";
    let Some((status, out, err)) = common::run(&["/system/bin/sh", "-c", script]) else { return };
    assert_eq!(status, OK, "{out}\n{err}");
    assert!(out.contains("[u:object_r:app_data_file:s0:c136,c256,c512,c768"), "{out}\n{err}");
    assert!(out.trim_end().ends_with("refused"), "{out}\n{err}");
}
