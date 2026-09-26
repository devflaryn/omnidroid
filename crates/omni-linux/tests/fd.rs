use std::sync::Arc;

use omni_linux::errno::{EBADF, EFAULT, ENOENT, EROFS};
use omni_linux::fd::Output;
use omni_linux::process::{Process, Task};
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

fn fixture() -> (Arc<Process>, Task, u64, Arc<parking_lot::Mutex<Vec<u8>>>) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    // One directory per test: tests run in parallel, and rewriting a file another test has open
    // races on Windows.
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("omni-linux-fd-{}-{n}", std::process::id()));
    std::fs::create_dir_all(dir.join("objects/aa")).unwrap();
    std::fs::write(dir.join("objects/aa/aa01"), b"hello, file\n").unwrap();
    let m = manifest::parse("d\t755\t/\nd\t755\t/system\nd\t755\t/system/bin\nf\t644\t12\taa01\t/system/bin/hello.txt\nl\thello.txt\t/system/bin/link\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&dir, m), vec![], b"/system/bin/hello.txt".to_vec());
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let p = Process::for_tests(vfs, Output::Capture(Arc::clone(&out)));
    let task = p.test_task();
    let scratch = p.scratch();
    (p, task, scratch, out)
}

fn call(p: &Process, t: &mut Task, number: u64, args: [u64; 6]) -> i64 {
    p.syscall(t, number, args) as i64
}

#[test]
fn open_read_and_close_a_sysroot_file_through_a_symlink() {
    let (p, mut t, s, _) = fixture();
    p.mem.write(s, b"/system/bin/link\0").unwrap();
    let fd = call(&p, &mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    assert!(fd >= 3, "a descriptor above the standard three, got {fd}");
    let n = call(&p, &mut t, nr::READ, [fd as u64, s + 256, 64, 0, 0, 0]);
    assert_eq!(n, 12);
    assert_eq!(p.mem.read(s + 256, 12).unwrap(), b"hello, file\n");
    assert_eq!(call(&p, &mut t, nr::READ, [fd as u64, s + 256, 64, 0, 0, 0]), 0, "end of file");
    assert_eq!(call(&p, &mut t, nr::CLOSE, [fd as u64, 0, 0, 0, 0, 0]), 0);
    assert_eq!(call(&p, &mut t, nr::CLOSE, [fd as u64, 0, 0, 0, 0, 0]), -(EBADF.0 as i64));
}

#[test]
fn write_to_stdout_is_captured_and_a_bad_buffer_is_efault() {
    let (p, mut t, s, out) = fixture();
    p.mem.write(s, b"hi\n").unwrap();
    assert_eq!(call(&p, &mut t, nr::WRITE, [1, s, 3, 0, 0, 0]), 3);
    assert_eq!(out.lock().as_slice(), b"hi\n");
    assert_eq!(call(&p, &mut t, nr::WRITE, [1, 0xdead_0000, 5, 0, 0, 0]), -(EFAULT.0 as i64));
}

#[test]
fn a_missing_file_is_enoent_and_writing_the_sysroot_is_erofs() {
    let (p, mut t, s, _) = fixture();
    p.mem.write(s, b"/system/bin/nope\0").unwrap();
    assert_eq!(call(&p, &mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]), -(ENOENT.0 as i64));
    p.mem.write(s, b"/system/bin/hello.txt\0").unwrap();
    assert_eq!(call(&p, &mut t, nr::OPENAT, [(-100i64) as u64, s, 2, 0, 0, 0]), -(EROFS.0 as i64));
}

#[test]
fn fstatat_with_an_empty_path_stats_the_descriptor() {
    let (p, mut t, s, _) = fixture();
    p.mem.write(s, b"/system/bin/hello.txt\0").unwrap();
    let fd = call(&p, &mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]) as u64;
    p.mem.write(s + 64, b"\0").unwrap();
    assert_eq!(call(&p, &mut t, nr::NEWFSTATAT, [fd, s + 64, s + 512, 0x1000, 0, 0]), 0);
    let st = p.mem.read(s + 512, 128).unwrap();
    assert_eq!(i64::from_le_bytes(st[48..56].try_into().unwrap()), 12, "st_size");
    let mode = u32::from_le_bytes(st[16..20].try_into().unwrap());
    assert_eq!(mode & 0o170000, 0o100000, "a regular file");
}

#[test]
fn readlink_of_proc_self_exe_and_getdents_of_a_directory() {
    let (p, mut t, s, _) = fixture();
    p.mem.write(s, b"/proc/self/exe\0").unwrap();
    let n = call(&p, &mut t, nr::READLINKAT, [(-100i64) as u64, s, s + 256, 256, 0, 0]);
    assert_eq!(p.mem.read(s + 256, n as usize).unwrap(), b"/system/bin/hello.txt");

    p.mem.write(s, b"/system/bin\0").unwrap();
    let fd = call(&p, &mut t, nr::OPENAT, [(-100i64) as u64, s, 0o40000, 0, 0, 0]) as u64;
    let n = call(&p, &mut t, nr::GETDENTS64, [fd, s + 1024, 2048, 0, 0, 0]);
    let buf = p.mem.read(s + 1024, n as usize).unwrap();
    let mut names = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let reclen = u16::from_le_bytes([buf[at + 16], buf[at + 17]]) as usize;
        let name = &buf[at + 19..at + reclen];
        names.push(name[..name.iter().position(|&b| b == 0).unwrap()].to_vec());
        at += reclen;
    }
    names.sort();
    assert_eq!(names, [&b"hello.txt"[..], b"link"]);
    assert_eq!(call(&p, &mut t, nr::GETDENTS64, [fd, s + 1024, 2048, 0, 0, 0]), 0, "exhausted");
}

#[test]
fn fstatfs_describes_an_ext4_filesystem() {
    let (p, mut t, s, _) = fixture();
    p.mem.write(s, b"/system/bin/hello.txt\0").unwrap();
    let fd = call(&p, &mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]) as u64;
    assert_eq!(call(&p, &mut t, nr::FSTATFS, [fd, s + 256, 0, 0, 0, 0]), 0);
    assert_eq!(p.mem.read_u64(s + 256).unwrap(), 0xEF53, "f_type EXT4_SUPER_MAGIC");
    assert_eq!(p.mem.read_u64(s + 264).unwrap(), 4096, "f_bsize");
    assert_eq!(call(&p, &mut t, nr::STATFS, [s, s + 512, 0, 0, 0, 0]), 0);
}

#[test]
fn readlink_of_proc_self_fd_names_the_open_file() {
    let (p, mut t, s, _) = fixture();
    p.mem.write(s, b"/system/bin/link\0").unwrap();
    let fd = call(&p, &mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]);
    p.mem.write(s, format!("/proc/self/fd/{fd}\0").as_bytes()).unwrap();
    let n = call(&p, &mut t, nr::READLINKAT, [(-100i64) as u64, s, s + 256, 256, 0, 0]);
    assert_eq!(p.mem.read(s + 256, n as usize).unwrap(), b"/system/bin/hello.txt", "the resolved path");
    p.mem.write(s, b"/proc/self/fd/99\0").unwrap();
    assert_eq!(call(&p, &mut t, nr::READLINKAT, [(-100i64) as u64, s, s + 256, 256, 0, 0]), -(ENOENT.0 as i64));
}

struct OneFile;

impl omni_linux::procfs::ProcFs for OneFile {
    fn node(&self, path: &[u8]) -> Option<omni_linux::vfs::Node> {
        match path {
            b"/proc" => Some(omni_linux::vfs::Node::Dir),
            b"/proc/fake" => Some(omni_linux::vfs::Node::Generated),
            _ => None,
        }
    }
    fn list(&self, _path: &[u8]) -> Vec<omni_linux::vfs::DirEnt> {
        Vec::new()
    }
    fn read(&self, path: &[u8]) -> Option<Vec<u8>> {
        (path == b"/proc/fake").then(|| b"0123456789".to_vec())
    }
}

#[test]
fn a_generated_file_reads_in_pieces_seeks_back_and_stats_like_proc() {
    let (p, mut t, s, _) = fixture();
    let fake: Arc<dyn omni_linux::procfs::ProcFs> = Arc::new(OneFile);
    p.vfs.attach_proc(Arc::downgrade(&fake));
    p.mem.write(s, b"/proc/fake\0").unwrap();
    let fd = call(&p, &mut t, nr::OPENAT, [(-100i64) as u64, s, 0, 0, 0, 0]) as u64;
    assert_eq!(call(&p, &mut t, nr::READ, [fd, s + 256, 3, 0, 0, 0]), 3);
    assert_eq!(call(&p, &mut t, nr::READ, [fd, s + 259, 100, 0, 0, 0]), 7);
    assert_eq!(p.mem.read(s + 256, 10).unwrap(), b"0123456789");
    assert_eq!(call(&p, &mut t, nr::LSEEK, [fd, 0, 0, 0, 0, 0]), 0);
    assert_eq!(call(&p, &mut t, nr::READ, [fd, s + 512, 100, 0, 0, 0]), 10);
    assert_eq!(call(&p, &mut t, nr::FSTAT, [fd, s + 1024, 0, 0, 0, 0]), 0);
    let st = p.mem.read(s + 1024, 128).unwrap();
    assert_eq!(u32::from_le_bytes(st[16..20].try_into().unwrap()), 0o100444, "a regular file, read-only");
    assert_eq!(i64::from_le_bytes(st[48..56].try_into().unwrap()), 0, "size 0, as /proc reports");
    assert_eq!(call(&p, &mut t, nr::WRITE, [fd, s, 1, 0, 0, 0]), -(omni_linux::errno::EACCES.0 as i64));
}
