# OS surface inventory: what `libroblox.so` imports from the OS

`libroblox.so` (2.738.1397, identical in the stock APK) imports **565** symbols: FUNC 539,
OBJECT 23, NOTYPE 3. `python tools/os_surface.py --check --union` classifies them from the
`libroblox.so` block of `apk-undefined-symbols.txt`, cross-checks against that file's union
sections, prints every per-class list, and asserts the total. Re-run 2026-09-26: counts below
reproduce exactly. Reachability columns come from `tools/init_reach.py` (`init-reach-scan.txt`).

## 1. Classification

Rules: STT_OBJECT goes to `data-object` by type first; then name rules, first match wins;
anything unmatched stays `unclear` and is printed.

| Class | Count | Tier A (floor) | Tier C (ceiling) | Not in Tier C |
|---|---:|---:|---:|---:|
| pure (string, math, locale, `*_chk`) | 150 | 50 | 89 | 61 |
| memory | 10 | 7 | 8 | 2 |
| file-io | 73 | 35 | 39 | 34 |
| network (incl. poll/select/epoll/eventfd) | 39 | 8 | 8 | 31 |
| threads-sync (incl. signals, `__errno`) | 56 | 37 | 44 | 12 |
| time-clocks (incl. timerfd) | 17 | 7 | 13 | 4 |
| process-env | 41 | 15 | 16 | 25 |
| dynamic-link | 6 | 5 | 5 | 1 |
| logging | 7 | 4 | 4 | 3 |
| android-api (EGL 17, GLES 74, A* 50) | 141 | 0 | 0 | 141 |
| data-object | 23 | 18 | 18 | 5 |
| unclear (`__gcov_dump`, `__gcov_flush`) | 2 | 2 | 2 | 0 |
| **total** | **565** | **188** | **246** | **319** |

Tier A is a lower bound and Tier C an over-approximation of what static initializers reach
(`init_reach.py` docstring lists its seven holes). "Not in Tier C" does not mean unused.

## 2. Findings

- **No android-api symbol is initializer-reachable**, even at Tier C: static initializers force
  no EGL/GLES/NDK seam.
- **Networking is a full stack**: name resolution, TCP/UDP, `sendmmsg`/`recvmmsg`, `socketpair`,
  poll/select/epoll, eventfd. No plain `send`/`recv`: only the `to/from/msg` variants. Eight are
  already in Tier A (`eventfd freeaddrinfo gai_strerror getaddrinfo inet_ntop poll select
  socket`).
- **No allocator imports.** `malloc`, `free`, `calloc`, `realloc` are absent from
  `libroblox.so`'s imports; it carries its own allocator. `mallinfo` is imported and in Tier A.
- **Weak `__gcov_dump`/`__gcov_flush`** are tested for null before use and are deliberately left
  unresolved (`omni-android/src/bionic/absent.rs`).
- **Linux-only or Android-only shapes** with no direct desktop equivalent: `epoll_*`, `eventfd`,
  `timerfd_*` (fd identities the guest polls), `getauxval` (in Tier A), `__system_property_get`,
  `AAsset_openFileDescriptor` (a real fd for an APK asset), `mremap`, `fork`/`execv`/`execve`,
  `ptrace` (not in Tier C), `AMediaCodec_*`/`AMediaFormat_*`.

## 3. Data objects (23)

18 are in Tier A and are supplied by `omni-android/src/bionic/data.rs` (`DATA_OBJECTS`):

| Symbols | Need |
|---|---|
| `__sF`, `stdin`, `stdout`, `stderr` | `__sF` is `3 * FILE_BYTES`; the three pointers point into it. The bytes are never read: `omni-bionic::stdio` keys streams by `FILE *` address |
| `__stack_chk_guard` | one word; its value must match the canary D13 programs |
| `environ` | `char **`, NULL-terminated |
| `in6addr_any`, `in6addr_loopback` | 16-byte IPv6 constants |
| `AMEDIAFORMAT_KEY_*` (10) | pointers to C strings with the NDK key names (`"mime"`, `"width"` ...) |

The 5 not in Tier C (`daylight`, `timezone`, `tzname`, `optarg`, `optind`) are not supplied.

## 4. Where each class is served now

The original gap analysis (when `omni-platform` had only `vm` and `fault`) is obsolete. Guest
imports are bound in `omni-android` (`bionic/*` for libc, `ndk/*` for `AAsset*`, `AConfiguration*`,
`ALooper*`, `ANativeWindow*`, `vulkan/*`, `aaudio/*`), and host OS calls go through
`omni-platform`'s modules: `vm`, `fault`, `fs`, `net`, `clock`, `process`, `log`, `audio`,
`window`, `webview`, `sampler`, `hypervisor`. Not bound as of 2026-09-26 (grep of
`crates/omni-android/src`): `fork`, `execve`, `ptrace`, `mremap`, `AMediaCodec_*`.
