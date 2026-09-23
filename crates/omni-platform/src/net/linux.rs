//! Linux backend for the network seam.
//!
//! **Structural only: unverified and not implemented.** The body is the shared
//! [`unix`](super::unix) module, and it covers exactly the eleven calls with no portable `std`
//! spelling. Everything else the seam offers is `std::net` and is implemented once for all five
//! targets, which is D22's rule applied in the direction it points.
//!
//! Linux-specific notes for whoever implements these:
//!
//! * **`socket(2)` takes flags in its `type` argument here**: `SOCK_NONBLOCK` and `SOCK_CLOEXEC`
//!   can be OR'd in, so a non-blocking close-on-exec socket is one syscall rather than three.
//!   macOS has neither. The seam's contract is that a socket starts *blocking*, so the flag is
//!   not the free win it looks like — see the shared module.
//! * **`poll(2)` reports `POLLHUP`, and `select` on Windows cannot.** That is one of the few
//!   places where this target can answer something the tested one cannot, and the shared module
//!   argues for letting it rather than flattening the answer to match.
//! * **`epoll` is the obvious temptation and is deliberately not the plan.** It is Linux-only, so
//!   it would split this file from `macos.rs`, and its state lives in a kernel object that is
//!   itself a descriptor — a second descriptor kind and a second allocator, which D30 point 2 is
//!   explicit about not wanting. `poll(2)` is what the guest asks for anyway; the engine imports
//!   `epoll_create1` and, per D17, importing is not calling.
//! * **`SO_RCVBUF` reads back doubled.** The kernel adds an equal amount for its own bookkeeping,
//!   so `setsockopt(SO_RCVBUF, 65536)` followed by `getsockopt(SO_RCVBUF)` answers 131072 on a
//!   stock kernel. macOS does not do this. A round-trip test asserting equality would therefore
//!   pass on one unix and fail on the other, which is why this crate's own asserts only that the
//!   kernel agreed to something.
//! * **`IPV6_V6ONLY` defaults from `net.ipv6.bindv6only`**, which is 0 on nearly every
//!   distribution — so a v6 socket here carries v4 traffic by default and on macOS it does not.
//!   A runtime that depends on either must set the option rather than inherit it.
//! * **The guest is an Android ARM64 binary and this host would be a Linux ARM64 one**, which is
//!   the configuration `ARCHITECTURE.md` section 6 runs guest code **natively** on. So the
//!   guest's own `struct sockaddr_in6` and the host's would be the same layout, and the adapter's
//!   marshaller would be doing a conversion that happens to be the identity. That is the target
//!   where a layout error in it would be hardest to notice — the same warning `fs::linux` carries
//!   about `struct stat`, for the same reason.

pub(super) use super::unix::{
    bind, broadcast, buffer_bytes, create_datagram, create_stream, keep_alive, keep_alive_count,
    keep_alive_idle, keep_alive_interval, kind_from_raw, linger, poll, reuse_address,
    set_broadcast, set_buffer_bytes, set_dont_fragment, set_keep_alive, set_keep_alive_count,
    set_keep_alive_idle, set_keep_alive_interval, set_linger, set_reuse_address, set_v6only,
    socket_error, start_connect, v6only,
};
