#!/usr/bin/env python3
"""Classify libroblox.so's 565 imported symbols by OS resource class. Read-only.

The report this feeds, ``docs/research/os-surface-inventory.md``, needs every one of the
565 imports in exactly one of 12 classes, with counts that sum to 565. Doing that by hand
invites exactly the miscount that pinned self-checks exist to catch, so this script does
the parsing and bucketing, and every non-obvious placement is an explicit named rule that
can be reviewed rather than re-derived.

Source of truth: ``docs/research/apk-undefined-symbols.txt``, section
``PER-LIBRARY UNDEFINED SYMBOL LISTS``, ``libroblox.so`` block (line 728). It is the
committed enumeration behind ARCHITECTURE section 5 and the same file
``tools/init_reach.py`` parses for its provider grouping.

Two independent parse paths over the same file cross-check each other:

* path 1 (default): the indented per-library block (``  name  BINDING KIND``),
* path 2 (``--union``): the union sections at the top, taking names whose
  ``<- libroblox.so`` attribution includes libroblox.so.

Both must produce the same 565-name set or the script exits non-zero.

Classification order:

1. ``STT_OBJECT`` symbols go to ``data-object`` by *type*, before any name rule. This is
   the task text's own definition of the class ("an STT_OBJECT data symbol, not a
   function") and matches ``tools/init_reach.py``'s treatment of data imports as
   addresses with host storage behind them.
2. Name rules below, first match wins.
3. Anything unmatched stays ``unclear`` and is printed, so a name can never fall into a
   bucket silently.

Usage::

    python tools/os_surface.py                # counts + lists to stdout
    python tools/os_surface.py --json         # machine-readable
    python tools/os_surface.py --check        # also assert the total is 565
    python tools/os_surface.py --union        # also cross-check the two parse paths
    python tools/os_surface.py --reach FILE   # cross-reference a reachability name list
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOC = ROOT / "docs/research/apk-undefined-symbols.txt"

EXPECTED_TOTAL = 565

CLASSES = [
    "pure",
    "memory",
    "file-io",
    "network",
    "threads-sync",
    "time-clocks",
    "process-env",
    "dynamic-link",
    "logging",
    "android-api",
    "data-object",
    "unclear",
]

# --------------------------------------------------------------------------
# Name rules, first match wins. Every non-obvious placement is here on purpose;
# the judgment calls are documented in NOTES and quoted in the report.
# --------------------------------------------------------------------------

REGEX_RULES: list[tuple[str, re.Pattern[str]]] = [
    # --- Android/GLES/EGL surface (examples named by the task text, plus prefix families) ---
    ("android-api", re.compile(r"^(egl[A-Z]|gl[A-Z])")),
    ("android-api", re.compile(r"^(AAsset|AConfiguration|ALooper|AMedia(?!FORMAT)|ANative)")),
    # --- logging ---
    ("logging", re.compile(r"^__android_log")),
    ("logging", re.compile(r"^(syslog|openlog|closelog)$")),
    # --- dynamic link ---
    ("dynamic-link", re.compile(r"^dl(_iterate_phdr|addr|close|error|open|sym)$")),
    # --- memory ---
    ("memory", re.compile(r"^(mmap|munmap|mprotect|madvise|msync|mlock|mremap|mallinfo)$")),
    ("memory", re.compile(r"^(getpagesize|sysconf)$")),
    # --- network (sockets + the fd-poll family the task text files here + name resolution) ---
    ("network", re.compile(
        r"^(socket|socketpair|bind|listen|accept|accept4|connect|shutdown|getsockname|"
        r"getpeername|getsockopt|setsockopt|send|sendto|sendmsg|sendmmsg|recv|recvfrom|"
        r"recvmsg|recvmmsg|inet_pton|inet_ntop|if_nametoindex|if_indextoname|gethostbyname|"
        r"poll|select|epoll_create|epoll_create1|epoll_ctl|epoll_wait|eventfd|getaddrinfo|"
        r"getnameinfo|freeaddrinfo|gai_strerror|__sendto_chk|__FD_CLR_chk|__FD_ISSET_chk|"
        r"__FD_SET_chk|__cmsg_nxthdr)$")),
    # --- threads/sync (incl. signal machinery: judgment call, see NOTES) ---
    ("threads-sync", re.compile(r"^(pthread_|sem_)")),
    ("threads-sync", re.compile(
        r"^(sigaction|sigprocmask|sigfillset|sigemptyset|sigaddset|sigdelset|sigismember|"
        r"sigpending|sigsuspend|sigwait|sigaltstack|raise|signal|__errno|"
        r"__cxa_thread_atexit_impl)$")),
    # --- time ---
    ("time-clocks", re.compile(
        r"^(clock|clock_gettime|clock_settime|clock_getres|gettimeofday|nanosleep|time|"
        r"gmtime|gmtime_r|localtime|localtime_r|mktime|tzset|difftime|strftime|strftime_l|"
        r"usleep|timerfd_create|timerfd_settime)$")),
    # --- process/environment ---
    ("process-env", re.compile(
        r"^(getenv|putenv|setenv|unsetenv|getpid|getppid|gettid|getuid|geteuid|gethostname|"
        r"uname|sysinfo|exit|_exit|_Exit|atexit|abort|execv|execve|fork|waitpid|getpriority|"
        r"setpriority|sched_setscheduler|sched_getscheduler|sched_yield|getentropy|getrandom|"
        r"getauxval|arc4random_buf|prctl|ptrace|syscall|__assert|__assert2|__register_atfork|"
        r"__stack_chk_fail|__cxa_atexit|__cxa_finalize|android_set_abort_message|"
        r"__system_property_get|sched_get_priority_max|sched_get_priority_min|sched_getcpu|"
        r"sched_getparam|getopt_long)$")),
    # --- file I/O (incl. stdio, fd fortify wrappers, terminals) ---
    ("file-io", re.compile(
        r"^(open|openat|close|read|pread|pread64|readv|write|pwrite|pwrite64|writev|lseek|"
        r"ftruncate|fsync|fcntl|ioctl|unlink|rename|mkdir|rmdir|access|fchmod|fchown|utimes|"
        r"utime|opendir|closedir|readdir|stat|fstat|lstat|getcwd|realpath|readlink|statvfs|"
        r"posix_fallocate|dup|dup2|pipe|mkstemp|fopen|fdopen|fclose|fread|__fread_chk|fwrite|"
        r"fseek|fseeko|ftell|ftello|feof|ferror|clearerr|fflush|fputc|fputs|fputwc|putc|puts|"
        r"getc|getwc|fgets|fgetwc|ungetc|ungetwc|fscanf|fprintf|vfprintf|printf|snprintf|"
        r"vsnprintf|asprintf|vasprintf|fileno|setvbuf|remove|__open_2|__read_chk|__write_chk|"
        r"__readlink_chk|tcgetattr|tcsetattr)$")),
    # --- pure: string/byte/_conversion/math/locale/jump ---
    ("pure", re.compile(
        r"^(memchr|memcmp|memcpy|memmove|memrchr|memset|__memcpy_chk|__memmove_chk|__memset_chk|"
        r"strcasecmp|strcat|strchr|strcmp|strcoll_l|strcpy|strcspn|strerror|strerror_r|strlen|"
        r"strncasecmp|strncat|strncmp|strncpy|strnlen|strpbrk|strrchr|strspn|strstr|strtod|"
        r"strtof|strtol|strtold_l|strtoll|strtoll_l|strtoul|strtoull|strtoull_l|strxfrm_l|"
        r"__strcat_chk|__strchr_chk|__strcpy_chk|__strlen_chk|__strncpy_chk|__strncpy_chk2|"
        r"__vsnprintf_chk|__vsprintf_chk|__gnu_strerror_r|__ctype_get_mb_cur_max|"
        r"qsort|bsearch|rand|srand|ldiv|isspace|tolower|localeconv|nan|"
        r"setjmp|longjmp|newlocale|freelocale|uselocale|"
        r"btowc|wctob|wcrtomb|wctomb|mbrlen|mbrtowc|mbsnrtowcs|mbsrtowcs|mbtowc|wmemchr|wmemcmp|"
        r"acos|acosf|asin|asinf|atan|atan2|atan2f|atanf|cbrt|cbrtf|cos|cosf|cosh|coshf|exp|exp2|"
        r"exp2f|expf|expm1|finitef|fmod|fmodf|frexp|frexpf|hypotf|ilogb|ldexp|ldexpf|log|log10|"
        r"log10f|log2|log2f|logf|modf|modff|pow|powf|powl|sin|sincos|sincosf|sinf|sinh|sinhf|tan|"
        r"tanf|tanh|tanhf|erff|erfcf|nextafterf|remainderf|remquof|fmal|round|"
        r"atof|atoi|atol|atoll|sscanf|vsscanf)$")),
    ("pure", re.compile(r"^(wcs|isw|tow)")),
]

#: Judgment calls that a reviewer would otherwise have to reverse-engineer. Keyed by name.
NOTES: dict[str, str] = {
    "sysconf": "task text files sysconf under memory ('sysconf-for-page-size')",
    "abort": "task text files abort under process-env",
    "sigaction family": "signals have no class in the task text; filed under threads-sync as "
    "per-thread state (masks, alt stack) that pairs with the crash-handler/fault seam",
    "__errno": "address of thread-local errno -> TLS mechanics -> threads-sync",
    "__cxa_thread_atexit_impl": "thread-local destructor registration -> threads-sync",
    "__cxa_atexit/__cxa_finalize": "task text files atexit/__cxa_atexit under process-env",
    "mallinfo": "heap introspection -> memory (it queries the allocator, which this binary "
    "does not otherwise import -- see the report's surprise section)",
    "epoll/poll/select/eventfd/__FD_*_chk/__cmsg_nxthdr": "task text: 'poll/epoll if used for "
    "sockets' -> network",
    "timerfd_create/timerfd_settime": "timer primitives -> time-clocks; Linux-only API, "
    "flagged in the report",
    "tcgetattr/tcsetattr": "terminal attribute ioctls -> file-io",
    "syscall": "generic syscall entry -> process-env; which syscall numbers are issued is "
    "unknown from the import table alone",
    "ptrace": "process control -> process-env; flagged in the report",
    "prctl": "process control -> process-env",
    "__assert/__assert2": "assert failure path (log + abort) -> process-env",
    "__stack_chk_fail": "abort-path helper -> process-env",
    "AMEDIAFORMAT_KEY_*": "STT_OBJECT -> data-object by type, before any name rule",
}

# --------------------------------------------------------------------------
# Parsing
# --------------------------------------------------------------------------

BLOCK_RE = re.compile(r"^  (\S+)\s+(GLOBAL|WEAK|LOCAL)\s+(FUNC|OBJECT|NOTYPE|IFUNC|TLS)\s*$")


def parse_per_library_block() -> list[tuple[str, str]]:
    """Path 1: the libroblox.so block in PER-LIBRARY UNDEFINED SYMBOL LISTS."""
    lines = DOC.read_text(encoding="utf-8", errors="replace").splitlines()
    start = None
    for i, line in enumerate(lines):
        if line.startswith("libroblox.so  (565 undefined"):
            start = i + 2  # skip the dashed rule line directly beneath the header
            break
    if start is None:
        raise SystemExit("could not find the libroblox.so block")
    out: list[tuple[str, str]] = []
    for line in lines[start:]:
        if line.startswith("---") or line.startswith("=="):
            break
        m = BLOCK_RE.match(line)
        if m:
            out.append((m.group(1), m.group(3)))
    return out


def parse_union_sections() -> set[str]:
    """Path 2: the union sections, names attributed to libroblox.so."""
    names: set[str] = set()
    current: str | None = None
    header = re.compile(r"^### (.+?)\s+\(\d+ symbols\)\s*$")
    for line in DOC.read_text(encoding="utf-8", errors="replace").splitlines():
        m = header.match(line)
        if m:
            t = m.group(1).strip()
            current = None if (t.startswith("PER-LIBRARY") or t.startswith("FULL ")) else t
            continue
        if line.startswith("### "):
            current = None
            continue
        if current is None or not line or line.startswith(("=", "-", " ")):
            continue
        if "<- " not in line:
            continue
        left, right = line.split("<- ", 1)
        libs = [x.strip() for x in right.split(",")]
        if any(x.startswith("libroblox.so") for x in libs):
            names.add(left.split()[0])
    return names


def classify(name: str, kind: str) -> str:
    if kind == "OBJECT":
        return "data-object"
    for cls, rx in REGEX_RULES:
        if rx.search(name):
            return cls
    return "unclear"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--check", action="store_true", help="assert the total is 565")
    ap.add_argument("--union", action="store_true", help="cross-check the two parse paths")
    ap.add_argument("--reach", default="",
                    help="path to a reachability list (one name per line, '#' comments ok)")
    args = ap.parse_args()

    parsed = parse_per_library_block()
    names = [n for n, _ in parsed]
    kinds = {n: k for n, k in parsed}

    dupes = sorted(n for n in set(names) if names.count(n) > 1)
    if dupes:
        print(f"DUPLICATE NAMES in block: {dupes}", file=sys.stderr)
        return 1

    buckets: dict[str, list[str]] = {c: [] for c in CLASSES}
    for n in names:
        buckets[classify(n, kinds.get(n, "?"))].append(n)
    for c in buckets:
        buckets[c].sort()

    total = len(names)
    ok = True

    if args.union:
        union = parse_union_sections()
        block_set = set(names)
        if union == block_set:
            print(f"CROSS-CHECK ok: union-section attribution set == per-library block "
                  f"({len(block_set)} names)")
        else:
            ok = False
            print("CROSS-CHECK MISMATCH:", file=sys.stderr)
            print(f"  union-only: {sorted(union - block_set)}", file=sys.stderr)
            print(f"  block-only: {sorted(block_set - union)}", file=sys.stderr)

    if total != EXPECTED_TOTAL:
        ok = False
        print(f"TOTAL MISMATCH: {total} != {EXPECTED_TOTAL}", file=sys.stderr)

    reach = None
    if args.reach:
        reach = {
            ln.split()[0]
            for ln in Path(args.reach).read_text(encoding="utf-8").splitlines()
            if ln.strip() and not ln.startswith("#")
        }

    if args.json:
        print(json.dumps({
            "total": total,
            "counts": {c: len(buckets[c]) for c in CLASSES},
            "kinds": {c: {n: kinds.get(n, "?") for n in buckets[c]} for c in CLASSES},
            "symbols": buckets,
        }, indent=1))
        return 0 if ok else 1

    print(f"libroblox.so imports: {total} (expected {EXPECTED_TOTAL})")
    print(f"kind census: " + ", ".join(
        f"{k}={sum(1 for kk in kinds.values() if kk == k)}"
        for k in sorted(set(kinds.values()))))
    print()
    print(f"{'class':<14} {'count':>5}")
    print("-" * 21)
    for c in CLASSES:
        print(f"{c:<14} {len(buckets[c]):>5}")
    print("-" * 21)
    print(f"{'TOTAL':<14} {total:>5}")
    print()

    for c in CLASSES:
        syms = buckets[c]
        print(f"### {c}  ({len(syms)})")
        if syms:
            width = max(len(s) for s in syms)
            percol = 3
            rows = (len(syms) + percol - 1) // percol
            cols = [syms[i * rows:(i + 1) * rows] for i in range(percol)]
            for r in range(rows):
                print("  ".join(f"{col[r] if r < len(col) else '':<{width}}" for col in cols))
        print()

    if reach is not None:
        print(f"REACHABILITY cross-reference ({len(reach)} names in the provided list)")
        print(f"{'class':<14} {'reached':>8} {'of':>5}")
        print("-" * 30)
        for c in CLASSES:
            r = sum(1 for s in buckets[c] if s in reach)
            print(f"{c:<14} {r:>8} {len(buckets[c]):>5}")
        print("-" * 30)
        print(f"{'TOTAL':<14} "
              f"{sum(1 for s in names if s in reach):>8} {len(names):>5}")
        missing = sorted(set(names) - reach)
        if missing:
            print(f"(names in the block but not in the reach list: {len(missing)})")

    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
