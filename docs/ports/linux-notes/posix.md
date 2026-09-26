# Linux: process, files, network, clock

Measured on the port host (`../linux.md`; ext4 checkout, tmpfs `/tmp`). The calls are in each
seam's `linux.rs` (and the POSIX-common `unix.rs`) module docs; this file keeps what was learned.

## Files: path confinement with real symlinks

`tests/fs_linux.rs` ran the confinement rules with real symlinks for the first time (Windows
cannot create them unprivileged; VERIFICATION entry 4): nine link shapes out of the root or into a
sibling instance's, each through every path-taking operation. **No escape.** `..` after a link is
lexical and stays inside the root; `unlink` of a final link removes the link, never its target.

Still open: the check and the use are two calls, so a link made inside the root between them by a
second party is followed (the race `path.rs` records); `openat` + `O_NOFOLLOW` per component would
close it, a shared-code change. A hard link, bind mount or FIFO placed inside the root by the
embedding is outside the rules (the guest cannot create them). `allocate` (`posix_fallocate`)
allocates from 0, because the shared signature passes only the end.

## Process

* `getrandom` flags 0 (blocks only before the CRNG's first seeding). Through glibc's vDSO it was
  never short under a 50 us signal storm (0/200); the raw syscall was (200/200), so the loop is
  tested through the raw call.
* Nice is per thread (`PRIO_PROCESS` + `gettid`). With `RLIMIT_NICE` 0 an unprivileged thread
  cannot lower its nice value at all; see `../linux.md` for FMOD and the limit.
* `CLOCK_PROCESS_CPUTIME_ID` steps by ~565 ns (Windows: 15.625 ms).

## Network (Linux against Winsock)

* `SO_ERROR` is cleared by the read. A fresh TCP socket polls `POLLOUT|POLLHUP`; a refused
  connect and a peer reset are `POLLERR|POLLHUP`; a FIN alone is readable, not hung up.
* `SO_RCVBUF`/`SO_SNDBUF` read back doubled. `EAGAIN` from `connect` is port exhaustion, not
  progress. The first `connect` after a non-blocking connect completed answers 0, only the next
  `EISCONN`; both map to `Connected`.

## Clock

A 1 ms `clock::sleep` takes 1.082 ms median (n = 41), the same with `TimerResolution` held: there
is no coarse tick to raise, so the non-Windows arm's `Ok` is true here.
