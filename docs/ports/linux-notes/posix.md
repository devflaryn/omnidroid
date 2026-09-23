# Linux port notes: the POSIX worker (process, fs, net, clock)

Host for every figure below: Ubuntu 26.04, kernel 7.0.0-30-generic, glibc 2.43, Rust 1.98.1,
i5-4460 (4 cores), 7 GB RAM; `/tmp` is tmpfs (3.6 GB), the worktree is ext4. Linux x86-64
only -- **nothing here has been run on Linux ARM64 or on macOS.** No graphics figures are in this
file, so the lavapipe caveat does not arise.

## What is implemented, and where

| seam | POSIX-common (`unix.rs`, compiled on macOS too, **not run there**) | Linux-only (`linux.rs`) |
|---|---|---|
| process | `cpu_time` = `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` | `random_bytes` (`getrandom`, looped), `current_cpu` (`sched_getcpu`), `set_current_thread_nice` / `current_thread_host_priority` (`setpriority`/`getpriority(PRIO_PROCESS, gettid())`), `host_manufacturer` (`/sys/class/dmi/id/sys_vendor`, trimmed) |
| fs | `pread`/`pwrite` (`FileExt::read_at`/`write_at`, one call), `volume_stats` (`statvfs`, `f_frsize`) | `allocate` (`posix_fallocate(fd, 0, end)`) |
| net | `bind`, `listen`, `start_connect`, `SO_*`/`IPV6_V6ONLY` options, `SO_ERROR`, `poll(2)`, `getifaddrs`, errno table | `socket`/`accept4` with `SOCK_CLOEXEC` (blocking), `TCP_KEEPIDLE/INTVL/CNT` (4/5/6), `IP_MTU_DISCOVER`/`IPV6_MTU_DISCOVER` |
| clock | unchanged (`clock.rs` is shared); its non-Windows `TimerResolution` arm is **measured** true | -- |

The macOS-only pieces in each `unix.rs` stay the structural `Unsupported` refusals macOS's backend
re-exports (they are `allow(dead_code)` on Linux, where `linux.rs` has its own).

## Measured findings

### process

* **getrandom: blocking (flags 0), not `GRND_NONBLOCK`.** It blocks only until the CRNG is first
  seeded after boot; `arc4random_buf` has no error path, and bionic seeds the same way.
* **glibc 2.43 answers `getrandom` from the vDSO and it is never short under signals.** n = 200
  calls of 16 MiB with SIGUSR1 to the thread every 50 us: glibc `getrandom` short **0 / 200**;
  `syscall(SYS_getrandom)` short **200 / 200**; `pread` of `/dev/urandom` short **200 / 200**.
  EINTR never occurred (the kernel only returns it while waiting for the first seeding). So the
  loop's short-fill arm is exercised through the raw syscall (a test-only source), and the EINTR
  arm through a scripted source with getrandom(2)'s documented contract.
* **`sched_getcpu` matches `sched_setaffinity`** pinning on each of the 4 cpus.
* **Nice is per thread** (`PRIO_PROCESS` + `gettid`), verified with a bystander thread read by id.
* **RLIMIT_NICE here is 0 (`ulimit -e`), no CAP_SYS_NICE: an unprivileged thread cannot lower
  its nice value at all** (floor 20; not even from 19 back to 0). `setpriority(-16)` answers
  **EACCES**, reported as the new `ProcessError::Errno { errno: 13 }`. See open issue 1.
* `host_manufacturer` = `"ASUS"` (the file is `ASUS\n`).
* `CLOCK_PROCESS_CPUTIME_ID` is nanosecond-grained: smallest non-zero step between consecutive
  readings **565 ns** (n = 10,000 readings, 10,000 steps), where Windows ticks at 15.625 ms.

### fs

* **Confinement ran with real symlinks for the first time (VERIFICATION entry 4) and held.**
  `tests/fs_linux.rs`: bait outside the root, a sibling instance's root, nine link shapes
  (absolute file link, relative, directory, `..`, nested `../../`, intermediate, dangling, a
  self-loop, one that stays inside), each driven through open (5 flag sets), stat, access
  (F_OK/W_OK), statvfs, opendir, open(O_DIRECTORY), set_times, unlink, rename (both ends), mkdir,
  rmdir, lstat, readdir -- then the bait, the sibling's file and the outer listings are checked.
  **No escape, no finding in shared code.** `readdir` reports links as links (no target stat);
  `..` after a link is lexical and lands inside the root (the kernel would have left it).
  The one assertion that failed was the draft test's: `unlink` of a final link removes the link
  (`FinalLink::Describe`, deliberately), never its target -- the seam was right.
* **Still open, documented, not a new finding:** the check and the use are two calls, so a link
  created inside the root *between* them by a second party is followed (the TOCTOU race
  `path.rs` already records). `openat` + `O_NOFOLLOW` per component would close it on Linux; it
  is a change to shared `path.rs`/`mod.rs`. Also populator-only hazards the rules do not cover
  (by design, since the guest cannot create them): a hard link or a bind mount inside the root to
  an outside file, and a FIFO inside the root (an `open` of one blocks the host thread).
* **Fidelity note (not security):** host file names containing a Windows-reserved character
  (`: \ * ? < > " |`) written into the root by the *embedding* (not through the seam) are listed
  to the guest as that character but opened through its U+F0xx stand-in, i.e. they cannot be
  opened back. Files the guest creates through the seam are unaffected.
* `pread` of 16 MiB from `/dev/urandom` under signals: **16 / 16 short**, reported as short.
* `statvfs` agrees with coreutils `stat -f` (`%S %b %l`) on tmpfs (942,934 blocks x 4096) and
  ext4 (57,411,411 x 4096). **`f_bsize == f_frsize` on every mount of this host** (tmpfs, ext4,
  squashfs 128 KiB, fuseblk 512, ...), so the choice of `f_frsize` is unit-tested on an
  NFS-shaped record (`f_bsize` 1 MiB, `f_frsize` 4 KiB).
* `posix_fallocate` of 1 MiB leaves `st_blocks * 512 >= 1 MiB`; a 1 PiB request is the host's
  error (tmpfs `ENOSPC`) and leaves the file as it was.

### net

* Linux `SO_ERROR` is **cleared** on read (Winsock was measured not clearing).
* `getsockname` on an unbound socket answers `0.0.0.0:0` itself (Winsock: `WSAEINVAL`).
* A fresh TCP socket polls **`POLLOUT | POLLHUP`** (`TCP_CLOSE`); a fresh UDP socket `POLLOUT`.
  A refused connect is `POLLERR | POLLHUP`; a peer's reset is `POLLERR | POLLHUP` even when only
  `POLLIN` was asked; a peer FIN alone is readable, not hung up; FIN + own `SHUT_WR` is `POLLHUP`.
* `SO_RCVBUF`/`SO_SNDBUF` 65,536 read back **131,072** (both kinds); a request of 4 x
  `rmem_max` (4,194,304 here) reads back 2 x `rmem_max`. The seam reports the kernel's number.
* Non-blocking loopback TCP connect answers `EINPROGRESS`; a second connect while the SYN is
  dropped (accept queue full via `listen(0)`) answers `EALREADY` -> `InProgress`; on a connected
  socket `EISCONN` -> `Connected`. `EAGAIN` from connect is **not** progress on Linux (it is port
  exhaustion), unlike Winsock's `WSAEWOULDBLOCK`.
* **The first `connect` after a non-blocking connect has completed answers 0, not `EISCONN`**
  (the kernel reports the completion once); only the call after that answers `EISCONN`. Both map
  to `Connected`. Found by mutation row `lnx-net-A7` going NOT CAUGHT on a test that made only
  one extra call -- measured with the seam and independently with Python's `connect_ex`.
* Keep-alive: 120/31/7 round-trip and read back raw as options 4/5/6; the kernel's ranges
  (`TCP_KEEPIDLE`/`INTVL` 1..=32767, `TCP_KEEPCNT` 1..=127) come back as `InvalidInput`.
* Path MTU: a fresh socket (both families) reads `IP_PMTUDISC_WANT` (1) -> the seam's `None`;
  DONT/DO/PROBE are 0/2/3 in the kernel. `net.ipv4.ip_no_pmtu_disc = 0` here.
* `IPV6_V6ONLY` defaults to `net.ipv6.bindv6only` = 0 here; a v4 datagram reaches a `[::]` socket.
* `poll(2)`'s timeout is rounded **up** to whole ms (POSIX "at least"); a 150 ms idle poll
  returns 0 after >= 150 ms. `MAX_POLL_SOCKETS` (1024) is kept for all hosts.
* All sockets are created close-on-exec (`SOCK_CLOEXEC`, `accept4`), blocking by default --
  checked with `fcntl` in the kernel's own flags.

### clock

Median of n = 41 `clock::sleep(1 ms)`, timed with `Instant`:

| condition | min | median | p90 | max |
|---|---|---|---|---|
| nothing raised | 1.041 ms | **1.082 ms** | 1.086 ms | 1.092 ms |
| `TimerResolution` held | 1.028 ms | **1.083 ms** | 1.089 ms | 1.099 ms |
| `sleep(100 us)` | 123 us | **178 us** | 181 us | 183 us |

So the non-Windows `TimerResolution` arm's `Ok` is true on this host: no coarse tick to raise.

## Open issues

1. **FMOD's `setpriority(0, 0, -16)` will be refused on this host.** The adapter
   (`omni-android/src/bionic/procenv.rs`, shared) turns the host's error into a refusal; a device
   answers 0 because AOSP sets `RLIMIT_NICE 40`. With `ulimit -e` = 0 every FMOD thread dies on
   its first call here, and `omni-android/tests/bionic.rs::setpriority_applies_the_nice_value_to_the_calling_host_thread`
   will fail on Linux. Options, for whoever owns the adapter: raise `RLIMIT_NICE`
   (`/etc/security/limits.conf` `nice`), grant `CAP_SYS_NICE`, or decide an adapter policy for a
   host that refuses. Not decided here -- the seam reports the truth.
2. `allocate` allocates `0..end` because the shared backend signature passes only the end; a
   sparse file's holes before the guest's offset are allocated too. Passing the offset would let
   Linux allocate exactly the guest's range.
3. The TOCTOU symlink race above (shared `path.rs`), closable on Linux with `openat`/`O_NOFOLLOW`.

## Tests and mutation rows

* `~/odb/cargo-locked test -p omni-platform --release --no-fail-fast`: **exit 0** -- lib 153
  passed (was 125 + 1 failed + 2 ignored at base: `dev_urandom` failed on the missing entropy
  backend), `fs_linux` 10, `net_loopback_linux` 35, `clock_linux` 2, `net_seam` 11, `vm_seam` 9,
  `window_seam` 8; the live/hardware files are `#[ignore]`d. The window/audio/webview/vm/fault
  targets did not fail here.
* `~/odb/cargo-locked test -p omni-bionic --release --no-fail-fast`: **exit 0**.
* New test files: `tests/fs_linux.rs`, `tests/net_loopback_linux.rs`, `tests/clock_linux.rs`;
  unit tests in `process/linux.rs`, `fs/linux.rs`, `net/linux.rs`.
* `tools/lnx_rows/posix.py`: 41 rows (proc 8, fs 9, net 22, clock 2; 34 A, 7 B). Whole Linux
  table (with `lnx-build-A1`): first run **41/42 caught**, `lnx-net-A7` NOT CAUGHT (the EISCONN
  finding above); after the test fix, `lnx-net-A7` caught, and the whole-table rerun is recorded
  in the final report. Rows `lnx-fs-A7`/`A8`/`B1` mutate shared `fs/path.rs` and
  `lnx-clock-*` shared `clock.rs`, temporarily, to prove the Linux tests detect them.
* Not given rows, because nothing on this host can reach them: `GRND_NONBLOCK` (the CRNG is
  seeded), `EAGAIN` from `connect` treated as progress (needs ephemeral-port exhaustion), a
  `getpriority` that treats every `-1` as an error (an unprivileged thread cannot reach nice -1).

## Shared edits (for the merge notes)

| file | what | why |
|---|---|---|
| `process/error.rs` | new variant `ProcessError::Errno { operation, api, errno }` | POSIX calls report `errno`, a third number space beside `NTSTATUS` and `GetLastError` |
| `process/mod.rs` (tests only) | two `cfg_attr(ignore)` and two "only Windows has a backend" asserts widened to `windows \| linux` | they asserted Linux had no backend |
| `fs/mod.rs` (tests only) | `mod sockets` compiled on `any(windows, linux)` | the socket-in-the-descriptor-table tests now have a Linux backend to run on |

The module docs in shared `process/mod.rs`, `fs/mod.rs`, `net/mod.rs` and `lib.rs` still say
"`Unsupported` on Linux and macOS" for these primitives; they are now true of macOS only. Left
for the merge rather than edited here.
