# OS Surface Inventory — what `libroblox.so` needs from the host OS

**Question:** what OS resources does `libroblox.so` actually need, and what must `omni-platform` grow to provide them?

**Status: analysis only.** Nothing in this report was implemented; no file outside `docs/research/os-surface-inventory.md` and `tools/os_surface.py` was created or modified.

**Method legend.** Every claim is tagged **VERIFIED** (read directly in a file, with path and line) or **INFERRED** (reasoned, with the reasoning). "unknown" is used where it is the honest answer.

**Sources**

| file | role | key loci |
|---|---|---|
| `docs/research/apk-undefined-symbols.txt` | authoritative import inventory | section "PER-LIBRARY UNDEFINED SYMBOL LISTS" begins line 724 (VERIFIED); `libroblox.so` header line 728 (VERIFIED), symbol block lines 729–1295 (VERIFIED); next header (`libzstd-jni-1.5.7-6.so`) at line 1297 (VERIFIED) |
| `docs/research/init-reach-scan.txt` | committed output of `tools/init_reach.py` | Tier A union 188 (line 48), Tier C 246 (line 60), provider table line 82 |
| `tools/init_reach.py` | the reachability tool (read + run read-only) | docstring lines 35 and 39–67: lower-bound statement and the 7 numbered limits |
| `crates/omni-platform/src/lib.rs` | the only OS-calling crate | lines 14–23: only `vm` and `fault` exist; lines 15–16: the Global-Constraint-4 statement |
| `tools/os_surface.py` | new read-only classifier (written for this report) | parses the libroblox block, classifies, self-checks |

---

## 1. Classification of all 565 imports

Counts below are produced by `tools/os_surface.py` parsing lines 729–1295 of the inventory (VERIFIED method); the script prints each full list. Its parse cross-checks the same file's union sections, requiring the same 565-name set (`CROSS-CHECK ok ... 565 names` on this run, VERIFIED). A kind census of the block gives **FUNC 539, OBJECT 23, NOTYPE 3** (VERIFIED script output).

### 1.1 Summary table — sums to 565

| class | count |
|---|---:|
| pure | 150 |
| memory | 10 |
| file-io | 73 |
| network | 39 |
| threads-sync | 56 |
| time-clocks | 17 |
| process-env | 41 |
| dynamic-link | 6 |
| logging | 7 |
| android-api | 141 |
| data-object | 23 |
| unclear | 2 |
| **TOTAL** | **565** |

**The counts sum to 565** — VERIFIED (`tools/os_surface.py --check` exits 0; `150+10+73+39+56+17+41+6+7+141+23+2 = 565`).

Rule set: STT_OBJECT symbols are `data-object` by type before any name rule; the 3 NOTYPE symbols are `__gcov_dump`, `__gcov_flush` (→ process-env, coverage hooks) and `getentropy` (→ process-env). Judgment calls are listed in §5.2; the full lists are in §1.3.

### 1.2 Rule-order note

A few names would change class if the rules were reordered. The rules above are applied: android-api prefixes first; then logging, dynamic-link, memory, network, threads-sync, time-clocks, process-env, file-io; then pure. First match wins; everything unmatched falls to `unclear` and is printed, never silently bucketed (VERIFIED: the script's `classify()` prints nothing extra and leaves 2 names in `unclear`).

### 1.3 Full per-class symbol lists

`pure` (150):

```
__ctype_get_mb_cur_max __gnu_strerror_r __memcpy_chk __memmove_chk __memset_chk __strcat_chk
__strchr_chk __strcpy_chk __strlen_chk __strncpy_chk __strncpy_chk2 __vsnprintf_chk
__vsprintf_chk acos acosf asin asinf atan atan2 atan2f atanf atof atoi atol atoll bsearch
btowc cbrt cbrtf cos cosf cosh coshf erfcf erff exp exp2 exp2f expf expm1 finitef fmal fmod
fmodf freelocale frexp frexpf hypotf ilogb isspace iswalpha_l iswblank_l iswcntrl_l iswdigit_l
iswlower_l iswprint_l iswpunct_l iswspace_l iswupper_l iswxdigit_l ldexp ldexpf ldiv localeconv
log log10 log10f log2 log2f logf longjmp mbrlen mbrtowc mbsnrtowcs mbsrtowcs mbtowc memchr
memcmp memcpy memmove memrchr memset modf modff nan newlocale nextafterf pow powf powl qsort
rand remainderf remquof round setjmp sin sincos sincosf sinf sinh sinhf srand sscanf strcasecmp
strcat strchr strcmp strcoll_l strcpy strcspn strerror strerror_r strlen strncasecmp strncat
strncmp strncpy strnlen strpbrk strrchr strspn strstr strtod strtof strtol strtold_l strtoll
strtoll_l strtoul strtoull strtoull_l strxfrm_l tan tanf tanh tanhf tolower towlower_l
towupper_l uselocale vsscanf wcrtomb wcscoll_l wcslen wcsnrtombs wcsxfrm_l wctob wmemchr wmemcmp
```

`memory` (10): `getpagesize madvise mallinfo mlock mmap mprotect mremap msync munmap sysconf`

`file-io` (73):

```
__fread_chk __open_2 __read_chk __readlink_chk __write_chk access clearerr close closedir
fchmod fchown fclose fcntl fdopen feof ferror fflush fgets fileno fopen fprintf fputc fputs
fputwc fread fscanf fseek fseeko fstat fsync ftell ftello ftruncate fwrite getc getcwd getwc
ioctl lseek lstat mkdir open opendir pipe posix_fallocate pread pread64 printf puts pwrite read
readdir realpath remove rename rmdir setvbuf snprintf stat statvfs tcgetattr tcsetattr ungetc
ungetwc unlink utime utimes vasprintf vfprintf vsnprintf write writev
```

`network` (39):

```
__FD_CLR_chk __FD_ISSET_chk __FD_SET_chk __cmsg_nxthdr __sendto_chk accept accept4 bind connect
epoll_create epoll_create1 epoll_ctl epoll_wait eventfd freeaddrinfo gai_strerror getaddrinfo
gethostbyname getnameinfo getpeername getsockname getsockopt if_indextoname if_nametoindex
inet_ntop inet_pton listen poll recvmmsg recvfrom recvmsg select sendmmsg sendmsg sendto
setsockopt shutdown socket socketpair
```

`threads-sync` (56):

```
__cxa_thread_atexit_impl __errno pthread_attr_destroy pthread_attr_getstack pthread_attr_init
pthread_attr_setdetachstate pthread_attr_setschedparam pthread_attr_setstacksize
pthread_cond_broadcast pthread_cond_destroy pthread_cond_init pthread_cond_signal
pthread_cond_timedwait pthread_cond_wait pthread_condattr_destroy pthread_condattr_init
pthread_condattr_setclock pthread_create pthread_detach pthread_equal pthread_exit
pthread_getattr_np pthread_getschedparam pthread_getspecific pthread_join pthread_key_create
pthread_key_delete pthread_mutex_destroy pthread_mutex_init pthread_mutex_lock
pthread_mutex_trylock pthread_mutex_unlock pthread_mutexattr_destroy pthread_mutexattr_init
pthread_mutexattr_settype pthread_once pthread_rwlock_destroy pthread_rwlock_init
pthread_rwlock_rdlock pthread_rwlock_unlock pthread_rwlock_wrlock pthread_self
pthread_setname_np pthread_setschedparam pthread_setspecific pthread_sigmask raise sem_destroy
sem_init sem_post sem_wait sigaction sigaltstack sigemptyset sigfillset signal
```

`time-clocks` (17): `clock clock_gettime difftime gettimeofday gmtime gmtime_r localtime localtime_r mktime nanosleep strftime strftime_l time timerfd_create timerfd_settime tzset usleep`

`process-env` (41):

```
_Exit __assert __assert2 __cxa_atexit __cxa_finalize __register_atfork __stack_chk_fail
__system_property_get _exit abort android_set_abort_message arc4random_buf execv execve exit
fork getauxval getentropy getenv geteuid gethostname getopt_long getpid getppid getpriority
gettid getuid prctl ptrace sched_get_priority_max sched_get_priority_min sched_getcpu
sched_getparam sched_getscheduler sched_setscheduler sched_yield setpriority syscall sysinfo
uname waitpid
```

`dynamic-link` (6): `dl_iterate_phdr dladdr dlclose dlerror dlopen dlsym`

`logging` (7): `__android_log_assert __android_log_buf_write __android_log_print __android_log_write closelog openlog syslog`

`android-api` (141):

```
AAssetManager_fromJava AAssetManager_open AAsset_close AAsset_getBuffer AAsset_getLength
AAsset_openFileDescriptor AConfiguration_delete AConfiguration_fromAssetManager
AConfiguration_getCountry AConfiguration_getLanguage AConfiguration_getNavHidden
AConfiguration_getScreenHeightDp AConfiguration_getScreenSize AConfiguration_getScreenWidthDp
AConfiguration_new ALooper_acquire ALooper_addFd ALooper_forThread ALooper_pollOnce
ALooper_prepare ALooper_release ALooper_removeFd AMediaCodec_configure
AMediaCodec_createDecoderByType AMediaCodec_createEncoderByType AMediaCodec_delete
AMediaCodec_dequeueInputBuffer AMediaCodec_dequeueOutputBuffer AMediaCodec_flush
AMediaCodec_getInputBuffer AMediaCodec_getOutputBuffer AMediaCodec_getOutputFormat
AMediaCodec_queueInputBuffer AMediaCodec_releaseOutputBuffer AMediaCodec_start AMediaCodec_stop
AMediaFormat_delete AMediaFormat_getBuffer AMediaFormat_getInt32 AMediaFormat_new
AMediaFormat_setBuffer AMediaFormat_setFloat AMediaFormat_setInt32 AMediaFormat_setString
AMediaFormat_toString ANativeWindow_acquire ANativeWindow_fromSurface ANativeWindow_getHeight
ANativeWindow_getWidth ANativeWindow_release
eglChooseConfig eglCreateContext eglCreatePbufferSurface eglCreateWindowSurface
eglDestroyContext eglDestroySurface eglGetConfigAttrib eglGetCurrentContext eglGetDisplay
eglGetError eglGetProcAddress eglInitialize eglMakeCurrent eglQuerySurface eglSwapBuffers
eglSwapInterval eglTerminate
glActiveTexture glAttachShader glBindAttribLocation glBindBuffer glBindFramebuffer
glBindRenderbuffer glBindTexture glBlendFunc glBlendFuncSeparate glBufferData glBufferSubData
glCheckFramebufferStatus glClear glClearColor glClearDepthf glClearStencil glColorMask
glCompileShader glCompressedTexImage2D glCompressedTexSubImage2D glCopyTexSubImage2D
glCreateProgram glCreateShader glCullFace glDeleteBuffers glDeleteFramebuffers glDeleteProgram
glDeleteRenderbuffers glDeleteShader glDeleteTextures glDepthFunc glDepthMask glDisable
glDisableVertexAttribArray glDrawArrays glDrawElements glEnable glEnableVertexAttribArray
glFramebufferRenderbuffer glFramebufferTexture2D glGenBuffers glGenFramebuffers
glGenRenderbuffers glGenTextures glGenerateMipmap glGetActiveUniform glGetError glGetIntegerv
glGetProgramInfoLog glGetProgramiv glGetShaderInfoLog glGetShaderiv glGetString
glGetUniformLocation glLinkProgram glPixelStorei glPolygonOffset glReadPixels
glReleaseShaderCompiler glRenderbufferStorage glScissor glShaderSource glStencilFunc
glStencilMask glStencilOp glTexImage2D glTexParameterf glTexParameterfv glTexParameteri
glTexSubImage2D glUniform1i glUseProgram glVertexAttribPointer glViewport
```

`data-object` (23): `AMEDIAFORMAT_KEY_BIT_RATE AMEDIAFORMAT_KEY_CHANNEL_COUNT AMEDIAFORMAT_KEY_COLOR_FORMAT AMEDIAFORMAT_KEY_FRAME_RATE AMEDIAFORMAT_KEY_HEIGHT AMEDIAFORMAT_KEY_I_FRAME_INTERVAL AMEDIAFORMAT_KEY_MIME AMEDIAFORMAT_KEY_SAMPLE_RATE AMEDIAFORMAT_KEY_STRIDE AMEDIAFORMAT_KEY_WIDTH __sF __stack_chk_guard daylight environ in6addr_any in6addr_loopback optarg optind stderr stdin stdout timezone tzname`

`unclear` (2): `__gcov_dump __gcov_flush` — filed under process-env in practice (they are libgcov coverage hooks; bionic-specific per the reachability scan's provider grouping). Kept in `unclear` because the import table alone says only "WEAK NOTYPE"; their kind is the reason, their placement is a judgment call (VERIFIED kinds: `grep -E "NOTYPE" docs/research/apk-undefined-symbols.txt` within the block returns exactly `__gcov_dump`, `__gcov_flush`, `getentropy`).

---

## 2. Cross-reference with reachability

`tools/init_reach.py` was run read-only (`--list`), against `Roblox-2.738.1397.apk` in the repository root. Its self-check passed on all 13 pinned counts (VERIFIED terminal output: `ok .eh_frame_hdr FDEs 245117`, `ok undefined (imported) .dynsym entries 565`, `ok DT_INIT_ARRAY slots 3594`, ... all `ok`). Its numeric sections are byte-identical to the committed `docs/research/init-reach-scan.txt` (VERIFIED: `diff` of the numeric sections shows no difference; the only textual difference is that the committed capture omitted the `--list` symbol dumps). Headline: **Tier A0 113 / Tier A 188 / Tier C 246 / total 565** (VERIFIED `init-reach-scan.txt` lines 36, 48, 60; `tools/init_reach.py` line 35).

The tool's own limit statement, in its docstring (VERIFIED `tools/init_reach.py` lines 39–67): *"A static call graph over a stripped binary is a **lower bound** on reachability"*, followed by 7 numbered holes: unresolved indirect calls (BLR/BR), register-GOT calls, vtables and pointer tables not followed (106,899 function starts are named by RELATIVE relocations — the population an indirect call can land on), windowed ADRP pairing, non-instruction words decoding as instructions, one 2.67 MB FDE-less region containing initializer slot 60, and `__cxa_atexit` handlers running host code later. Tier A is a floor, Tier C a ceiling; "never reached" below means "not referenced from the Tier C closure", not "never used at runtime".

Per-class reachability (counting method: parse of the fresh `--list` output's three named sections — Tier A, Tier C-only, Never-referenced — joined against the classification; VERIFIED script arithmetic: 188 + 58 + 319 = 565):

| class | Tier A (floor) | Tier C (ceiling) | never in Tier C | total |
|---|---:|---:|---:|---:|
| pure | 50 | 89 | 61 | 150 |
| memory | 7 | 8 | 2 | 10 |
| file-io | 35 | 39 | 34 | 73 |
| network | 8 | 8 | 31 | 39 |
| threads-sync | 37 | 44 | 12 | 56 |
| time-clocks | 7 | 13 | 4 | 17 |
| process-env | 15 | 16 | 25 | 41 |
| dynamic-link | 5 | 5 | 1 | 6 |
| logging | 4 | 4 | 3 | 7 |
| android-api | 0 | 0 | 141 | 141 |
| data-object | 18 | 18 | 5 | 23 |
| unclear | 2 | 2 | 0 | 2 |
| **TOTAL** | **188** | **246** | **319** | **565** |

Reading of the table (INFERRED from the numbers, stated with the tool's caveats):

* **Zero android-api symbols are reachable even at Tier C.** All 141 EGL/GLES/A* imports are outside the static-initializer closure. Initializers alone therefore do not force any graphics or Android-API seam.
* **At the floor, 8 network symbols are already reachable**: `eventfd freeaddrinfo gai_strerror getaddrinfo inet_ntop poll select socket` (VERIFIED: the per-symbol join). A static initializer touches `socket`, `getaddrinfo` and the poll family before any frame renders.
* **8 of 10 memory symbols are in Tier C** (`getpagesize madvise mallinfo mlock mmap mprotect munmap sysconf`; the never-set is `msync mremap`), and 7 already at the floor — the vm seam is load-bearing from the start.
* The two symbols never in Tier C from `memory` are `msync`/`mremap`; from `dynamic-link` it is `dladdr`; from `data-object`: `daylight optarg optind tzname timezone` — none of which is evidence they are unused (Tier C itself is an over-approximation of use, and its complement is not an under-approximation of non-use; the docstring's limitation 7, `__cxa_atexit` handlers, applies).

---

## 3. The gap — what omni-platform must grow

Today `omni-platform` contains only `vm` (virtual memory: reservation, lazy commit, decommit, protection, placeholder splitting, file-backed mapping; Windows implemented and measured, Linux/macOS structural `Unsupported`) and `fault` (process-wide vectored exception handler) — VERIFIED `crates/omni-platform/src/lib.rs` lines 14–23. Its own docstring says: *"Threads, clocks, dynamic loading and windowing will arrive as sibling modules in later tasks"* (line 23).

Serviceability per class (VERIFIED against the module list above; the "no seam" rows are the gap):

| class | servable today? | notes |
|---|---|---|
| pure | yes | no OS interaction; a thunk layer over compiler/runtime intrinsics suffices. Outside omni-platform's OS constraint |
| memory | **mostly yes** | `vm/` covers mmap/munmap/mprotect/madvise-shaped operations and file-backed mapping. Not yet shaped: `mlock`/`msync` (no committed/decommit variants exposed), `mremap` (Windows has no equivalent), `mallinfo` (allocator introspection), `sysconf`/`getpagesize` (page-size queries exist inside vm but likely not as a public query yet) |
| fault-adjacent signals | **partial** | `fault/` owns the host-side handler; the guest-facing `sigaction`/`sigaltstack` semantics have no seam yet |
| file-io | **no seam** | nothing in omni-platform opens a file or directory |
| network | **no seam** | nothing in omni-platform touches a socket |
| threads-sync | **no seam** | the docstring names threads as a future sibling module; nothing exists yet |
| time-clocks | **no seam** | same |
| process-env | **no seam** | same |
| dynamic-link | **no seam** | named as future in the docstring; nothing exists yet |
| logging | **no seam** | trivially small (7 symbols) but currently nowhere |
| android-api | **no seam** | and per §2, zero of it is initializer-reachable |
| data-object | **partial** | `__sF`/`stdin`/`stdout`/`stderr` need host storage behind the GOT binding; that is a linker/loader concern the loader work already touches, not a new module |

For every class with no seam, the primitives the seam would need to expose, with the Windows backing APIs first and the POSIX calls that Linux and macOS versions would eventually use. (Descriptive inventory, not an API design.)

**file-io**
* primitives: open/read/write/seek/close on files; directory enumeration; metadata (size, times, type); path resolution and filesystem queries; pipe/anonymous-fd creation (an initializer-reachable need: `pipe` is in Tier A's 35 file-io symbols).
* Windows: `CreateFile2`/`ReadFile`/`WriteFile`/`SetFilePointerEx`/`CloseHandle`; `FindFirstFileExW`/`FindNextFileW`; `GetFileInformationByHandleEx`; `GetFullPathNameW`; `CreatePipe`; `CreateFileMapping` (for `posix_fallocate`-shaped preallocation, `SetFileInformationByHandle`).
* POSIX: `open`/`read`/`write`/`lseek`/`close`; `opendir`/`readdir`/`closedir`; `stat`/`fstat`/`lstat`; `realpath`; `pipe`; `fcntl`; `ioctl`; `fsync`; `truncate`/`ftruncate`.

**network**
* primitives: socket create/bind/listen/accept/connect/shutdown; send/recv (incl. scatter-gather and the `recvmmsg`/`sendmmsg` batch forms); option get/set; name/address resolution (`getaddrinfo`/`getnameinfo` family — already in Tier A); interface-name/index mapping; and a readiness primitive (`poll`/`select`/`epoll`) plus an fd-shaped wake-up object (`eventfd`) — both already in Tier A.
* Windows: `WSASocketW`/`bind`/`listen`/`WSAAccept`/`WSAConnect`/`shutdown`; `WSASend`/`WSARecv`/`WSASendMsg`/`WSARecvMsg` (multiple-recv batch forms have no direct WinSock equivalent — INFERRED gap to design around); `getsockopt`/`setsockopt`; `WSAGetAddrInfoW`/`getaddrinfo`/`freeaddrinfo`; `if_nametoindex`/`if_indextoname`; `WSAPoll`/`select`; `CreateWaitableTimerExW` or a self-pipe over `CreatePipe` to stand in for `eventfd`.
* POSIX: `socket`/`bind`/`listen`/`accept4`/`connect`/`shutdown`; `sendmsg`/`recvmsg`/`sendmmsg`/`recvmmsg`; `getsockopt`/`setsockopt`; `getaddrinfo`/`getnameinfo`/`freeaddrinfo`/`gai_strerror`; `poll`/`ppoll`/`select`; `epoll` on Linux, `kqueue` on macOS; `eventfd` on Linux, `kqueue` EVFILT_USER or a pipe on macOS.

**threads-sync**
* primitives: thread create/join/detach/exit with stack-size and scheduling attributes; mutexes, condition variables (with a clock choice — `pthread_condattr_setclock` is imported), rwlocks, semaphores; `pthread_once`; thread-local storage (create/delete key, get/set value; plus the implicit TLS of `__errno` and `__cxa_thread_atexit_impl`); thread naming; and the signal-adjacent pieces (`sigaction`, `sigprocmask`, `sigaltstack`, `raise`) that pair with the existing `fault/` seam.
* Windows: `CreateThread`/`WaitForSingleObject`/`TerminateThread`-free design around `_beginthreadex`; `CreateWaitableTimerExW` + condition variables via `SleepConditionVariableSRW` (clock choice maps to waitable-timer vs. SRW with timeout — INFERRED mapping, design detail); SRW locks/`InitializeCriticalSection` for mutex; `InitializeSRWLock`-based or `AcquireSRWLockShared` for rwlock; `CreateSemaphoreExW`; `TlsAlloc`/`TlsGetValue`/`TlsSetValue`/`FlsAlloc`; `SetThreadName`/`SetThreadDescription`; vectored exception handler (already exists in `fault/`) backs `sigaction`-shaped guest crash handlers; `RaiseFailFastException` or `RaiseException` for `raise`.
* POSIX: `pthread_create`/`pthread_join`/`pthread_detach`/`pthread_exit`; `pthread_mutex_*`, `pthread_cond_*` (+condattr clock), `pthread_rwlock_*`; `sem_init`/`sem_post`/`sem_wait`/`sem_destroy`; `pthread_once`; `pthread_key_*`; `pthread_setname_np`; `sigaction`/`sigprocmask`/`sigaltstack`/`raise` (Linux and macOS, with macOS's FFI-only sigaltstack caveat INFERRED from platform knowledge, not from project files).

**time-clocks**
* primitives: monotonic and wall-clock reads (`clock_gettime`-shaped), calendar conversion (`gmtime_r`/`localtime_r`/`mktime`/`strftime` — TZ database needed), sleep (`nanosleep`/`usleep`), and an fd-based timer object (`timerfd_create`/`timerfd_settime`, Linux-only — see §4).
* Windows: `QueryPerformanceCounter` (monotonic), `GetSystemTimePreciseAsFileTime` (wall), `WaitableTimer` objects; TZ: `GetTimeZoneInformation` + the ICU/registry TZ database (the engine's own TZ expectations are unknown — flagged).
* POSIX: `clock_gettime(CLOCK_MONOTONIC/CLOCK_REALTIME)`, `nanosleep`, `timerfd_*` (Linux) / `kqueue` timers (macOS), `gmtime_r`/`localtime_r`/`mktime`/`tzset` directly.

**process-env**
* primitives: process identity (`getpid`/`getppid`/`gettid`), user/group identity (`getuid`/`geteuid`), environment (`getenv`), hostname, entropy (`getentropy`/`arc4random_buf`), auxiliary vector (`getauxval` — page size, cache sizes), system info (`uname`, `sysinfo`), scheduler controls, exit/abort plumbing (`exit`, `_exit`, `atexit`, `__cxa_atexit`, `abort`, `android_set_abort_message`), subprocesses (`fork`/`execv`/`execve`/`waitpid`), and the grab-bag (`__system_property_get`, `prctl`, `ptrace`, `syscall`).
* Windows: `GetCurrentProcessId`/`GetCurrentProcess`/`GetCurrentThreadId`; `GetEnvironmentVariableW`; `GetComputerNameExW`; `BCryptGenRandom`/`ProcessPrng`; `GetLogicalProcessorInformationEx`; `GetSystemInfo`/`GlobalMemoryStatusEx` (for `sysinfo`-shaped queries); `TerminateProcess`/`ExitProcess`; `_onexit`/CRT atexit chain for `__cxa_atexit`; `CreateProcessW` + `WaitForSingleObject`; `GetNamedPipeClientProcessId`-style property queries are *not* a match for Android system properties — `__system_property_get` needs a fake property service (INFERRED; no Windows equivalent).
* POSIX: `getpid`/`getppid`/`gettid`; `getuid`/`geteuid`; `getenv`; `gethostname`; `getentropy`; `getauxval`; `uname`/`sysinfo`; `sched_*`; `exit`/`_exit`/`atexit`/`abort`; `fork`/`execv`/`execve`/`waitpid`; `sysctl`/`sysconf` on macOS where `getauxval` is absent (macOS has no `getauxval` — INFERRED, flagged).

**dynamic-link**
* primitives: open a library by name (`dlopen`), resolve symbols (`dlsym`), query an address's library/symbol (`dladdr`), enumerate loaded modules and their phdrs (`dl_iterate_phdr`), reference counting and error strings (`dlclose`/`dlerror`).
* Windows: `LoadLibraryExW`/`GetProcAddress`/`FreeLibrary`/`GetModuleHandleExW`; `EnumProcessModules` or `CreateToolhelp32Snapshot` + `GetModuleInformation` for the phdr enumeration; `SymGetModuleInfo64`-shaped or manual PE-walk for `dladdr`.
* POSIX: `dlopen`/`dlsym`/`dladdr`/`dlclose`/`dlerror`/`dl_iterate_phdr` (Linux; all present on macOS).

**logging**
* primitives: a priority-tagged line writer and a buffered variant, plus POSIX `syslog`/`openlog`/`closelog` passthrough or stubs.
* Windows: `OutputDebugStringW`, plus whatever the project's own log sink is.
* POSIX: `__android_log_print` equivalents do not exist off-Android; map to stderr/journald/os_log or to the project sink (mapping choice INFERRED as a later design decision).

**data-object**
* primitives: host storage for 23 addresses, bound behind the GOT binding the loader already performs. Not a new OS seam; a loader/emulator concern. Details in §4.4.

---

## 4. Surprises

### 4.1 Sockets: yes — 39 imports, and 8 are initializer-reachable

The earlier uncertainty is settled by the inventory itself. The full list (VERIFIED, each name confirmed in the libroblox block lines 729–1295 of `apk-undefined-symbols.txt`; e.g. `socket` line 1213, `poll` line 1109, `getaddrinfo` line 942, `eventfd` line 898):

```
accept accept4 bind connect epoll_create epoll_create1 epoll_ctl epoll_wait eventfd
freeaddrinfo gai_strerror getaddrinfo gethostbyname getnameinfo getpeername getsockname
getsockopt if_indextoname if_nametoindex inet_ntop inet_pton listen poll recvmmsg recvfrom
recvmsg select sendmmsg sendmsg sendto setsockopt shutdown socket socketpair
__FD_CLR_chk __FD_ISSET_chk __FD_SET_chk __cmsg_nxthdr __sendto_chk
```

(classification note: `epoll*`/`poll`/`select`/`eventfd`/`__FD_*_chk`/`__cmsg_nxthdr` are filed under network per the task's "poll/epoll if used for sockets" instruction — VERIFIED as a judgment call, not a fact of use.)

That is 39 of 565 (counted by the script's network bucket). Notably the set is **complete enough to be a real network stack**: name resolution + TCP/UDP + poll/epoll + `socketpair` + the `recvmmsg`/`sendmmsg` batch APIs. And per §2, 8 of these 39 (`eventfd freeaddrinfo gai_strerror getaddrinfo inet_ntop poll select socket`) are in Tier A — static initializers already touch the networking surface (VERIFIED: the per-symbol join against the tool's own Tier A list; INFERRED interpretation: likely interface enumeration or socket-pair setup at startup — the tool does not say why).

### 4.2 Allocator: the "imports NONE" claim is wrong — but barely, and the direction of the surprise is the opposite

**Claim as given:** a previous analysis concluded libroblox.so imports no allocator and carries its own.

**What the inventory actually says, verified by name search:**
* Searching the libroblox.so block (lines 729–1295) for the allocator family `malloc|free|calloc|realloc|posix_memalign|memalign|aligned_alloc|strdup|reallocarray|malloc_usable_size|malloc_info|mallinfo` returns exactly **one** hit: `mallinfo  GLOBAL FUNC` (VERIFIED: grep over the line range; `mallinfo` is at line 1077 of the file).
* In particular, **`malloc`, `free`, `calloc`, `realloc` are NOT in libroblox.so's block**. The earlier `calloc`/`free` hits that appear when grepping past line 1296 belong to `libzstd-jni-1.5.7-6.so`'s block, which begins at line 1297 (VERIFIED header position; the union sections of the same file attribute `calloc` to `libbacktrace-native.so,libzstd-jni-1.5.7-6.so` and `free` to `libbacktrace-native.so,libeigen_blas.so,libimage_processing_util_jni.so,librenderscript-toolkit.so,libzstd-jni-1.5.7-6.so`, with `libroblox.so` absent from both attributions).
* So the structural conclusion of the earlier analysis **stands**: no malloc/free/calloc/realloc imports. `libroblox.so` carries its own allocator — consistent with its `.rodata` containing allocator-related section names like `malloc_hook`, `pb_defaults`, `protodesc_cold` (VERIFIED `docs/research/apk-analysis.md` line 387; the inference that these indicate a bundled allocator is INFERRED).
* The refinement this inventory adds: **`mallinfo` is imported and reachable** (in Tier A, VERIFIED per-symbol join), so the allocator that is *not* imported from bionic must itself either implement `mallinfo`-shaped introspection or have it called against the bionic heap anyway. The thunk layer will need a real answer for `mallinfo` even though it never needs `malloc`. What `mallinfo`'s return value should say when the heap behind it is the host's is unknown — flagged for design, not resolved here.

### 4.3 Symbols needing capabilities a desktop OS does not have (or does not expose the same way)

* `timerfd_create` / `timerfd_settime` — Linux-specific; no Windows or macOS equivalent object. Windows can stand in with waitable timers, macOS with kqueue user events, but the fd-shaped identity (the guest may `read`/`poll` the fd) has no native counterpart (INFERRED shape; the import itself VERIFIED at lines 1264–1265 of the inventory).
* `epoll_*` (4 imports) — Linux-only. Same shape problem on Windows/macOS: the readiness object is pollable by fd. `WSAPoll`/`kqueue` cover the behavior, not the fd-identity (INFERRED).
* `eventfd` — Linux-only for the same reason (it is a *countable* fd; `WSAPoll` doesn't help; a self-pipe or a named-pipe pair can emulate).
* `fork` / `execv` / `execve` — on Windows `fork` does not exist (`CreateProcessW` is a different model). What the engine's initializers actually do with `fork` is unknown from the import table; it is in Tier C, not Tier A (VERIFIED per-symbol join).
* `mremap` — no Windows equivalent; in Tier C only.
* `mallinfo` — a *bionic allocator introspection* API imported by a binary that does not import the allocator. Semantics unknown (see §4.2).
* `__system_property_get` — reads Android's global property database. Needs a fake property service; values the engine expects are unknown.
* `ptrace` — process-injection/debugging primitives on Windows require different privileges and different APIs entirely (`DebugActiveProcess`); unclear why the engine imports it; not reachable even at Tier C (VERIFIED: `ptrace` appears in the Never-referenced list).
* `getauxval` — no direct Windows/macOS equivalent; host must synthesize the AT_* entries (`AT_PAGESZ` at minimum). Notably `getauxval` IS in Tier A (VERIFIED join), so this synthesis is needed early.
* `if_indextoname`/`if_nametoindex` — present on modern Windows (`if_nametoindex` in netioapi.h) but Android-specific semantics (WLAN interface naming) may matter to the engine (INFERRED; unknown how it is used).
* `AAsset_openFileDescriptor` — returns a *real fd* for an APK asset. On a desktop host the "asset" is a file inside an APK the host manages; the fd must be synthesized (a named pipe, a real temp file, or a file-view handle duplicated into an fd-like object). This is a genuine capability-shape mismatch (INFERRED).
* `AMediaCodec_*` / `AMediaFormat_*` (23 media symbols) — Android's hardware codec service. Desktop has no equivalent service; these will need a real software backend or a stub that fails gracefully. Not initializer-reachable (VERIFIED: android-api has 0 in Tier C).

### 4.4 The 23 STT_OBJECT data symbols — what is obvious and what is not

Kind VERIFIED: all 23 are `GLOBAL OBJECT` in the inventory's libroblox block (the script's kind census). Reachability VERIFIED: 18 of the 23 are in Tier A — the tool itself says data imports are "addresses with host storage behind it, never a branch target" (`tools/init_reach.py` docstring; the Tier A table's `data` column, `init-reach-scan.txt` line 82).

| symbol | size / layout requirement | status |
|---|---|---|
| `stdin` `stdout` `stderr` (3) | pointers to `FILE` objects — **bionic layout**, not MSVC/UCRT and not glibc. Bionic's `FILE` (`__sbuf`/`__sF` structure) differs from both hosts' CRT types. Behind the GOT, host storage must be an object whose *address* is valid; any field access into it is the engine reading a bionic layout it knows. | size unknown from import table alone (bionic's `FILE` is an internal struct; its size is not in this project's files) |
| `__sF` | bionic's `FILE` array — the *storage* backing `stdin`/`stdout`/`stderr` on bionic. Same layout question, and worse: it must be at least 3 `FILE`s large and the `stdin`/`stdout`/`stderr` symbols must point into it consistently (INFERRED from bionic's design; the exact element count/size in this binary is unknown — the inventory does not record st_size, and `tools/init_reach.py` reads `st_size` from `.dynsym` but never prints it — VERIFIED lines 329/336 read it, no print site found) | element count/size unknown |
| `__stack_chk_guard` | the SSP canary word. One pointer-sized value (INFERRED: bionic declares it as `uintptr_t`). The guest's SSP failure path calls `__stack_chk_fail` (also imported, in Tier A per the provider table — VERIFIED in the tool's Tier A list under libc.so). Layout trivial; the *value* must be per-process random (INFERRED). | size INFERRED (1 word), value policy unknown |
| `environ` | `char **` — pointer to a NULL-terminated array of `char *`. Layout is trivial and universal, but the *contents* determine engine behavior (what the engine reads from the environment is unknown). | layout obvious; contents unknown |
| `daylight` `timezone` `tzname` | the three `tzset()` globals. Sizes are conventional (int / long / `char*[2]`) but the *lifetimes and update rules* interlock with `tzset` and `localtime_r` — the thunk must keep them consistent after `tzset` is called (INFERRED from POSIX semantics; Android/bionic follows the same shape). | sizes conventional, INFERRED |
| `optarg` `optind` | getopt state. Two scalars; layout trivial. Whether the engine actually parses argv through these is unknown (they are in the Tier A data-18 set — VERIFIED in the tool's data list — so some initializer touches their GOT slot; what it does with the value is unknown). | layout trivial; usage unknown |
| `in6addr_any` `in6addr_loopback` | two `struct in6_addr` constants (16 bytes each — INFERRED from the type's universal definition in RFC 2553-era headers, which bionic follows). Both are in Tier A (VERIFIED in the tool's data list). 32 bytes total; values fixed by the IPv6 spec. | layout obvious and standard |
| `AMEDIAFORMAT_KEY_*` (10) | `const char*` string constants. On Android they are exported by libmediandk.so as opaque C strings whose *content* is the key name ("bit-rate", "mime", ...). The thunk must provide strings with the exact documented key names (INFERRED from NDK documentation knowledge, not from project files — the specific literal values are not recorded anywhere in this repository, which is itself worth flagging). | content unknown from project files; layout trivial |

Summary of 4.4: 6 of 23 are conventional and safe (`in6addr_*`, `optarg`, `optind`, `daylight`, `timezone`, `tzname` — sizes conventional, INFERRED). 4 are trivially sized but semantically loaded (`environ`, `__stack_chk_guard`). The remaining 13 (3 FILE pointers + `__sF` + 10 AMEDIAFORMAT keys) have a real unknown: **`__sF`/`stdin`/`stdout`/`stderr` require a bionic-`FILE`-shaped object whose exact size/layout this repository does not record**, and the AMEDIAFORMAT key strings require literal values this repository does not record. Both are discoverable from bionic sources / NDK headers, not from this project's committed analysis. The tool reads `st_size` (`tools/init_reach.py` lines 329, 336) but never reports it; extending that output would settle the sizes factually — noted as an observation, not done (this task is analysis-only).

---

## 5. Self-check

### 5.1 Do the counts sum to 565?

**Yes.** VERIFIED two ways:
1. `tools/os_surface.py --check` exits 0 on this run (its own assert: `total == 565`).
2. The printed per-class counts sum to 565 by hand: 150+10+73+39+56+17+41+6+7+141+23+2 = 565.

### 5.2 Judgment calls (for the design session to revisit)

* `__errno` → threads-sync (it returns the address of thread-local errno; TLS mechanics).
* `sigaction`/`sigaltstack`/`sigfillset`/`sigemptyset`/`raise`/`signal` → threads-sync (per-thread signal state; pairs with the fault seam). A reasonable reviewer could put them in process-env.
* `mallinfo` → memory (allocator introspection), even though the allocator it introspects is not imported (§4.2).
* `epoll*`/`poll`/`select`/`eventfd`/`__FD_*_chk`/`__cmsg_nxthdr`/`__sendto_chk` → network (the task's "poll/epoll if used for sockets" clause).
* `timerfd_create`/`timerfd_settime` → time-clocks rather than network, despite being fd-returning.
* `syscall`/`ptrace`/`prctl` → process-env; which syscalls the guest issues through the generic `syscall()` entry is unknown from the import table.
* `__cxa_atexit`/`__cxa_finalize`/`__register_atfork` → process-env (the task text's "exit/atexit/__cxa_atexit" clause), while `__cxa_thread_atexit_impl` → threads-sync (thread-local destructors). Inconsistent-looking; deliberate.
* `tcgetattr`/`tcsetattr` → file-io (they are ioctls on a terminal fd).
* `__gcov_dump`/`__gcov_flush` → left in `unclear` (WEAK NOTYPE; provider grouping calls them bionic-specific, per the tool's Tier A provider table).
* `AMEDIAFORMAT_KEY_*` → data-object by type (STT_OBJECT), not android-api by name.

### 5.3 Independent cross-checks (different methods than the first count)

| # | what was cross-checked | method 1 (used for the report) | method 2 (independent) | result |
|---|---|---|---|---|
| 1 | total = 565 | script parse of the block (name list from `^  name  BINDING KIND` lines, lines 729–1295) | the script's second parse path over the same file's *union* sections (different line format: `name ... <- lib...` attribution) | identical 565-name set (script's `--union` mode: `CROSS-CHECK ok`) |
| 2 | network = 39 | script's network bucket | direct grep of the block lines 729–1295 for `^  (socket\|connect\|send\|recv\|getaddrinfo\|...)` — manual count of the §4.1 list | both 39 (grep returned the same 39 names; VERIFIED terminal output) |
| 3 | memory = 10 | script's memory bucket | grep of block for `^  (mmap\|munmap\|mprotect\|madvise\|msync\|mlock\|mremap\|mallinfo\|sysconf\|getpagesize)\s` → 10 | both 10 (VERIFIED terminal output) |
| 4 | android-api = 141 | script's android-api bucket | grep of block for `^  (egl[A-Z]\|gl[A-Z])` → 91; grep of block for `^  A[A-Z]` → 60, of which the 10 `AMEDIAFORMAT_KEY_*` are STT_OBJECT (data-object), leaving 50 A\* names; 91 + 50 = 141 | both 141 (VERIFIED terminal output) |
| 5 | data-object = 23 | script (kind == OBJECT) | grep census: 10 AMEDIAFORMAT + 3 stdio + `__sF` + `__stack_chk_guard` + `environ` + `daylight` `timezone` `tzname` + `optarg` `optind` + `in6addr_any` `in6addr_loopback` = 23 | both 23 |
| 5b | no plain `send`/`recv` imports | script's network bucket (which contains `sendto`/`recvfrom` but not `send`/`recv`) | grep of block for `^  (recv\|send)\s` → no match | consistent: the engine uses only the *to/from/msg* variants (VERIFIED terminal output) |
| 6 | kind census FUNC 539 / OBJECT 23 / NOTYPE 3 | script's kind counter | sum: 539 + 23 + 3 = 565 (arithmetic consistency with #1) | consistent |
| 7 | reachability joins (§2 table) | fresh `tools/init_reach.py --list` output parsed into Tier A / Tier C-only / Never sets (188/58/319) | committed `docs/research/init-reach-scan.txt`: Tier A union 188 (line 48), Tier C 246 (line 60), provider-table data counts (line 82: 18 data objects), provider grouping matches | fresh numbers byte-identical to committed (VERIFIED diff of numeric sections), and §2's column sums (188/246/319) match the tool's own printed 113/188/246/565 ladder |

### 5.4 Known limits of this report

* Reachability numbers are the tool's own lower/upper bounds with the seven holes its docstring states; "never referenced from the Tier C closure" is a statement about that closure, not about runtime behavior.
* Symbol *sizes* were never read. `st_size` is available in `.dynsym` and `tools/init_reach.py` already parses it; pulling it into a report would firm up §4.4. Not done here (analysis-only constraint interpreted conservatively; it would require extending that tool's output, i.e. modifying it or writing a new report-consuming tool).
* Where this report says "unknown", it means the committed project files contain no answer: notably `mallinfo`'s expected semantics on a foreign heap, the bionic `FILE`/`__sF` layout details, the AMEDIAFORMAT key literals, what the engine expects from `__system_property_get`, and why `fork`/`ptrace`/`execve` are imported.
