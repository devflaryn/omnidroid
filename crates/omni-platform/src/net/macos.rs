//! macOS backend for the network seam.
//!
//! **Structural only: unverified and not implemented.** The body is the shared
//! [`unix`](super::unix) module, covering the eleven calls with no portable `std` spelling.
//!
//! macOS-specific notes for whoever implements them, and a reason not to assume this is
//! symmetric with Linux:
//!
//! * **`socket(2)` has no `SOCK_NONBLOCK` and no `SOCK_CLOEXEC`.** Both are `fcntl` calls after
//!   the fact, so what is one syscall on Linux is three here. That is not a performance question
//!   at the rate this runtime creates sockets; it is a *window*, between the socket existing and
//!   `FD_CLOEXEC` being set, in which a `fork` would inherit the descriptor. Nothing in this
//!   runtime forks, which is what makes the window acceptable — and is worth re-checking rather
//!   than assuming if that ever changes.
//! * **`SIGPIPE` is the trap on this target.** Writing to a socket whose peer has gone raises
//!   `SIGPIPE`, and the default disposition kills the process. Linux offers `MSG_NOSIGNAL` per
//!   call; macOS does **not** have it and offers `SO_NOSIGPIPE` as a socket option instead. A
//!   backend that ported the Linux spelling would compile, run, and terminate the whole runtime
//!   the first time a connection dropped mid-write — with no error, no log line and no unwind,
//!   because a signal is not a return value. D24 records that this runtime delivers no signals to
//!   the guest, which makes it worse rather than better: the process dies and nothing in this
//!   layer ever hears about it. **`SO_NOSIGPIPE` belongs in `create_stream` on this target**, and
//!   it is written here rather than discovered later because it is the one difference in this
//!   file that is fatal rather than merely wrong.
//! * **`IPV6_V6ONLY` defaults to 1**, where Linux takes it from a sysctl that is almost always 0.
//!   So a v6 socket here does not carry v4 traffic unless the option is cleared, and a client
//!   that worked on Linux by accident will not work here.
//! * **`SO_RCVBUF` does not double.** Linux adds an equal amount for bookkeeping and this does
//!   not, so the two unix targets disagree about what `getsockopt` answers after an identical
//!   `setsockopt`. Neither is wrong; a test that asserted equality would pass here and fail
//!   there.
//! * **`poll(2)` is available and is the right call**, as on Linux. `kqueue` is the native
//!   interface and would split this file from `linux.rs` for a gain nothing has measured, in
//!   exchange for a kernel-held state object that is a second descriptor kind — which D30
//!   point 2 is explicit about not wanting.

pub(super) use super::unix::{
    bind, buffer_bytes, create_datagram, create_stream, keep_alive, keep_alive_count,
    keep_alive_idle, keep_alive_interval, kind_from_raw, poll, reuse_address, set_buffer_bytes,
    set_dont_fragment, set_keep_alive, set_keep_alive_count, set_keep_alive_idle,
    set_keep_alive_interval, set_reuse_address, set_v6only, socket_error, start_connect, v6only,
};
