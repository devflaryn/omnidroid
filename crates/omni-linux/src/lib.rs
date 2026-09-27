//! The Linux kernel personality (sub-project A, `docs/superpowers/specs/2026-09-27-linux-abi-layer-design.md`).
//!
//! omnidroid's original layer emulates bionic *functions* for one library. This crate emulates the
//! *kernel* instead, so the real AOSP `linker64` and `libc.so` run unmodified as guest code and
//! load whatever `.so` any code asks for. Every syscall answers as Linux would; one this layer does
//! not implement answers `-ENOSYS` and is recorded by name ([`syscall::Refusals`]).
pub mod apex;
pub mod binder;
pub mod boot;
pub mod errno;
pub mod exec;
pub mod fd;
pub mod futex;
pub mod guest;
pub mod init;
pub mod manifest;
pub mod mm;
pub mod pipe;
pub mod poll;
pub mod process;
pub mod procfs;
pub mod props;
pub mod shm;
pub mod signal;
pub mod socket;
pub mod sys;
pub mod syscall;
pub mod vfs;

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
}
