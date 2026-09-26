# omni-bionic: bionic's pure surface as host-side Rust

`crates/omni-bionic` implements bionic (Android libc/libm) functions that `libroblox.so` imports
and that are computation over guest memory: no OS access, zero dependencies, `forbid(unsafe_code)`,
compiles unchanged on every host. `omni-android/src/bionic/*` is the adapter that binds them to
imports and supplies what this crate cannot (host threads, descriptors, sockets, signals,
`setjmp`/`longjmp`, varargs marshalling). The crate began with the 84 pure functions below and
has since grown threads/sync state machines, stdio, scanf, time, wctype and unwind modules; the
crate's `lib.rs` header records its current scope and what stays in the adapter.

## 1. Original scope (2026-09-20)

Candidate set: imports classified `pure` by `tools/os_surface.py` (150) and in the Tier C closure
of `tools/init_reach.py` = **89**; minus 5 excluded at the time (`sscanf`, `__vsnprintf_chk`,
`__vsprintf_chk` as variadic; `setjmp`, `longjmp` as CPU-state primitives) = **84**:

| Group | Count | Functions |
|---|---:|---|
| memory | 7 | `memcpy memmove memset memcmp memchr __memcpy_chk __memset_chk` |
| strings | 19 + 1 | `strlen __strlen_chk strnlen strcmp strncmp strcasecmp strncasecmp strcpy strncpy __strncpy_chk __strncpy_chk2 strcat strncat __strcat_chk strchr strrchr strstr strspn strcspn`, `__gnu_strerror_r` (+ `strerror` wrapper) |
| wide/multibyte | 6 | `wcslen wmemchr wmemcmp wctob mbrtowc mbsrtowcs` |
| ctype/misc | 5 | `isspace tolower __ctype_get_mb_cur_max rand srand` |
| numeric | 9 | `atoi atoll atof strtod strtof strtol strtoll strtoul strtoull` |
| sort/search | 2 | `qsort bsearch` (comparator via the `GuestCompare` trait) |
| locale | 4 | `newlocale freelocale uselocale localeconv` |
| libm | 34 | trig, hyperbolic, exp/log, `pow(f)`, `frexp ldexp(f) ilogb fmodf modff cbrtf sincosf nan` |

The three variadic ones and `setjmp`/`longjmp` are now bound in the adapter
(`omni-android/src/bionic/handlers.rs`), using this crate's `printf` and `scanf` engines.

## 2. Design rules

- Every guest access goes through `memory::GuestMemory` and can fail; a failure is a returned
  error, never a host panic. Address ranges are checked (`checked_range`: the last byte
  `addr + len - 1` must fit in `u64`, so `[u64::MAX, 1]` is valid).
- No plausible stubs: `BionicError::{Memory, CheckFailed, Unimplemented, InvalidArgument}` each
  name the function or check.
- Copies validate (and for string copies, probe) the whole destination before the first write:
  a fault leaves no partial write. Transfers are chunked (256-byte host buffers).
- String scans read one byte per trait call, so a NUL on the last mapped byte is found and no
  scan overreads a mapping.
- Storage the guest reads back (`strerror`, `localeconv`) comes from the adapter through
  `GuestContext` (scratch buffer); the crate never picks addresses. `libroblox.so` imports no
  allocator.
- `_chk` failures return `CheckFailed(name)` rather than aborting the host.

## 3. ABI decisions

| Topic | Decision |
|---|---|
| `long` | 64-bit (LP64); `strtol` overflow clamps to +/-2^63 with `ERANGE` |
| `wchar_t` | 32-bit, little-endian |
| errno | Linux numbers only (`errno::consts`: EINVAL 22, EDOM 33, ERANGE 34, EILSEQ 84 ...); host errno never used |
| `long double` | not in scope (no reachable function needs it) |
| `mbstate_t` | UTF-8 is stateless; a non-initial state is `Unimplemented` |
| `lconv` | POSIX field order at LP64 alignment, C-locale values |
| locale | one C/UTF-8 locale; `locale_t` is a static sentinel, `freelocale` frees nothing |

## 4. Bionic semantics chosen over host C

| Area | Implemented (bionic) |
|---|---|
| `strcmp`/`strncmp`/`memcmp` | byte difference `c - d` (`"a"` vs `"z"` = -25), not +/-1 |
| `__strlen_chk` | fails when `strlen(s) >= size` |
| `__gnu_strerror_r` | GNU form; unknown codes give `Unknown error <n>` |
| `ilogb` | 0 and NaN give `INT_MIN`, inf gives `INT_MAX` |
| `pow(-0.0, 2.0)` | `+0` (Annex F) |
| printf `%e` | at least 2 exponent digits; `%a` minimal nibbles |
| printf `%n` | refused (`FormatError::NNotSupported`) before any output |
| `sincosf` null output | skipped (on device it would crash) |
| `rand` | LCG `r = (1103515245*r + 12345) mod 2^31`; after `srand(1)`: 1103527590, 377401575, 662824084 ... Bionic bit-exactness not claimed |
| `strtod` | `u128` decimal mantissa times `10^e` via `powi`: correctly rounded for exponents in [-22, 22], within 1-2 ulp outside |
| `qsort` | heapsort (in place, not stable; C allows either) |
| overlapping `memcpy` | copies forward, deterministic |

Printf gaps: positional `%n$`, `'` grouping, `%ls`/`%lc`, locale decimal point.

## 5. Verification method

Tests assert exact IEEE bits for `fmodf frexp ldexp modff ilogb`, 1 ulp for transcendentals, and
exact 64-bit boundary literals for numeric conversion. Mutation spot-checks (15 breaks) found four
test gaps, each fixed: multi-chunk backward `memmove`, hugely negative `strtol` overflow, a bad
continuation byte in `mbrtowc`, and `strcmp` magnitude (which led to the byte-difference choice).
