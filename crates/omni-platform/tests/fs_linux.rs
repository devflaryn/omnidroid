//! The filesystem seam on Linux: the four backend operations against the real kernel, and the
//! **confinement rules with real symbolic links** -- which could never execute on the Windows
//! host whose suite was their evidence (VERIFICATION entry 4: `WinError 1314`, a directory
//! junction as the stand-in, and file links never tried at all).
//!
//! ```text
//! cargo test -p omni-platform --release --test fs_linux
//! ```
//!
//! Every link here is made by the test, unprivileged, with `std::os::unix::fs::symlink`, so there
//! is no fixture that can be missing. Each confinement test plants **bait outside the root** and
//! asserts on the bait afterwards -- unread, unmodified, not moved, no file appearing beside it --
//! because an escape shows up as an effect, and a refusal that was not asserted by its effect is
//! a return value somebody chose.
#![cfg(target_os = "linux")]

use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use omni_platform::fs::{AccessCheck, FileKind, Filesystem, FsError, FsErrorKind, OpenFlags};

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let at = std::env::temp_dir().join(format!(
            "omni-fslinux-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        Scratch(at)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_flags() -> OpenFlags {
    OpenFlags { read: true, ..OpenFlags::default() }
}

fn write_flags() -> OpenFlags {
    OpenFlags { write: true, create: true, truncate: true, ..OpenFlags::default() }
}

// ====================================================================== confinement

/// The world one confinement test runs in: bait outside the guest's root, a second instance's
/// root beside it, and a root seeded with every shape of link a populated root could hold.
struct Planted {
    _scratch: Scratch,
    outer: PathBuf,
    root: PathBuf,
    sibling: PathBuf,
}

const SECRET: &[u8] = b"HOST SECRET";
const PRIVATE: &[u8] = b"THE OTHER INSTANCE'S FILE";

impl Planted {
    fn new(tag: &str) -> Planted {
        let scratch = Scratch::new(tag);
        let outer = scratch.0.clone();
        let root = outer.join("root");
        let sibling = outer.join("sibling");
        for dir in [&root, &sibling, &outer.join("secretdir"), &root.join("data")] {
            std::fs::create_dir_all(dir).expect("a directory");
        }
        std::fs::write(outer.join("secret.txt"), SECRET).expect("the bait");
        std::fs::write(outer.join("secretdir/inner.txt"), SECRET).expect("the bait in a dir");
        std::fs::write(sibling.join("private.txt"), PRIVATE).expect("the sibling's file");
        std::fs::write(root.join("data/real.txt"), b"mine").expect("a file the guest owns");

        let link = |target: &Path, name: &str| {
            symlink(target, root.join(name)).unwrap_or_else(|e| panic!("symlink {name}: {e}"));
        };
        link(&outer.join("secret.txt"), "file_link"); // absolute, to a file outside
        link(Path::new("../secret.txt"), "rel_link"); // relative, to a file outside
        link(&outer.join("secretdir"), "dir_link"); // a directory outside
        link(&sibling, "sibling_link"); // another instance's root
        link(&outer.join("created-outside.txt"), "dangling"); // to nothing, outside
        link(Path::new(".."), "up_link"); // the root's parent
        link(&root.join("data/real.txt"), "inner_link"); // a link that stays inside
        link(Path::new("self_loop"), "self_loop"); // ELOOP
        symlink(Path::new("../../secretdir"), root.join("data/deep_link")).expect("a nested link");
        Planted { _scratch: scratch, outer, root, sibling }
    }

    fn fs(&self) -> Filesystem {
        Filesystem::new(&self.root).expect("a filesystem over the root")
    }

    /// Everything outside the root is as it was planted: the bait unread-into and unmoved, the
    /// sibling's file intact, and nothing new beside either.
    fn assert_outside_untouched(&self, after: &str) {
        assert_eq!(std::fs::read(self.outer.join("secret.txt")).expect("bait"), SECRET, "{after}");
        assert_eq!(
            std::fs::read(self.outer.join("secretdir/inner.txt")).expect("bait in a dir"),
            SECRET,
            "{after}"
        );
        assert_eq!(
            std::fs::read(self.sibling.join("private.txt")).expect("the sibling's file"),
            PRIVATE,
            "{after}"
        );
        let names = |dir: &Path| -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .expect("list")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&self.outer), ["root", "secret.txt", "secretdir", "sibling"], "{after}");
        assert_eq!(names(&self.outer.join("secretdir")), ["inner.txt"], "{after}");
        assert_eq!(names(&self.sibling), ["private.txt"], "{after}");
    }
}

/// A refusal of the confinement kind, and nothing else: an `Io` error here would be the host
/// having been *asked*, which is already a path that reached it.
fn assert_confined<T: std::fmt::Debug>(result: Result<T, FsError>, what: &str) {
    match result {
        Err(FsError::Confined { .. }) => {}
        other => panic!("{what}: expected a confinement refusal, got {other:?}"),
    }
}

/// Every guest path that goes **through** a link -- the link as the last component, or as a
/// directory on the way -- in every spelling a guest could write it.
const THROUGH_A_LINK: &[&str] = &[
    "/file_link",
    "/rel_link",
    "/dir_link/inner.txt",
    "/dir_link/./inner.txt",
    "//dir_link//inner.txt",
    "dir_link/inner.txt",
    "/sibling_link/private.txt",
    "/up_link/secret.txt",
    "/up_link/root/data/real.txt",
    "/data/deep_link/inner.txt",
    "/data/../dir_link/inner.txt",
    "/inner_link",
    "/self_loop",
    "/dangling",
];

/// **Reading through any link is refused, and no byte of the bait reaches the guest.**
#[test]
fn no_read_through_a_symlink_reaches_the_bait() {
    let world = Planted::new("read");
    let fs = world.fs();
    for path in THROUGH_A_LINK {
        assert_confined(fs.open(path.as_bytes(), read_flags()), &format!("open({path}, O_RDONLY)"));
        assert_confined(fs.stat(path.as_bytes()), &format!("stat({path})"));
        assert_confined(
            fs.access(path.as_bytes(), AccessCheck::Exists),
            &format!("access({path}, F_OK)"),
        );
        assert_confined(fs.statvfs(path.as_bytes()), &format!("statvfs({path})"));
    }
    for dir in ["/dir_link", "/sibling_link", "/up_link", "/data/deep_link"] {
        assert_confined(fs.opendir(dir.as_bytes()), &format!("opendir({dir})"));
        let directory = OpenFlags { directory: true, ..OpenFlags::default() };
        assert_confined(fs.open(dir.as_bytes(), directory), &format!("open({dir}, O_DIRECTORY)"));
    }
    assert_eq!(fs.open_count(), 3, "no refused open left a descriptor behind");
    world.assert_outside_untouched("after every read-shaped call");
}

/// **Writing, creating and truncating through a link is refused**, and the effect is checked:
/// the bait keeps its bytes, and a file is not created at the dangling link's target.
#[test]
fn no_write_create_or_truncate_through_a_symlink_lands_outside() {
    let world = Planted::new("write");
    let fs = world.fs();
    let flags = [
        OpenFlags { write: true, ..OpenFlags::default() },
        OpenFlags { write: true, create: true, ..OpenFlags::default() },
        OpenFlags { write: true, create: true, truncate: true, ..OpenFlags::default() },
        OpenFlags { write: true, append: true, ..OpenFlags::default() },
        OpenFlags { read: true, write: true, create: true, exclusive: true, ..OpenFlags::default() },
    ];
    for path in THROUGH_A_LINK.iter().chain(&["/dir_link/new.txt", "/sibling_link/new.txt"]) {
        for flag in flags {
            assert_confined(fs.open(path.as_bytes(), flag), &format!("open({path}, {flag:?})"));
        }
        assert_confined(
            fs.set_times(path.as_bytes(), std::time::UNIX_EPOCH, std::time::UNIX_EPOCH),
            &format!("set_times({path})"),
        );
        assert_confined(fs.access(path.as_bytes(), AccessCheck::Writable), &format!("W_OK {path}"));
    }
    world.assert_outside_untouched("after every write-shaped call");
    let secret_mtime = std::fs::metadata(world.outer.join("secret.txt")).expect("bait").mtime();
    assert!(secret_mtime > 0, "set_times through a link reached the bait's timestamps");
}

/// **The namespace calls through a link are refused**: nothing is removed, renamed out of or
/// into an outside directory, and no directory appears beside the bait.
#[test]
fn no_namespace_call_through_a_symlink_changes_anything_outside() {
    let world = Planted::new("namespace");
    let fs = world.fs();
    for path in ["/dir_link/inner.txt", "/sibling_link/private.txt", "/up_link/secret.txt"] {
        assert_confined(fs.unlink(path.as_bytes()), &format!("unlink({path})"));
        assert_confined(fs.rename(path.as_bytes(), b"/stolen.txt"), &format!("rename({path}, ..)"));
        assert_confined(
            fs.rename(b"/data/real.txt", path.as_bytes()),
            &format!("rename(.., {path})"),
        );
    }
    for dir in ["/dir_link/made", "/sibling_link/made", "/up_link/made", "/data/deep_link/made"] {
        assert_confined(fs.mkdir(dir.as_bytes()), &format!("mkdir({dir})"));
    }
    for dir in ["/dir_link", "/up_link/secretdir", "/sibling_link"] {
        assert_confined(fs.rmdir(dir.as_bytes()), &format!("rmdir({dir})"));
    }
    // `unlink` of a link as its *final* component removes the link and never its target --
    // POSIX's `unlink` does not follow, and the seam resolves it with `FinalLink::Describe` for
    // that reason. MEASURED: an earlier draft of this test asserted a refusal here, and the seam
    // was right and the draft wrong. The target outside is what must survive, and it does.
    fs.unlink(b"/file_link").expect("unlink removes the link itself");
    assert!(std::fs::symlink_metadata(world.root.join("file_link")).is_err(), "the link is gone");
    fs.unlink(b"/dangling").expect("and a dangling one");
    // `rename` resolves both ends with `Refuse`, so a link as either end is refused -- an
    // over-refusal the design accepts (the guest cannot create a link, so it never owns one).
    assert_confined(fs.rename(b"/dir_link", b"/moved"), "rename of a link");
    assert!(world.root.join("data/real.txt").exists(), "the refused rename moved the guest's file");
    assert!(!world.root.join("stolen.txt").exists(), "a rename out of an outside directory");
    world.assert_outside_untouched("after every namespace call");
}

/// **`lstat` describes a link as the final component and still refuses one on the way**, and
/// the listing of a directory holding links reports them as links -- it never stats the target.
#[test]
fn lstat_and_readdir_describe_links_without_following_them() {
    let world = Planted::new("lstat");
    let fs = world.fs();
    for name in ["/file_link", "/dir_link", "/dangling", "/self_loop", "/up_link"] {
        let described = fs.lstat(name.as_bytes()).unwrap_or_else(|e| panic!("lstat({name}): {e}"));
        assert_eq!(described.kind, FileKind::Symlink, "{name}");
    }
    assert_confined(fs.lstat(b"/dir_link/inner.txt"), "lstat through a link on the way");
    assert_confined(fs.lstat(b"/up_link/secret.txt"), "lstat through `..` as a link");

    let dir = fs.opendir(b"/").expect("the root lists");
    let mut kinds = std::collections::BTreeMap::new();
    while let Some(entry) = fs.readdir(dir).expect("readdir") {
        kinds.insert(entry.name, entry.kind);
    }
    fs.closedir(dir).expect("closedir");
    for link in ["file_link", "rel_link", "dir_link", "sibling_link", "dangling", "up_link"] {
        assert_eq!(kinds.get(link), Some(&FileKind::Symlink), "{link} in {kinds:?}");
    }
    assert_eq!(kinds.get("data"), Some(&FileKind::Directory));
}

/// **A `..` after a link is lexical, and lands inside the root** -- where the kernel would have
/// resolved it against the link's target. `/dir_link/../secret.txt` is `<outer>/secret.txt` to
/// the kernel; here it is `/secret.txt` under the root, which does not exist.
#[test]
fn a_dot_dot_after_a_link_climbs_the_guests_path_not_the_links_target() {
    let world = Planted::new("dotdot");
    let fs = world.fs();
    for path in ["/dir_link/../secret.txt", "/sibling_link/../secret.txt", "/up_link/../secret.txt"]
    {
        match fs.open(path.as_bytes(), read_flags()) {
            Err(FsError::Io { kind: FsErrorKind::NotFound, .. }) => {}
            other => panic!("{path}: {other:?} -- it must name /secret.txt inside the root"),
        }
    }
    // And inside the root the same shape resolves normally.
    std::fs::write(world.root.join("secret.txt"), b"the guest's own").expect("a guest file");
    let fd = fs.open(b"/dir_link/../secret.txt", read_flags()).expect("the guest's own file");
    let mut buf = [0u8; 32];
    let n = fs.read(fd, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"the guest's own");
    world.assert_outside_untouched("after the lexical climbs");
}

/// **Two instances cannot reach each other's files** by any path, link or not: the sibling's
/// root is a directory beside this one, and every climb is absorbed at this root.
#[test]
fn an_instance_cannot_name_a_sibling_instances_file() {
    let world = Planted::new("sibling");
    let fs = world.fs();
    for path in [
        "/../sibling/private.txt",
        "/../../sibling/private.txt",
        "../sibling/private.txt",
        "/data/../../sibling/private.txt",
        "/sibling_link/private.txt",
    ] {
        match fs.open(path.as_bytes(), read_flags()) {
            Err(FsError::Confined { .. } | FsError::Io { kind: FsErrorKind::NotFound, .. }) => {}
            other => panic!("{path}: {other:?}"),
        }
    }
    world.assert_outside_untouched("after the sibling attempts");
}

// ====================================================================== the four backend calls

/// Signals `target` with a no-op `SIGUSR1` (no `SA_RESTART`) every 50 us until told to stop, so
/// that a long read in `target` is interrupted between pages. Returns the stop switch and the
/// signalling thread.
fn signal_storm(target: libc::pthread_t) -> (Arc<AtomicBool>, std::thread::JoinHandle<u64>) {
    extern "C" fn nothing(_: libc::c_int) {}
    // SAFETY: a handler for SIGUSR1 that touches nothing; nothing else in this binary uses it.
    unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = nothing as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&raw mut action.sa_mask);
        assert_eq!(libc::sigaction(libc::SIGUSR1, &raw const action, core::ptr::null_mut()), 0);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let stop = Arc::clone(&stop);
        // `pthread_t` is an integer (`c_ulong`) on Linux, so the closure is `Send` as it is.
        std::thread::spawn(move || {
            let mut sent = 0;
            while !stop.load(Ordering::Relaxed) {
                // SAFETY: the target thread outlives the storm: it stops it and joins it first.
                unsafe { libc::pthread_kill(target, libc::SIGUSR1) };
                sent += 1;
                std::thread::sleep(Duration::from_micros(50));
            }
            sent
        })
    };
    (stop, handle)
}

/// **`pread` is one call and a short read is reported, not completed.**
///
/// The source that returns short is real: `/dev/urandom`, which the kernel fills a page at a
/// time and stops between pages when a signal is pending. MEASURED here: `pread(2)` of 16 MiB
/// from it under this storm returns short 200 times in 200. So a `pread` that looped until the
/// buffer was full would never return short here, and that is what this detects -- a loop turns
/// one guest `pread` into several host ones and makes the guest's own short-read handling
/// unreachable. The root is `/dev` so that `/urandom` is a host path under it, not the seam's
/// own `/dev/urandom` device.
#[test]
fn pread_is_one_call_and_a_short_read_under_signals_is_reported() {
    let fs = Filesystem::new("/dev").expect("a root at /dev");
    let fd = fs.open(b"/urandom", read_flags()).expect("the host's /dev/urandom");
    // SAFETY: no arguments.
    let (stop, storm) = signal_storm(unsafe { libc::pthread_self() });
    let mut buffer = vec![0u8; 16 << 20];
    let mut short = 0;
    for _ in 0..16 {
        let n = fs.pread(fd, &mut buffer, 0).expect("pread from /dev/urandom");
        assert!(n > 0 && n <= buffer.len(), "{n}");
        if n < buffer.len() {
            short += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let sent = storm.join().expect("the storm");
    eprintln!("pread of 16 MiB from /dev/urandom under {sent} signals: {short} of 16 short");
    assert!(short > 0, "16 preads under {sent} signals and none short: the seam loops");
}

/// `pread` and `pwrite` leave the descriptor's offset where it was, report a short read at the
/// end of the file, and `pwrite` past the end leaves a hole the kernel fills with zeroes.
#[test]
fn pread_and_pwrite_are_positional_and_report_what_the_kernel_did() {
    let scratch = Scratch::new("positional");
    let fs = Filesystem::new(&scratch.0).expect("a filesystem");
    let fd = fs.open(b"/f", OpenFlags { read: true, ..write_flags() }).expect("open");
    assert_eq!(fs.write(fd, b"0123456789").expect("write"), 10);
    let mut tail = [0u8; 8];
    assert_eq!(fs.pread(fd, &mut tail, 7).expect("pread"), 3, "a short read at the end, reported");
    assert_eq!(&tail[..3], b"789");
    assert_eq!(fs.pread(fd, &mut tail, 10).expect("pread at the end"), 0);
    assert_eq!(fs.pread(fd, &mut tail, 1 << 40).expect("pread far past the end"), 0);
    assert_eq!(fs.pwrite(fd, b"AB", 2).expect("pwrite"), 2);
    assert_eq!(fs.pwrite(fd, b"Z", 14).expect("pwrite past the end"), 1);
    assert_eq!(fs.seek(fd, 0, 1).expect("the offset"), 10, "neither call moved the offset");
    let mut all = [0xFFu8; 16];
    let n = fs.pread(fd, &mut all, 0).expect("pread all");
    assert_eq!(&all[..n], b"01AB456789\0\0\0\0Z", "the hole reads as zeroes");
}

/// **`fallocate` allocates**: the blocks exist afterwards, not merely the length. A body that
/// extended with `ftruncate` would leave a sparse file -- `st_blocks` of zero -- where a later
/// write can still fail with `ENOSPC`, which is the one thing `posix_fallocate` promises against.
#[test]
fn fallocate_allocates_blocks_and_never_shortens() {
    let scratch = Scratch::new("fallocate");
    let fs = Filesystem::new(&scratch.0).expect("a filesystem");
    let fd = fs.open(b"/f", write_flags()).expect("open");
    const MIB: u64 = 1 << 20;
    fs.fallocate(fd, 0, MIB).expect("posix_fallocate 1 MiB");
    let meta = std::fs::metadata(scratch.0.join("f")).expect("metadata");
    assert_eq!(meta.len(), MIB, "the file was extended to the range's end");
    assert!(
        meta.blocks() * 512 >= MIB,
        "{} bytes allocated for a 1 MiB fallocate: the file is sparse",
        meta.blocks() * 512
    );
    fs.fallocate(fd, 0, 16).expect("a range inside the file");
    assert_eq!(std::fs::metadata(scratch.0.join("f")).expect("metadata").len(), MIB, "shortened");
    fs.fallocate(fd, MIB, MIB).expect("a range past the end");
    assert_eq!(std::fs::metadata(scratch.0.join("f")).expect("metadata").len(), 2 * MIB);
}

/// `stat -f`'s answer for a path, as `(fundamental block size, total blocks, name max)`.
///
/// coreutils is the **independent** oracle here -- it reads `statvfs` itself, with its own
/// structure and its own field selection (`%S` is documented as "fundamental block size (for
/// block counts)"), so a seam that took `f_bsize` would disagree with it on any volume where the
/// two differ. On this host they never do (MEASURED on every mount); `fs::linux`'s unit test
/// covers the case where they differ.
fn stat_f(path: &Path) -> (u64, u64, u64) {
    let out = std::process::Command::new("stat")
        .args(["-f", "-c", "%S %b %l"])
        .arg(path)
        .output()
        .expect("coreutils stat");
    assert!(out.status.success(), "stat -f {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).expect("utf-8");
    let fields: Vec<u64> = text.split_whitespace().map(|f| f.parse().expect("a number")).collect();
    (fields[0], fields[1], fields[2])
}

/// **`statvfs` answers the host's own numbers**, checked against coreutils on the two file
/// systems this host has under a writable root: tmpfs (`/tmp`) and ext4 (this worktree).
#[test]
fn statvfs_answers_what_the_host_volume_says() {
    let tmp = Scratch::new("statvfs");
    let worktree = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for root in [tmp.0.as_path(), worktree.as_path()] {
        let fs = Filesystem::new(root).expect("a filesystem");
        let stats = fs.statvfs(b"/").expect("statvfs");
        let (block, blocks, name_max) = stat_f(root);
        assert_eq!(stats.block_size, block, "{}: the block counts' unit", root.display());
        assert_eq!(stats.blocks, blocks, "{}", root.display());
        assert_eq!(stats.name_max, name_max, "{}", root.display());
        assert!(stats.blocks_available <= stats.blocks_free && stats.blocks_free <= stats.blocks);
        assert!(!stats.read_only, "{} is writable", root.display());
        eprintln!(
            "statvfs {}: {} blocks of {} bytes, {} free, {} available, name_max {}",
            root.display(),
            stats.blocks,
            stats.block_size,
            stats.blocks_free,
            stats.blocks_available,
            stats.name_max
        );
    }
}
