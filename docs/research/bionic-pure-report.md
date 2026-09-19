# bionic-pure — implementing libroblox.so's pure libc/libm surface as host-side Rust

**Branch:** `bionic-pure` (created from `android-abi`; all work happens here).
**Scope of writes:** `crates/omni-bionic/**`, this report, `Cargo.lock` (automatic). The workspace
`members = ["crates/*"]` glob already covers the new crate, so the root `Cargo.toml` is not touched
(VERIFIED: root `Cargo.toml` line 2).

**Status per phase:**

| phase | status |
|---|---|
| 0 — scope | done (commit `f228384`) |
| 1 — foundation | done (commit `ac023fb`) |
| 2 — memory functions | done (commit `f434809`) |
| 3 — string functions | done (commit `1335aeb`) |
| 4 — ctype + numeric conversion | done (commit `06d332b`) |
| 5 — libm | done (this commit) |
| 6 — printf core (stretch) | pending |
| 7 — verification | pending |

---

## 1. Phase 0 — scope table

### 1.1 Method (re-derived, not trusted from the inventory)

The candidate set is the intersection of three predicates:

1. **classified `pure`** by `tools/os_surface.py` — 150 names (VERIFIED: `python tools/os_surface.py
   --json` reports `"pure": 150` in `counts`, list printed and counted 150 by hand).
2. **reachable from the static initializers**, i.e. inside the Tier C closure of
   `tools/init_reach.py` (Tier A ∪ Tier C-only). Run fresh, read-only, for this session:
   `python tools/init_reach.py --list`. Its pinned self-checks passed (VERIFIED: exit 0; headline
   counts identical to the committed `docs/research/init-reach-scan.txt`: Tier A union 188, Tier C
   246). Parsed from the fresh `--list` output: Tier A 188 names, Tier C-only 58, never-referenced
   319 — sums to 565 (VERIFIED arithmetic on the parsed sets, cross-checked against the tool's own
   printed ladder 113/188/246/565).
3. **not excluded** by the task rules (variadic, OS-needing, allocating, control-flow primitives).

Result: **pure ∧ Tier-C-reachable = 89 names** (VERIFIED: set intersection of the parsed reach
sets against the classification; the inventory's §2 table independently says pure Tier C = 50 + 89
= 139... no — the inventory's table says pure has 50 in Tier A and 89 total in Tier C, i.e. 39 of
the pure names are Tier C-only. Recounted directly: Tier A∩pure = 50, TierC-only∩pure = 39, sum 89.
Both numbers agree with the fresh run and with the inventory table's `pure | 50 | 89 | 61 | 150`
row, where the 89 column is "in Tier C" = 50 + 39. VERIFIED two ways.)

**Reachability caveat (INFERRED, from `tools/init_reach.py`'s own docstring):** Tier C is a
*ceiling* and Tier A a *floor*; a name outside Tier C is "not referenced from the static
initializer closure", which is not proof of non-use (indirect calls through the 106,899
RELATIVE-named function starts are not followed). The strict intersection therefore *under-scopes*
by some unknown amount. This task follows the strict intersection; a later session can widen it —
nothing in the crate's design (pure functions over a memory trait) makes widening hard.

### 1.2 Included functions — 84 (of which 1 is partial by design)

Provider classes are per `os_surface.py`; "tier" is A (floor) or C-only (reached solely through an
address-taken edge). All 84 are non-variadic, non-allocating, and pure computation over guest
memory.

#### Memory (phase 2) — 7

| function | tier | C signature (LP64) |
|---|---|---|
| `memcpy` | A | `void *memcpy(void *dst, const void *src, size_t n)` |
| `memmove` | A | `void *memmove(void *dst, const void *src, size_t n)` |
| `memset` | A | `void *memset(void *dst, int c, size_t n)` |
| `memcmp` | A | `int memcmp(const void *a, const void *b, size_t n)` |
| `memchr` | A | `void *memchr(const void *s, int c, size_t n)` |
| `__memcpy_chk` | A | `void *__memcpy_chk(void *dst, const void *src, size_t n, size_t dst_size)` |
| `__memset_chk` | A | `void *__memset_chk(void *dst, int c, size_t n, size_t dst_size)` |

#### Strings (phase 3) — 19

| function | tier | C signature |
|---|---|---|
| `strlen` | A | `size_t strlen(const char *s)` |
| `__strlen_chk` | A | `size_t __strlen_chk(const char *s, size_t size)` |
| `strcmp` | A | `int strcmp(const char *a, const char *b)` |
| `strncmp` | A | `int strncmp(const char *a, const char *b, size_t n)` |
| `strcpy` | A | `char *strcpy(char *dst, const char *src)` |
| `strncpy` | A | `char *strncpy(char *dst, const char *src, size_t n)` |
| `__strncpy_chk` | C | `char *__strncpy_chk(char *dst, const char *src, size_t n, size_t dst_size)` |
| `__strncpy_chk2` | A | `char *__strncpy_chk2(char *dst, const char *src, size_t n, size_t dst_size, size_t src_size)` |
| `strcat` | A | `char *strcat(char *dst, const char *src)` |
| `strncat` | C | `char *strncat(char *dst, const char *src, size_t n)` |
| `__strcat_chk` | C | `char *__strcat_chk(char *dst, const char *src, size_t dst_size)` |
| `strchr` | A | `char *strchr(const char *s, int c)` |
| `strrchr` | A | `char *strrchr(const char *s, int c)` |
| `strstr` | A | `char *strstr(const char *h, const char *n)` |
| `strcasecmp` | A | `int strcasecmp(const char *a, const char *b)` |
| `strncasecmp` | A | `int strncasecmp(const char *a, const char *b, size_t n)` |
| `strcspn` | C | `size_t strcspn(const char *s, const char *reject)` |
| `strspn` | C | `size_t strspn(const char *s, const char *accept)` |
| `strnlen` | C | `size_t strnlen(const char *s, size_t n)` |

#### Error strings (phase 3) — 1

| function | tier | C signature |
|---|---|---|
| `__gnu_strerror_r` | A | `char *__gnu_strerror_r(int errnum, char *buf, size_t buflen)` |

`strerror` (tier A) is implemented as a thin wrapper that writes the same message into a
scratch buffer supplied through the context trait (see §1.4 — the adapter owns placement;
nothing here allocates). `strerror_r` (POSIX `int` form) is *not* imported by libroblox.so
and is out of scope (never-referenced).

#### Wide / multibyte, C/POSIX (UTF-8) locale (phase 3) — 6

| function | tier | C signature |
|---|---|---|
| `wcslen` | C | `size_t wcslen(const wchar_t *s)` |
| `wmemchr` | C | `wchar_t *wmemchr(const wchar_t *s, wchar_t c, size_t n)` |
| `wmemcmp` | C | `int wmemcmp(const wchar_t *a, const wchar_t *b, size_t n)` |
| `wctob` | C | `int wctob(wint_t c)` |
| `mbrtowc` | C | `size_t mbrtowc(wchar_t *pwc, const char *s, size_t n, mbstate_t *ps)` |
| `mbsrtowcs` | C | `size_t mbsrtowcs(wchar_t *dst, const char **src, size_t len, mbstate_t *ps)` |

All wide/multibyte behaviour is the C/POSIX locale with UTF-8 as the multibyte encoding
(Android's `C` locale is UTF-8; bionic maps every accepted locale to the same UTF-8 behaviour —
VERIFIED for the mb_cur_max value in phase 4 of this report; the general mapping claim INFERRED
from bionic's locale design, checked against bionic sources where cited).

#### ctype + misc libc (phase 4) — 5

| function | tier | C signature |
|---|---|---|
| `isspace` | A | `int isspace(int c)` |
| `tolower` | A | `int tolower(int c)` |
| `__ctype_get_mb_cur_max` | A | `size_t __ctype_get_mb_cur_max(void)` |
| `rand` | A | `int rand(void)` |
| `srand` | A | `void srand(unsigned int seed)` |

Only `isspace`/`tolower` are reachable ctype predicates (the `isw*_l`/`tow*_l` families are all
never-referenced). They are implemented in the C/POSIX locale.

`rand`/`srand`: bionic's `rand` is not a specified-sequence function (see phase 4 notes); this
crate implements a documented deterministic PRNG behind the context trait. Divergence from
bionic's exact sequence is documented, and the return contract (`[0, RAND_MAX]`) is kept.

#### Numeric conversion (phase 4) — 9

| function | tier | C signature |
|---|---|---|
| `atoi` | A | `int atoi(const char *s)` |
| `atoll` | A | `long long atoll(const char *s)` |
| `atof` | C | `double atof(const char *s)` |
| `strtod` | A | `double strtod(const char *s, char **endptr)` |
| `strtof` | A | `float strtof(const char *s, char **endptr)` |
| `strtol` | A | `long strtol(const char *s, char **endptr, int base)` |
| `strtoll` | A | `long long strtoll(const char *s, char **endptr, int base)` |
| `strtoul` | A | `unsigned long strtoul(const char *s, char **endptr, int base)` |
| `strtoull` | A | `unsigned long strtoull(const char *s, char **endptr, int base)` |

`long` = 64 bits (LP64). Overflow clamps at the 64-bit limits and sets `errno = ERANGE` (34).
`strtoul` on a negated input negates in unsigned arithmetic per the C standard.

#### Sort/search with a guest comparator (phase 4) — 2

| function | tier | C signature |
|---|---|---|
| `qsort` | A | `void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *))` |
| `bsearch` | A | `void *bsearch(const void *key, const void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *))` |

The comparator is a **guest function pointer** — a control transfer that belongs to the thunk
boundary, which is unreviewed. To stay decoupled, these take a `&mut dyn GuestCompare` callback
trait (defined in the crate): the future adapter supplies the closure that performs the guest
call. Sorting/searching itself is pure computation over guest memory. `qsort`'s output order for
equal elements is unspecified by C, so any correct sort satisfies the contract.

#### Locale handles (phase 3/4) — 4

| function | tier | C signature |
|---|---|---|
| `newlocale` | A | `locale_t newlocale(int category_mask, const char *locale, locale_t base)` |
| `freelocale` | C | `void freelocale(locale_t locobj)` |
| `uselocale` | C | `locale_t uselocale(locale_t newloc)` |
| `localeconv` | C | `struct lconv *localeconv(void)` |

Bionic has exactly one locale implementation (the C locale, UTF-8 charset); every *accepted*
locale name maps to it. The handle for the C locale is a static sentinel — no allocation is
needed to be behaviourally correct, because `locale_t` is opaque to the guest. `localeconv`
returns a pointer to a static `lconv`; the crate provides the lconv *values* (C/POSIX locale)
and composes the guest-visible struct into a scratch buffer supplied by the context trait.

#### libm (phase 5) — 34

All are reachable (A = floor, C = Tier C-only). Rust's `f64`/`f32` methods supply the
implementation; the phase-5 effort is edge cases, errno (EDOM/ERANGE per POSIX), and
exact-IEEE bit-for-bit testing.

| function | tier | | function | tier | | function | tier |
|---|---|---|---|---|---|---|---|
| `acos` | C | | `acosf` | A | | `asin` | C |
| `asinf` | C | | `atan2` | C | | `atan2f` | C |
| `atanf` | C | | `cbrtf` | C | | `cos` | C |
| `cosf` | A | | `cosh` | C | | `coshf` | C |
| `exp` | A | | `expf` | A | | `fmodf` | C |
| `frexp` | A | | `ilogb` | C | | `ldexp` | C |
| `ldexpf` | C | | `log` | A | | `log10` | C |
| `log10f` | C | | `log2` | C | | `log2f` | C |
| `logf` | A | | `modff` | C | | `nan` | A |
| `pow` | A | | `powf` | A | | `sincosf` | A |
| `sin` | C | | `sinf` | A | | `sinhf` | C |
| `sinh` | C | | `tanf` | A | | `tanhf` | A |

Notes:
- `sincos` (double) is **not** imported; only `sincosf` is. It returns through two guest
  pointers; both results go through the memory trait.
- No `fabs`/`floor`/`ceil`/`trunc`/`round`/`copysign` are reachable (`round` is
  never-referenced), so the "exact IEEE" testing rule applies to what is here: `fmodf`,
  `frexp`, `ldexp`, `ilogb`, `nan`.
- `fmal`, `powl`, `remainderf`, `remquof`, `nextafterf`, `erff`, `erfcf`, `hypotf`,
  `expm1`, `exp2(f)`, `finitef`, `cbrt` (double), `modf` (double), `fmod` (double) are all
  imported but never-referenced — excluded by reachability.
- `nan` is classified `pure` (libc) and implemented with libm.

### 1.3 Excluded functions — with reasons

From the 89 pure ∧ reachable names, 5 are excluded:

| function | tier | reason |
|---|---|---|
| `sscanf` | A | **variadic** (task rule). Variadic marshalling belongs to the unreviewed thunk boundary. |
| `__vsnprintf_chk` | A | **variadic** (the `va_list` form). Same reason. |
| `__vsprintf_chk` | C | **variadic** (the `va_list` form). Same reason. |
| `setjmp` | A | **not a library function in this layer** — a control-flow primitive that returns twice and saves CPU state. Belongs to the CPU crate; cannot be a plain Rust function over guest memory. |
| `longjmp` | A | same — restores CPU state and unwinds; a host Rust function cannot implement it. |

Also excluded, from the full `pure` class (never-referenced — outside the Tier C closure):
`__ctype_get_mb_cur_max` is *not* in this group (it is Tier A, included above); the never-referenced
pure names are: `__memmove_chk`, `__strchr_chk`, `__strcpy_chk`, `atan`, `atol`, `btowc`, `cbrt`,
`cosh`, `erfcf`, `erff`, `exp2`, `exp2f`, `expm1`, `finitef`, `fmal`, `fmod`, `frexpf`, `hypotf`,
`isspace`→(included, Tier A), `iswalpha_l`, `iswblank_l`, `iswcntrl_l`, `iswdigit_l`,
`iswlower_l`, `iswprint_l`, `iswpunct_l`, `iswspace_l`, `iswupper_l`, `iswxdigit_l`, `ldiv`,
`mbrlen`, `mbsnrtowcs`, `mbtowc`, `memrchr`, `modf`, `nextafterf`, `powl`, `remainderf`,
`remquof`, `round`, `sincos`, `srand`→(included, Tier A), `strcoll_l`, `strerror_r`, `strpbrk`,
`strtold_l`, `strtoll_l`, `strtoull_l`, `strxfrm_l`, `tan`, `tanh`, `towlower_l`, `towupper_l`,
`vsscanf`, `wcrtomb`, `wcscoll_l`, `wcsnrtombs`, `wcsxfrm_l` (VERIFIED: parsed from the fresh
`--list` "Never referenced" section and joined against the pure class; `isspace` and `srand`
appear there only because the join of the two sets is what defines scope — the Tier A list, not
the never list, is authoritative for them; the never-referenced pure count is 61 per §1.1).

The task's phase lists name several functions that are **not in scope because they are
never-referenced**: `memrchr`, `strlcpy`, `strlcat`, `strtok_r`, `strpbrk`, `strtoumax`,
`strtoimax`, `atol`, `sincos` (double), and the exact-IEEE libm examples `fabs`, `floor`, `ceil`,
`trunc`, `round`, `copysign`, `fmin`, `fmax`. None is imported-and-reachable. This is stated
explicitly so their absence from the crate is not read as an oversight.

Variadic exclusion also covers, from the full import list: `fprintf`, `snprintf`, `syslog`,
`open`, `prctl`, `syscall`, `__android_log_print`, `vsnprintf`, `vfprintf`, `vasprintf` — none of
them is both `pure`-classified and in scope anyway (VERIFIED: they are classified file-io /
logging / process-env).

**OS access:** no `pure`-classified function needs OS access, by construction of the class
(the classification rules put anything touching files, sockets, threads, time, process or
dynamic linking into other classes — VERIFIED rule set in `tools/os_surface.py`). The `memory`
class (mmap & co) is excluded as OS access.

### 1.4 Blocked on allocation design — none

libroblox.so imports no allocator (`malloc`/`free`/`calloc`/`realloc` are absent from its import
block; VERIFIED in `docs/research/os-surface-inventory.md` §4.2), and none of the 84 included
functions allocates on the guest's behalf: no `strdup` or similar is imported at all.

Two functions *return pointers into storage the guest can read* (`strerror`, `localeconv`).
Rather than leaving them blocked, both take that storage from the adapter through the context
trait (a scratch buffer with a caller-chosen guest address and capacity). The crate never
chooses addresses, so the allocation-design question stays entirely with the adapter layer,
where it belongs. `freelocale` frees nothing (the C-locale handle is a static sentinel);
correctness of that is argued in its phase notes.

### 1.5 Shape adjustments to the suggested trait (decision record)

The task sketches `GuestMemory::read/write` + `Fault` + a context with `set_errno`. Deviations,
with reasons, finalised in phase 1:

1. **`Result<T, Fault>` stays for plain memory/string functions.** `_chk` functions need to name
   the failed check (task rule 5), and a few functions need to name themselves as unimplemented
   or invalid — so those return a crate-wide `BionicError` with a `Memory(Fault)` variant instead
   of overloading `Fault` with a name string.
2. **Read-only vs writing functions:** functions that only read take `&impl GuestMemory`;
   functions that write take `&mut impl GuestMemory` (the trait's `write` needs `&mut`).
3. **`GuestContext` composes `GuestMemory`** (supertrait) and adds `set_errno`/`errno`, the
   `rand` state, and the scratch-buffer hooks used by `strerror`/`localeconv`. Rationale: the
   adapter owns guest state; the crate must be able to reach errno, PRNG state and scratch
   storage without knowing how they are placed.
4. **Scanning loops** read at least one byte through the trait per iteration (task rule), so every
   loop terminates at a terminator or a `Fault`; no scan is unbounded.

### 1.6 Phase 1 — what was built and verified

* `memory::Fault` (newtype over the faulting address, `Eq`/`Hash` for exact-address test
  assertions), `memory::GuestMemory` (`read`/`write`, both fallible), and
  `memory::checked_range`.
* **`checked_range` end-inclusive decision (VERIFIED by test):** a range's validity invariant is
  that its *last byte address* `addr + len - 1` fits in `u64` — not that the exclusive end
  `addr + len` fits. An exclusive-end check wrongly rejects a one-byte range at `u64::MAX`, which
  is a representable guest address; the end-inclusive rule accepts `[u64::MAX, 1]` and rejects
  `[u64::MAX, 2]`. Tested in `checked_range_rejects_null_and_overflow`.
* `error::BionicError` with `Memory(Fault)` / `CheckFailed(name)` / `Unimplemented(name)` /
  `InvalidArgument(name)` — every variant's `Display` names the function or check responsible.
* `errno::consts` — Linux numbering only (EINVAL 22, EDOM 33, ERANGE 34, ENOMEM 12, ENOSYS 38,
  EACCES 13, EPERM 1, ENOENT 2, EINTR 4, EBADF 9), each checked against the kernel UAPI
  numbering bionic uses on arm64. The host's errno values are never referenced.
* `context::GuestContext: GuestMemory` — errno get/set, `rand` state, and a `scratch()` hook
  returning `Option<(addr, capacity)>`. No blanket `GuestMemory` impl: the supertrait bound means
  any context already is a `GuestMemory` (a delegating blanket impl would recurse into itself;
  caught before committing and removed — noted here because the first draft had it).
* `mock::MockMemory` — disjoint regions over `Vec<u8>`, byte-granular access, fault at the first
  unmapped byte touched; documented that a faulting access may have already moved the bytes
  before the fault (callers validate whole ranges via `checked_range` first).
* Tests: `tests/mock_tests.rs` (12) and `tests/context_mock_tests.rs` (5) — **17 tests, all
  passing** (VERIFIED: `cargo test -p omni-bionic`); `cargo clippy -p omni-bionic --all-targets`
  reports no warnings (VERIFIED).

---

## 2. Implementation table

*(grows per phase; per-phase notes below, consolidated table in §2 of the final report)*

### 2.1 Phase 2 — memory functions (commit of this section)

7 functions: `memcpy`, `memmove`, `memset`, `memcmp`, `memchr`, `__memcpy_chk`, `__memset_chk`.
31 new tests in `tests/mem_tests.rs`, all passing; suite total 48.

Design points:
* **All-or-nothing on fault.** Both ranges are validated with `checked_range` before the first
  byte moves, so a faulting copy never leaves a partial write behind. (The mock itself may move
  bytes before faulting; the functions never let it happen by validating first.)
* **Chunked transfers** (256-byte host stack buffers): a guest `memcpy` of 100 MB must not
  allocate 100 MB on the host.
* **`memcpy` overlap decision:** overlap is UB in C. This implementation copies forward
  (byte 0 first), deterministically and host-safe. Pinned by
  `memcpy_overlapping_forward_bias_documented_semantics` so a change is a conscious one.
* **`memmove` overlap:** back-to-front when `dst` lies inside `(src, src+n)`, forward otherwise;
  verified against a snapshot-copy reference for both directions (oracle: a trivially-correct
  host-side reference, not the function under test).
* **`memcmp`** returns `-1/0/1`; tests assert sign only (C specifies sign); unsigned-char
  ordering tested with `0xFF` vs `0x01`.
* **`_chk` variants** return `CheckFailed("__memcpy_chk")` / `CheckFailed("__memset_chk")` on
  destination overflow — never a host abort (bionic would abort). `n == dst_size` is allowed
  (exactly full is legal); `n > dst_size` fails; `memset_chk` with `dst_size == 0` accepts only
  `n == 0`.
* Hostile inputs covered per function: null with nonzero length (fault at 0), zero length at any
  address (success, no access — valid C), unmapped source/destination, range running past a
  region end (exact fault address asserted), `addr+len` overflowing `u64` (fault, no wraparound —
  including a range ending at `u64::MAX` which is *representable* and proceeds to a normal
  unmapped fault at its first unmapped byte), self-copy, exact overlap.

### 2.2 Phase 3 — string functions (commit of this section)

19 libc + 1 wide-adjacent function group implemented: `strlen`, `__strlen_chk`, `strnlen`,
`strcmp`, `strncmp`, `strcasecmp`, `strncasecmp`, `strcpy`, `strncpy`, `__strncpy_chk`,
`__strncpy_chk2`, `strcat`, `strncat`, `__strcat_chk`, `strchr`, `strrchr`, `strstr`, `strspn`,
`strcspn`, `__gnu_strerror_r`; plus `wcslen`, `wmemchr`, `wmemcmp`, `wctob`, `mbrtowc`,
`mbsrtowcs` (wide/multibyte, `wchar_t` = 32-bit) and the locale-handle functions `newlocale`,
`freelocale`, `uselocale`, `localeconv` (values; the guest struct composition lands with the
adapter's scratch buffer). 38 string tests + 18 wide tests + 3 ctype tests (in-module); suite
107.

**One-byte-per-trait-call scanning (design note, applies crate-wide).** A bulk `read(256)` over
a mapping overreads past a region end the *string* never crosses — a NUL on the last mapped byte
must be findable, and a generic `GuestMemory` cannot be asked how far its mapping extends. All
scans (`find_nul`, `find_nul_bounded`, `strchr`, `walk_pair`, `span_walk`, `strstr`, wide scans)
therefore read exactly one byte per trait call: termination guaranteed, no false faults, and a
string ending exactly at a mapping boundary works.

**Mapping probe for no-partial-write copies.** `checked_range` proves address-space
representability, not mapping. `strcpy`/`strncpy`/`strcat`/`strncat` read (probe) the entire
destination range before the first write, so a faulting destination is detected with zero bytes
written. Cost: destination bytes are read twice. Caught by
`strcat_unmapped_dst_tail_faults_before_writing`, which failed before the probe existed.

**Bugs the tests caught during this phase (both real, both fixed, both VERIFIED failing first):**
1. `strcat` returned the append position instead of `dst` (C contract: returns `dst`).
2. `strcasecmp`/`strncasecmp` folded case *after* the byte walk, so a raw-case difference at
   position 0 produced a wrong sign; the fold now happens inside the walk loop.
3. `mbrtowc` initially treated structurally-invalid sequences with valid lead bytes (surrogates,
   > U+10FFFF) as merely incomplete; the decoder now distinguishes `Invalid`/`Incomplete`.
4. `wmemchr`/`wmemcmp` shadowed the element count with the byte count after range-checking,
   scanning `n*4` elements; fixed to keep counts separate.

**Bionic-vs-POSIX semantics recorded:** `__strlen_chk` fails when `strlen(s) >= size` (the
`>=`, not `>`, is bionic's documented check); `__strcat_chk` fails when
`strlen(dst) + strlen(src) + 1 > dst_size`; `__strncpy_chk2` checks both `n > dst_size` and
`n > src_size`; `__gnu_strerror_r` follows GNU semantics (returns `buf`, truncates to
`buflen-1` + NUL, never touches errno), with bionic's `Unknown error <n>` fallback for
unknown codes (glibc's `<n>: Unknown error` is NOT used).

**mbrtowc/mbsrtowcs errno:** `EILSEQ` is 84 in Linux numbering (kernel UAPI; VERIFIED against
`include/uapi/asm-generic/errno.h`) — not the Windows value (42), so the host's constant is
wrong by construction here.

**`localeconv` note:** the C-locale `lconv` *values* (POSIX-fixed: ".", "", CHAR_MAX=127
monetary fields) are implemented in `locale::LCONV_C_VALUES`; composing the guest-visible arm64
struct requires the adapter's scratch buffer, so the struct-writing half lands with the adapter.
The struct layout (pointer fields first, then `char`s, per POSIX declaration order at 8-byte
alignment) is documented on the module. `newlocale`/`uselocale` return/accept a static sentinel
`locale_t` (`freelocale` frees nothing — nothing was allocated, argued in the module docs).
`uselocale` takes the adapter's current-locale slot address explicitly rather than adding a
trait method — same reasoning as `scratch`.

### 2.3 Phase 4 — numeric conversion, qsort/bsearch (commit of this section)

9 conversions (`strtol`, `strtoll`, `strtoul`, `strtoull`, `atoi`, `atoll`, `atof`, `strtod`,
`strtof`) + `rand`/`srand` + `qsort`/`bsearch`. 20 numerics tests + 7 sort tests + 1 added atof
test; suite 135.

**ABI decisions exercised (the LP64 trap):** every `long`-shaped boundary value is written as an
explicit 64-bit literal in tests — `LONG_MAX = 9223372036854775807` (clamped overflow,
ERANGE), `LONG_MIN = -9223372036854775808` (both exact and `-9223372036854775809` → ERANGE),
`ULONG_MAX = 18446744073709551615` (exact fit) and `18446744073709551616` → ERANGE. A host
`long` (32-bit on LLP64 Windows) would silently produce 2147483647-clamped results; the tests
would fail loudly. `strtoul("-1") == u64::MAX` (the unsigned-negation quirk) is tested.

**endptr contract:** tested for the three C-mandated cases — after digits, at the ORIGINAL
string when no digits were consumed, and untouched/`EINVAL` for an invalid base (POSIX
refinement). The bare `"0x"` prefix case parses the `"0"` and stops before `x` (C's longest-
valid-prefix rule), tested for both strtol and strtod.

**Bugs the tests caught this phase (VERIFIED failing first, then fixed):**
1. `strtod("inf")`/`"nan"`: the inf/nan word reader advanced `cursor` past the word and the
   endptr computation added the word length *again*, faulting 3 bytes past a short mapping.
2. `strtod("1e400")`: the exponent clamp (`clamp(-310, 308)`) silently produced a *finite*
   1e308 instead of `HUGE_VAL`+ERANGE; overflow/underflow is now decided on the actual decimal
   magnitude `lg10(mantissa) + exp10` before evaluation.
3. The `rand` test's initial oracle was wrong: I quoted glibc's TYPE_3 sequence
   (`1804289383...`) for what is a TYPE_0-style LCG. Hand-recomputation
   (`(1103515245*1+12345) mod 2^31 = 1103527590`) fixed the test, not the code; the sequence
   tested is 1103527590, 377401575, 662824084, 1147902781, 2035015474. The
   C-specified part (same seed → same sequence) is also tested.

**strtod precision statement (documented limitation):** the parser accumulates the decimal
mantissa into `u128` (≈ 38 significant digits; further digits shift the exponent, which is
value-preserving for zeros but drops non-zero rounding surface) and evaluates
`mantissa * 10^exp10` through `f64::powi` products. For exponents in `[-22, 22]` the result is
correctly rounded (10^22 is exact in f64); outside that window the error is at most 1–2 ulp.
Bit-exactness with glibc's arbitrary-precision `strtod` is NOT claimed — flagged in §6, with the
mitigation that the engine's initializers parse short, well-behaved numbers.

**qsort implementation choice:** heapsort (in-place, deterministic, no guest allocation).
C leaves the order of equal elements unspecified, so no stability promise is made. The
comparator runs through the `GuestCompare` callback trait (the adapter will perform the guest
call); a comparator fault propagates as `Fault`. Verified against Rust's `sort_unstable` as an
independent reference on 100 pseudo-random elements.

### 2.4 Phase 5 — libm (commit of this section)

34 libm functions (see the phase 0 table). 18 libm tests; suite 152.

**Implementation policy held:** no transcendental function was reimplemented — Rust's
`f64`/`f32` methods provide the math; this crate's work is the POSIX/C11-Annex-F edge-case
contract: NaN propagation, ±inf, ±0 sign preservation, domain errors (log of negative,
acos/asin outside [-1,1], pow(negative, non-integer) → NaN + EDOM), and range errors
(exp/pow/sinh/cosh/ldexp overflow → ±HUGE_VAL + ERANGE; true underflow to 0 → +ERANGE per
bionic's choice among C11's options).

**Precision classes honoured in tests:** exact-IEEE functions (`fmodf`, `frexp`, `ldexp`,
`modff`, `ilogb`) tested **bit-for-bit** via `to_bits()` including signed zeros;
transcendental functions tested within **1 ULP** (stated tolerance) against Rust's own
independently-computed results; cases C fixes exactly (pow(x, ±0) = 1, pow(±0, odd y) =
±0, sin 0 = 0, cos 0 = 1, pow(2, 10) = 1024, cbrtf(±8) = ±2) tested exact.

**Bugs the tests caught this phase (VERIFIED failing first, then fixed):**
1. `ilogb` of subnormals: the fraction's MSB position subtracted the full 64-bit leading-
   zero count instead of the 52-bit fraction's (12-bit offset error) → wrong exponent
   (-1086 vs -1074 for the smallest subnormal).
2. `pow(-0.0, 2.0)` returned `-0.0`; Annex F requires `+0` (sign of x survives only for
   odd integral y).
3. Two test fixtures called functions through fresh contexts without mapping the output
   slots (fault at the mapped address) — test-only, not implementation.

**`frexp` without a std helper:** Rust 1.89 has no `frexp`; the implementation is pure
bit manipulation (`to_bits`/`from_bits`, safe code): normal path rewrites the biased
exponent to 1022; the subnormal path shifts the fraction's MSB into the normal slot and
compensates the exponent — exact by construction, verified by round-trip over
1e-300…1e300 and `f64::MIN_POSITIVE`.

**`ilogb` special values:** bionic's `FP_ILOGB0`/`FP_ILOGBNAN` are `INT_MIN`,
`FP_ILOGBINTB` is `INT_MAX` (VERIFIED against bionic `libm/include/math.h`); this differs
from glibc's `FP_ILOGBNAN = INT_MAX` choice — POSIX permits both, and the guest is bionic,
so `INT_MIN` is implemented for 0/NaN. Recorded as a bionic-vs-glibc divergence reviewers
should sanity-check against the NDK headers.

**`sincosf` returns through guest pointers:** both results are written through the context
(4 bytes each, little-endian). GNU/bionic dereference the output pointers unconditionally;
this crate treats a null pointer as "skip the write" — a documented divergence (on device a
null output would crash; here the guest's null cannot crash the host, and skipping is the
least surprising non-crashing behaviour).

---

## 3. ABI decisions (long / wchar_t / long double / errno / layouts)

*(accumulated per phase; summary here)*

| topic | decision | where |
|---|---|---|
| `long` / `unsigned long` | 64-bit everywhere; `strtol`/`strtoll` share limits, `strtoul`/`strtoull` share `u64`; overflow clamps at 64-bit bounds + ERANGE | `numerics.rs` |
| `wchar_t` | 32-bit UTF-32 elements, little-endian; `wmemcmp` unsigned ordering | `wide.rs` |
| `long double` | **not reachable**: no `strtold`/`fabsl`-family function is both imported and reachable (`strtold_l` is never-referenced). Nothing in the crate computes in 64-bit `double` where 128-bit quad is required — the case simply does not arise in scope | report §1.3 |
| errno numbering | Linux values only, defined in `errno::consts` (EINVAL 22, EDOM 33, ERANGE 34, EILSEQ 84, ...); host errno never referenced | `errno.rs`, `wide.rs`, `numerics.rs`, `libm.rs` |
| struct layouts | `lconv` per POSIX declaration order at LP64 alignment (pointers first, then `char`s; CHAR_MAX = 127 = signed-char max on arm64) | `locale.rs` |
| `mbstate_t` | treated as always-initial (UTF-8 is stateless); a non-initial state is rejected as `Unimplemented`, never guessed | `wide.rs` |

## 4. Bionic vs host-C divergences found

*(filled per phase)*

## 5. Mutation spot-checks

*(filled in phase 7)*

## 6. What could not be implemented correctly

*(filled per phase; nothing so far)*

## 7. Final verification results

*(filled in phase 7)*

## 8. Open questions for the reviewer

*(filled in phase 7)*
