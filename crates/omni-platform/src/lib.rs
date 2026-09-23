//! OS primitives for Omnidroid.
//!
//! This is the **only** crate in the workspace permitted to use `#[cfg(target_os = …)]` or to call
//! an operating-system API (Global Constraint 4). Everything else — `omni-mem`, `omni-apk`,
//! `omni-elf`, `omni-cpu`, `omni-android`, `omni-gfx`, `omni-core`, `omni-cli` — compiles for all
//! five targets with no `cfg` at all, and reaches the OS only through the seams defined here.
//!
//! Other crates depending on `omni-platform` is the seam working as intended, not a violation of
//! that constraint; the constraint forbids *external* OS crates (`windows-sys`, `libc`, …) outside
//! this one.
//!
//! # What is here
//!
//! * [`vm`] — virtual memory: reservation, lazy commit, decommit, protection, placeholder
//!   splitting and file-backed mapping. Implemented and measured on Windows; structural on Linux
//!   and macOS, where every operation returns a typed
//!   [`Unsupported`](vm::VmError::Unsupported) error.
//! * [`fault`] — guest memory faults: one process-wide **vectored** exception handler, so that
//!   Omnidroid sees an access violation in JIT-generated guest code before dynarmic's frame-based
//!   SEH does. D4 verified the ordering (`veh_hits = 1`, dynarmic's slow path never entered) and
//!   D10 requires it, because whoever handles the fault owns guest demand paging.
//! * [`clock`] — monotonic time, wall time and sleeping. One process-wide monotonic epoch.
//! * [`process`] — pid, cpu count, entropy, the current processor number and this process's
//!   consumed CPU time. Implemented and run on Windows; the entropy, cpu-id and cpu-time thirds
//!   are structural on Linux and macOS, where they return
//!   [`ProcessError::Unsupported`](process::ProcessError::Unsupported) naming the POSIX call they
//!   intend to make.
//! * [`log`] — a sink for a line the guest wrote, with Android's and syslog's priority scales.
//! * [`fs`] — files and directories: a **rooted** descriptor table, metadata, and directory
//!   listings. Every guest path is resolved inside one host directory supplied by the embedding,
//!   and a path that cannot be is refused by name; see [`fs::path`](fs) for the policy and the
//!   hostile cases. Fifteen of its seventeen operations are `std::fs` and are implemented once;
//!   `pread` and `statvfs` have a Windows backend and a structural unix one naming `pread(2)`
//!   and `statvfs(3)`.
//! * [`net`] — TCP and UDP client sockets, socket options, readiness over a set of sockets, and
//!   name resolution. The **only** place in the workspace where a socket call is made. It is a
//!   seam with a **policy** on it rather than an open socket: a [`Socket`](net::Socket) cannot be
//!   created without a [`NetPolicy`](net::NetPolicy), and the default reaches nothing. Sends,
//!   receives, `shutdown`, timeouts, `TCP_NODELAY` and resolution are portable `std` and are
//!   implemented once; socket *creation*, `bind`, `connect`, four socket options and readiness
//!   have a Windows backend and a structural unix one, because `std` cannot make a socket that is
//!   not already connected or bound and has no readiness call at all.
//! * [`window`] — a resizable desktop window, the handle a graphics backend puts a surface on,
//!   and a **non-blocking** drain of input and lifecycle events. Implemented and run on Windows;
//!   structural on Linux and macOS, where the window type is literally uninhabited so that the
//!   compiler discharges every operation but the one that refuses. M6's renderer is its only
//!   consumer today; GameActivity's input callbacks are the other one it exists for.
//!
//! Threads and dynamic loading may arrive as sibling modules in later tasks.
//! **Sockets did not, until M6, and the sentence that used to stand here is corrected rather than
//! deleted.** It read: *M3 task 3's network phase found that the four socket-shaped symbols the
//! guest reaches either need no OS call at all (`poll` and `select`, whose whole descriptor
//! domain is [`fs`]'s and whose answer POSIX fixes for it) or must be refused by name at the
//! adapter (`socket`, `eventfd`), so there was nothing left for a seam here to carry; D25 has the
//! argument. A later milestone that gives the guest a real network will add one.*
//!
//! That was true of a runtime that could not yet do anything a network was for. **D30 is the
//! milestone it predicted**: the project owner withdrew Global Constraint 8 because playable
//! Roblox needs login, settings and a game server, and [`net`] is the seam it required. D25's own
//! text said the day `socket` was bound for real, `poll` and `select` would have to grow a real
//! readiness source — that day has arrived, and [`net::poll`] is it.
//!
//! # Not every primitive needs a `cfg`, and saying which is part of the seam
//!
//! [`vm`] and [`fault`] are OS APIs end to end, so both have a Windows backend and a structural
//! unix one. [`clock`], [`log`], and half of [`process`] are **portable standard library** —
//! `Instant`, `SystemTime`, `thread::sleep`, `stderr`, `process::id`, `available_parallelism` —
//! and they are implemented once, with no backend and no `cfg`.
//!
//! [`fs`] is where that distinction has to be made operation by operation rather than module by
//! module, and the test it is made with is sharper than "does it call the OS": **is there one
//! `std` call that serves all five targets?** `File::open`, `fs::metadata` and `fs::read_dir`
//! are, so they are written once. `pread` is `FileExt::seek_read` on Windows and
//! `FileExt::read_at` on unix — two traits, two modules, no single call — and `statvfs` has no
//! `std` spelling at all, so those two get a backend and a structural unix half.
//!
//! [`net`] answers that question a **third** way, and D30 asked for it to be said out loud:
//! *partly*. Once a socket exists, everything done to it is one portable `std` call on all five
//! targets, so `send`, `recv`, `sendto`, `recvfrom`, `shutdown`, the non-blocking flag,
//! `TCP_NODELAY`, both timeouts and the whole of name resolution are written once. But `std` has
//! **no way to make a socket that is not already connected or bound** — `TcpStream::connect`
//! blocks and there is no `TcpStream::new` — and it has no readiness call in any form. So socket
//! creation, `bind`, `connect`, four socket options and `poll` have a backend, and the rest does
//! not.
//!
//! That asymmetry is deliberate and is written out in each module. The five-target rule this
//! project enforces is *never claim a platform works*, and a fabricated `Unsupported` return for
//! something `std` already does correctly on all five targets would be a false claim in the other
//! direction: it would assert that a clock this process can read cannot be read, and it would make
//! the non-Windows bring-up harder rather than easier. What stays unclaimed is what has been
//! **run**: nothing outside Windows x86-64 has been.
//!
//! # Diagnostics are part of the API
//!
//! [`vm::process_commit_charge`] and [`vm::process_working_set`] are public because commit charge
//! is the resource that limits how many guest instances fit on a machine (D10), and because tests
//! in other crates must be able to *assert* what a memory operation cost rather than assume it
//! (Global Constraint 6).

#![warn(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod audio;
pub mod clock;
pub mod fault;
pub mod fs;
pub mod log;
pub mod net;
pub mod process;
pub mod vm;
pub mod window;
