//! The Linux kernel personality (sub-project A, `docs/superpowers/specs/2026-09-27-linux-abi-layer-design.md`).
//!
//! omnidroid's original layer emulates bionic *functions* for one library. This crate emulates the
//! *kernel* instead, so the real AOSP `linker64` and `libc.so` run unmodified as guest code and
//! load whatever `.so` any code asks for. Every syscall answers as Linux would; one this layer does
//! not implement answers `-ENOSYS` and is recorded by name ([`syscall::Refusals`]).
pub mod apex;
pub mod binder;
pub mod code_trim;
pub mod boot;
pub mod cpuprof;
pub mod bpf;
pub mod device;
pub mod display_window;
pub mod dnsproxy;
pub mod errno;
pub mod evdev;
pub mod exec;
pub mod fd;
pub mod fork;
pub mod fuse;
pub mod futex;
pub mod gpu;
pub mod guest;
pub mod hal;
pub mod hostnet;
pub mod init;
pub mod inject;
pub mod input_channel;
pub mod locks;
pub mod manifest;
pub mod mount;
pub mod inet;
pub mod netlink;
pub mod owners;
pub mod mm;
pub mod pipe;
pub mod poll;
pub mod process;
pub mod procfs;
pub mod props;
pub mod seccomp;
pub mod relay;
pub mod remote;
pub mod shm;
pub mod signal;
pub mod socket;
pub mod sys;
pub mod unix;
pub mod vdso;
pub mod sync_file;
pub mod syscall;
pub mod timer;
pub mod vfs;
pub mod window_input;
pub mod xattr;
pub mod xsocket;
pub mod xtables;
pub mod zygote;

pub use fd::Output;
pub use process::{Exit, ExitStatus, Process, SpawnConfig, Task};

/// Every handler this crate implements, installed into `table`.
pub fn install_all(table: &mut syscall::Table) {
    fd::install(table);
    mm::install(table);
    sys::install(table);
    socket::install(table);
    pipe::install(table);
    poll::install(table);
    timer::install(table);
    xattr::install(table);
    mount::install(table);
    fork::install(table);
    bpf::install_syscalls(table);
}
