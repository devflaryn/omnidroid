//! **The bionic adapter, driven by real translated ARM64 code.**
//!
//! Every test here writes A64 instructions, lets the translating backend run them, and asserts on
//! the *value* the guest got back. A test that only checked `run` returned `Ok` would pass against
//! a handler that returned zero for everything, which is precisely the failure Global Constraint 1
//! is about.
//!
//! The hostile cases are in the second half of the file, and they are not an afterthought: four of
//! five foundation tasks in this project shipped a hostile-input defect their own passing suites
//! could not see, and every function bound here takes a guest pointer or a guest length.
//!
//! ```text
//! cargo test -p omni-android --test bionic --release
//! ```

#![cfg(target_arch = "x86_64")]

mod harness;

use std::sync::Arc;

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::{Bionic, ThreadHost};
use omni_android::{AbiError, Boundary};
use omni_cpu::{ExitReason, GuestCpu};

/// A guest, a bionic instance over its address space, and the boundary with every handler bound.
struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    boundary: Arc<Boundary>,
}

fn fixture() -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(256);
    bionic.bind_into(&builder).expect("bind every handler");
    // The log sink writes to the host's stderr by default, which is what a real run wants and what
    // a suite that logs 266 lines on purpose does not. The instance's ring still records every
    // line, and the ring is what these tests assert on.
    bionic.set_log_to_stderr(false);
    let boundary = builder.finish();
    Fixture { guest, bionic, boundary }
}

impl Fixture {
    /// The thunk address a symbol was given.
    fn thunk(&self, symbol: &str) -> omni_cpu::GuestAddr {
        self.boundary
            .slot_named(symbol)
            .unwrap_or_else(|| panic!("`{symbol}` is not bound"))
            .address
    }

    /// Run `entry` with this instance published to the calling thread.
    fn run(&self, cpu: &mut dyn GuestCpu, entry: omni_cpu::GuestAddr) -> Result<ExitReason, AbiError> {
        let _active = self.bionic.activate().expect("a thread block");
        self.boundary.run(cpu, entry, BUDGET)
    }

    /// Put a NUL-terminated C string at `at` and return `at`.
    fn cstring(&self, at: omni_cpu::GuestAddr, text: &[u8]) -> omni_cpu::GuestAddr {
        let mut bytes = text.to_vec();
        bytes.push(0);
        self.guest.write_bytes(at, &bytes);
        at
    }

    /// Read a NUL-terminated C string back out of guest memory.
    fn read_cstring(&self, at: omni_cpu::GuestAddr) -> Vec<u8> {
        let mut out = Vec::new();
        for offset in 0..4096 {
            let byte = (self.guest.read_u64((at + offset) & !7) >> (8 * ((at + offset) & 7))) as u8;
            if byte == 0 {
                break;
            }
            out.push(byte);
        }
        out
    }
}

/// A program that loads `setup`, calls `symbol`, stores `X0` at `data + 0`, and returns.
fn call_one(f: &Fixture, symbol: &str, setup: impl FnOnce(&mut Asm)) -> omni_cpu::GuestAddr {
    let thunk = f.thunk(symbol);
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    setup(&mut asm);
    asm.bl(thunk);
    asm.mov(22, f.guest.data as u64);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    f.guest.load(asm.words());
    entry
}

/// Run a one-call program and return what the guest stored from `X0`.
fn value_of(f: &Fixture, symbol: &str, setup: impl FnOnce(&mut Asm)) -> u64 {
    let entry = call_one(f, symbol, setup);
    let mut cpu = f.guest.thread(&f.boundary);
    let exit = f.run(&mut cpu, entry).expect("the run must complete");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    f.guest.read_u64(f.guest.data)
}

/// Run a one-call program that is expected to fail, and return the refusal.
fn refusal_of(f: &Fixture, symbol: &str, setup: impl FnOnce(&mut Asm)) -> AbiError {
    let entry = call_one(f, symbol, setup);
    let mut cpu = f.guest.thread(&f.boundary);
    match f.run(&mut cpu, entry) {
        Err(error) => error,
        Ok(exit) => panic!("`{symbol}` completed with {exit:?} where a refusal was required"),
    }
}

// =================================================================== the tables

/// The symbols bound here that Task 1's 188 does **not** contain, and the evidence for each.
///
/// It began as eight and the count is deliberately not restated in this sentence: the table's own
/// length is the number, and `the_bound_count_is_exactly_what_this_phase_claims` is what pins it.
///
/// # Task 1's prediction was a lower bound, and M3's gate is what measured by how much
///
/// D17 says 188 is a lower bound and says why: 17,698 indirect call sites the static closure could
/// not follow, and a 2,670,684-byte region with no unwind info hiding one initializer entry point
/// worth 67 of the 188. Running the 3,594 initializers is the first thing that could test that,
/// and it found symbols the prediction had placed in two *different* sections of its own file —
/// and, for two of them, in the section that says the initializers never reach them at all.
///
/// This list is what keeps the specification discipline the 188 used to provide. A bound symbol
/// must be an import of `libroblox.so`, and one outside the 188 must be named here with how it was
/// found — so a typo still fails, and scope creep is a visible diff rather than a silent binding.
const BEYOND_THE_PREDICTION: &[(&str, &str)] = &[
    (
        "uselocale",
        "M3 gate, called at init_array[2]. The file's Tier C section: reached, said the scan, \
         only through an address-taken edge",
    ),
    (
        "__ctype_get_mb_cur_max",
        "M3 gate, called at init_array[2]. The file's LAST section: never referenced from the \
         Tier C closure at all",
    ),
    (
        "mbtowc",
        "M3 gate, called at init_array[2], one direct call site at 0x2b772f4 passing n = 4. The \
         file's LAST section, as `__ctype_get_mb_cur_max`",
    ),
    (
        "freelocale",
        "NOT called by the initializers, and bound because `newlocale` is: a layer that hands out \
         a locale handle and refuses to take it back is worse than one that does neither. The \
         file's Tier C section",
    ),
    (
        "strerror_r",
        "M3 gate, reached at init_array[3118] on libc++'s verbose-abort path, before \
         /dev/urandom existed; not called once that path is gone. The POSIX spelling of a symbol \
         whose GNU spelling IS in the 188, and the two differ in what they return",
    ),
    (
        "strnlen",
        "M4 gate, called from JNI_OnLoad's registration helpers. Nothing before step 6 reached it.",
    ),
    (
        "gmtime",
        "M4 gate, called by nativeInitFastLog. gmtime_r IS in the 188; this spelling is not, and the two differ in who owns the struct tm.",
    ),
    (
        "getcwd",
        "M4 gate, called three times by nativeSetAssetPath while the engine canonicalises the asset directory.",
    ),
    (
        "pipe",
        "M5, read out of the binary rather than out of a run: jni-surface.md §5.2 decodes \
         initializeNativeCode at 0x0285b750 and finds pipe() twice -- once for the \
         msgread/msgwrite pair the ALooper watches, once in GameActivity_onCreate for the glue's \
         own command pipe. The file's LAST section says the initializers never reach it.",
    ),
    (
        "fcntl",
        "M5, from the same decoding: fcntl(F_SETFL, O_NONBLOCK) on both ends of both pipes. The \
         file's LAST section, as `pipe`.",
    ),
    (
        "strftime",
        "M4's gate, called by nativeInitFastLog -- one of the two scripted downcalls of §8 step 9          that did not return, and the reason was that there was no implementation to bind. The          file's LAST section; `strftime_l` is there too and is not reached. Bound in M5 once          `omni_bionic::time::strftime` existed.",
    ),
    (
        "pthread_attr_setdetachstate",
        "M5's gate, called by GameActivity_onCreate step 3 to make the game thread detached          before pthread_create. The file's LAST section -- the initializers never reach it. A          PURE BINDING GAP: omni_bionic::metadata::attr_setdetachstate has existed since phase          3c and nothing called it, and pthread_create already reads the detach state out of the          attribute object, so without this the glue's game thread would have been created          joinable and nothing joins it.",
    ),
    (
        "write",
        "M5. The file's Tier C section -- reached only through an address-taken edge -- so it is \
         outside the 188 although `read` and `__write_chk` are inside them. \
         android_native_app_glue writes one APP_CMD byte into the pipe per message, and the \
         engine carries that code's own diagnostic \"Failure writing android_app cmd: %s\" in \
         .rodata.",
    ),
    (
        "pthread_cond_timedwait",
        "M6 gate, found the way an Unbound is meant to be found: driving the row 19 \
         lifecycle native stopped inside `onStartNative` naming this symbol. Decoded \
         afterwards at guest 0x0285f7ec -- the GameActivity glue \
         `android_app_set_activity_state`, which writes the APP_CMD byte and then waits for \
         the game thread to acknowledge it, on an absolute CLOCK_REALTIME deadline two \
         seconds out. Its `cmp w0, #0x6e` is the guest own agreement with omni_bionic \
         ETIMEDOUT. jni-surface.md section 8 row 19.",
    ),
    (
        "sched_get_priority_max",
        "M6 gate, as an Unbound that killed a guest thread during the settle after the row \
         17-20 lifecycle natives. Two call sites, both passing SCHED_FIFO: 0x022077e8 and \
         0x054e026c.",
    ),
    (
        "sched_get_priority_min",
        "M6, by DECODING rather than by a run reaching it -- `pipe`, `fcntl` and `write` \
         provenance, and it is stated because the distinction matters. The site at \
         0x054e0260 calls it two instructions before the `sched_get_priority_max` at \
         0x054e026c, with no branch between, and then requires `max - min >= 3` before it \
         will use the band at all. The site that actually fired was the other one \
         (0x022077e8) -- this one would have refused at _min first -- so this symbol has not \
         yet been reached by a run: what is claimed here is a decoded call site, not an \
         observation.",
    ),
    (
        "sched_setscheduler",
        "M6, by decoding. It is three instructions after the `sched_get_priority_max` at \
         0x022077e8, in the same basic block with no branch between, so the run that reached \
         that one reaches this one next. It answers -1/EPERM, which is what a device answers \
         an app without CAP_SYS_NICE, and the call site ignores the result.",
    ),
    (
        "strftime_l",
        "M6, found the way an Unbound is meant to be found: \
         `nativePostClientSettingsLoadedInitialization3` stopped on it at guest 0x06258e44, \
         one call past the point where the client settings have been parsed and the engine \
         starts reporting its own build. The file LAST section names it beside `strftime` \
         as there too and not reached -- it is reached now, one row later than that note. \
         Bionic has one locale implementation, so its own `strftime_l` forwards to \
         `strftime`, and so does this one.",
    ),
    (
        "pthread_getattr_np",
        "M6's startup run, found by a guest WORKER THREAD dying on it rather than by a \
         downcall stopping: GuestThreadFailure { thread: 7, start_routine: 0x27798fa8db0, \
         why: \"the guest called the imported symbol `pthread_getattr_np` through its thunk \
         at 0x277928d3e20, and nothing in the compatibility layer implements it\" }. The BL \
         is at image offset 0x2173df8 and 0x2173dfc is the return address. The file's Tier C \
         section -- reached solely through an address-taken edge -- which is why it is \
         outside the 188 although `pthread_attr_init` and `pthread_attr_setstacksize` are \
         inside them. It is the first member of the attr family that reports a LIVE thread, \
         so it is the first that could not be bound by binding an `omni-bionic` function: \
         the stack base and size it answers with had to start being recorded when \
         `pthread_create` maps a stack.",
    ),
    (
        "pthread_mutex_trylock",
        "M6's startup run, and the SECOND guest worker thread to die on an Unbound in it: \
         GuestThreadFailure { thread: 3, start_routine: 0x284d168 (image-relative), \
         why: \"the guest called the imported symbol `pthread_mutex_trylock` through its \
         thunk at 0x1f7dad82f10, and nothing in the compatibility layer implements it\" }. \
         The BL is at image offset 0x2b53aa0 and 0x2b53aa4 is the return address, inside a \
         libc++ `std::mutex::try_lock` wrapper -- so the real callers are every try_lock in \
         the engine and not one site. The file's Tier C section, one line below \
         `pthread_getattr_np` in it. A PURE BINDING GAP of the `pthread_attr_setdetachstate` \
         kind: `omni_bionic::mutex::trylock` has existed since phase 3c with \
         `trylock_held_is_ebusy_all_types` beside it, and nothing called it.",
    ),
    (
        "pthread_attr_getstack",
        "M6's startup run, and the THIRD guest worker thread to die on an Unbound in it -- this          one only after `pthread_getattr_np` was bound, because it is the call that comes next:          GuestThreadFailure { thread: 7, why: \"the guest called the imported symbol          `pthread_attr_getstack` through its thunk at 0x17fe3bf3e30, and nothing in the          compatibility layer implements it\" }. A thread asks for its own live attributes and          then reads the stack back out of them, which is the whole point of having asked.          Another PURE BINDING GAP, and the third in one session:          `omni_bionic::metadata::attr_getstack` has existed since phase 3c and nothing called          it. Hand-written rather than generated because it has two out-parameters.",
    ),
    (
        "strcspn",
        "M6's startup run, and the one of these that mattered most: it was killing a guest          thread in the gate's **default** path, not only under OMNI_M6_ROWS_21_22, and had been          doing so unremarked for as long as that thread has existed. GuestThreadFailure {          thread: 7, start_routine: 0x2217f04 (image-relative), why: \"the guest called the          imported symbol `strcspn` through its thunk at 0x18a0c303cf0, and nothing in the          compatibility layer implements it\" }. Found by the assertion VERIFICATION.md entry 16          produced, on its first run. A PURE BINDING GAP: `omni_bionic::string::strcspn` has          existed since phase 3a. Its sibling `strspn` is the same `span_walk` with one test          flipped and is deliberately still unbound, because nothing has measured the guest          calling it.",
    ),
    (
        "__strcat_chk",
        "M6's startup run, once the client-settings phase got far enough to build URLs:          GuestThreadFailure { thread: 3, why: \"the guest called the imported symbol          `__strcat_chk` through its thunk at 0x20513e53200, and nothing in the compatibility          layer implements it\" }, from image offset 0x22ac22c. The FORTIFY form of `strcat`,          which was bound. Another PURE BINDING GAP -- `omni_bionic::string::strcat_chk` has          existed since phase 3a with the check's own test beside it -- and the fifth in this          session, which is a pattern rather than a coincidence: the primitives were written          against the import list and the wiring was done against what the run had reached.",
    ),
    (
        "inet_pton",
        "M6's startup run, and a guest WORKER THREAD dying on it: GuestThreadFailure { thread: \
         7, start_routine: 0x2217f04 (image-relative), why: \"the guest called the imported \
         symbol `inet_pton` through its thunk at 0x2c6fd3e3270, and nothing in the compatibility \
         layer implements it\" }. The same start routine `strcspn` was found on -- the engine's \
         URL and address handling -- one symbol further along it. The file's LAST section: never \
         referenced from the Tier C closure at all, although `inet_ntop` IS one of the 188 and \
         has been bound since phase 3d. NOT a binding gap, and the first of this session's \
         worker-thread finds that was not: nothing in `omni-bionic` parsed an address, so \
         `omni_bionic::net::inet_pton` is new code -- BIND's `inet_pton4`/`inet_pton6`, which is \
         what bionic ships, with the strictness that distinguishes it from `inet_aton` (no \
         leading zeros, no shorthand, no hex) tested rule by rule.",
    ),
    (
        "setsockopt",
        "M6's network run, found the way an Unbound is meant to be found and then found again one \
         option deeper: with `socket` answering for the first time, the engine's HTTP stack \
         created fd 12 and immediately configured it, and the guest WORKER THREAD carrying the \
         client-settings fetch died -- GuestThreadFailure { thread: 8, why: \"`setsockopt` ... \
         refused this call: the guest called setsockopt(fd 12, option 9 at SOL_SOCKET)\" }, from \
         link 0x6009d94. Option 9 at SOL_SOCKET is SO_KEEPALIVE. The file's LAST section: never \
         referenced from the Tier C closure at all, although `socket`, `getaddrinfo` and \
         `freeaddrinfo` ARE among the 188.",
    ),
    (
        "connect",
        "M6, with the socket group D30 authorises. **NOT yet reached by a run**, and that is \
         stated rather than implied: the client-settings fetch stops one call earlier, on the \
         SO_KEEPALIVE that `omni_platform::net::SocketOption` has no variant for, so nothing has \
         yet observed the connect that follows it. What is claimed here is the rest of one client \
         call sequence bound together -- socket, setsockopt, connect, poll, getsockopt(SO_ERROR), \
         write, read -- because the run stops at the first missing member and each round trip of \
         that discovery loop is three minutes. The file's LAST section.",
    ),
    (
        "getsockopt",
        "M6, with `connect`: it is how a non-blocking connect reports itself. The engine sets \
         O_NONBLOCK, calls connect, gets -1/EINPROGRESS, waits for writability and reads \
         SO_ERROR; binding the first three and not the fourth would leave the guest unable to \
         learn whether its own connection succeeded. NOT yet reached by a run -- see `connect`. \
         The file's LAST section.",
    ),
    (
        "sendto",
        "M6, with the socket group. `libroblox.so` imports NO plain `send` and no plain `recv` at \
         all -- MEASURED from the APK's own undefined-symbol table -- so `sendto` with a null \
         destination IS the guest's `send`, which is how bionic compiles one. NOT yet reached by \
         a run. The file's LAST section.",
    ),
    (
        "recvfrom",
        "M6, with `sendto` and for the same measured reason: there is no `recv` import, so \
         `recvfrom` with a null source address is the guest's `recv`. NOT yet reached by a run. \
         The file's LAST section.",
    ),
    (
        "__sendto_chk",
        "M6, the _FORTIFY_SOURCE form of `sendto`, bound beside it because the compiler chooses \
         between the two and a layer that implements one and not the other fails on whichever the \
         optimiser picked. It adds one check and nothing else -- len against the buffer size the \
         compiler knows -- and that check is refused by name rather than sent, because sending \
         would put whatever follows the buffer on the network. NOT yet reached by a run. The \
         file's LAST section.",
    ),
    (
        "bind",
        "M6, with the socket group. A UDP client binds a local port before it sends, which is the \
         game protocol's path rather than the settings fetch's, so this is the member of the \
         group furthest from what has been measured. It is bound rather than left Unbound because \
         `omni_platform::net::Socket::bind` exists and a bind is the one socket call whose \
         absence stops a datagram socket having a source address at all. NOT yet reached by a \
         run. The file's LAST section.",
    ),
    (
        "shutdown",
        "M6, with the socket group: it is how a client half-closes a TLS connection it is done \
         writing to, and `omni_platform::net::Socket::shutdown` implements it for stream sockets \
         and refuses it by name on a datagram one. NOT yet reached by a run. The file's LAST \
         section.",
    ),
    (
        "strtoull_l",
        "M6's network run, and the SECOND guest worker thread to die in it: GuestThreadFailure { \
         thread: 3, why: \"the guest called the imported symbol `strtoull_l` through its thunk \
         ...\" }, from image offset 0x2b81a44, while the client-settings response was being \
         parsed. NOT a network symbol at all -- it is here because the network is what got the \
         run far enough to reach it. A PURE BINDING GAP of the `pthread_mutex_trylock` kind: \
         `omni_bionic::numerics::strtoull` has existed since phase 1 and bionic has one locale, \
         so its own `strtoull_l` ignores the locale argument exactly as this does. `strftime_l` \
         is the same shape one milestone earlier. The file's LAST section, one line below \
         `strtoll_l`, which is there too and has not been reached.",
    ),
    (
        "srand",
        "M6's network run, on the engine's retry after the client-settings fetch failed: \
         GuestThreadFailure { thread: 6, why: \"the guest called the imported symbol `srand` \
         through its thunk at 0x1ac79524e60\" }, from link 0x22127fc. A PURE BINDING GAP and the \
         SEVENTH of this session, and the one that shows the pattern most sharply: `rand` has \
         been bound since phase 1 and this gate has counted it at 750 calls every run, while the \
         call that SEEDS it sat in `omni_bionic::numerics` with its own test and no caller. Not \
         entropy -- srand chooses where a deterministic sequence starts, which is the opposite of \
         what arc4random_buf, getentropy and the raw getrandom do. The file's LAST section.",
    ),
    (
        "log10",
        "M6's network run, one thread over: GuestThreadFailure { thread: 14, why: \"the guest \
         called the imported symbol `log10` through its thunk at 0x1ac79525500\" }, from link \
         0x2280b0c, on the telemetry path the engine takes once a flag fetch has failed. A PURE \
         BINDING GAP and the EIGHTH -- `omni_bionic::libm::log10` has sat beside the `log` that \
         WAS bound since phase 3a. Its sibling `log10f` is equally written, equally tested and \
         deliberately NOT bound. Section 7 of the file (Tier C only, reached solely through an \
         address-taken edge), which is outside the 188 the first six sections make.",
    ),
    (
        "mktime",
        "M6's network run, one call after the raw getrandom and the first thing that is          unambiguously CERTIFICATE validation: GuestThreadFailure { thread: 6, why: \"the guest          called the imported symbol `mktime` through its thunk at 0x1e172e54ef0\" }, from image          offset 0x2212920, on the thread carrying the client-settings fetch. OpenSSL turns an          X.509 notBefore/notAfter into a struct tm and calls this to compare it with now. NOT a          binding gap: `omni_bionic::time` had `gmtime` and `strftime` and nothing that went the          other way, so this is the second symbol of the session that needed NEW COMPUTATION --          `days_from_civil`, the exact inverse of the `civil_from_days` `gmtime` already used,          plus C 7.29.2.3's in-place normalisation. On this runtime local time IS UTC (no TZ, no          timezone database, no localtime bound or reached), so mktime and timegm coincide and          the handler says what would falsify that. The file's LAST section.",
    ),
    (
        "ioctl",
        "M6's network run, one call after `isspace` and on the same client-settings thread:          GuestThreadFailure { thread: 8, why: \"the guest called the imported symbol `ioctl`          through its thunk at 0x28fa0c555e0, and nothing in the compatibility layer implements          it\" }. Bound for FIONBIO ONLY, which is the second spelling of the          fcntl(F_SETFL, O_NONBLOCK) that has been bound since M5 and reaches the same          `Filesystem::set_nonblocking` on the same descriptor. What makes this entry worth          reading is that the scope was DECODED rather than guessed: `ioctl` reaches this binary          through one PLT stub at 0x62d7340 and five BL sites target it, four of which load a          literal request -- 0x5421 FIONBIO at 0x02956388, 0x8913 SIOCGIFFLAGS at 0x055b040c,          0x8912 SIOCGIFCONF at 0x061f4354, 0x8915 SIOCGIFADDR at 0x061f43d0 -- and the fifth is          a pass-through wrapper. The three SIOC* requests enumerate the HOST's interfaces,          nothing in omni-platform can answer them, and each refuses BY NAME rather than being          absent. The file's LAST section.",
    ),
    (
        "isspace",
        "M6's network run, one call after `getentropy` and still on the TLS handshake:          GuestThreadFailure { thread: 8, why: \"the guest called the imported symbol `isspace`          through its thunk at 0x18945ed5e60, and nothing in the compatibility layer implements          it\" }. A PURE BINDING GAP and the SIXTH of this session --          `omni_bionic::ctype::is_space` has existed since phase 1, with the test that asserts          exactly six bytes classify, and nothing had ever called it. That makes six symbols          whose primitive was written against the import list and whose wiring was done against          what a run had reached, which VERIFICATION.md entry 16 names as a pattern rather than a          coincidence. Its neighbour `tolower` is the same shape, has `omni_bionic::ctype::         to_lower` waiting, and is deliberately NOT bound. The file's LAST section.",
    ),
    (
        "getentropy",
        "M6's network run, one call after `getsockname` and the first thing in this project that          is unambiguously TLS: GuestThreadFailure { thread: 8, why: \"the guest called the          imported symbol `getentropy` through its thunk at 0x237481e5ec0, and nothing in the          compatibility layer implements it\" }. `libroblox.so` carries its own OpenSSL (D30) and          OpenSSL seeds its DRBG from `getentropy` where the platform has one, which Android does          from API 28. NOT a binding gap: the entropy source has been wired since phase 3a as          `arc4random_buf`, over the same `omni_platform::process::random_bytes`. What this adds          is the interface's own 256-byte bound, which is a real difference and not a formality --          a device answers EIO above it and the caller has a fallback path that would never run          here. The file's LAST section.",
    ),
    (
        "ldexp",
        "M6, and the FIRST symbol reached because a client-settings request finally          SUCCEEDED. With the application name corrected to `GoogleAndroidApp` the engine          received 1,358,051 bytes of flags instead of a 68-byte `HTTP 400`, began parsing          them, and died here: the guest called the imported symbol `ldexp` through its          thunk and nothing in the compatibility layer implemented it. `x * 2^exp` is what a          JSON number parser does to assemble a mantissa and a binary exponent, so the          document itself is what reached it -- the failing request had never got far enough          to ask. A PURE BINDING GAP and the TENTH of this session:          `omni_bionic::libm::ldexp` has existed since phase 1 and sits DIRECTLY BESIDE          `frexp`, which was bound, and nothing had ever called it. VERIFICATION.md entry 16          twice over -- the death was on a guest thread, so the only thing that reported it          was `guest_thread_failures()`, and while it was unbound the socket counters froze          mid-download and looked like a network stall: 409,075 of 1,358,051 bytes received          and then nothing, for 100 s. The frozen counter was a SYMPTOM of a thread this          layer had killed, not a transfer problem, and two runs were spent measuring read          sizes before that was clear.",
    ),
    (
        "memrchr",
        "M6's network run, and the symbol that proves the certificate bundle is being PARSED.          The APK ships `assets/ssl/cacert.pem` -- 228,725 bytes of authorities -- and OpenSSL's          compiled-in default store is a build machine's path that exists on no device, so on          Android the Java side unpacks the bundle into the app's files directory. D7 says the          Java side is defined rather than executed, so the gate does it. The moment it did, the          engine stopped reporting `HttpError: Unknown` and died here instead: GuestThreadFailure          { thread: 7, why: \"the guest called the imported symbol `memrchr` through its thunk at          0x21955c65d60, and nothing in the compatibility layer implements it\" }. Scanning          backwards is how a PEM reader finds the last `-----END CERTIFICATE-----` in a block.          NOT a binding gap: `memrchr` is a GNU extension that bionic has and this crate did not,          so `omni_bionic::mem::memrchr` is new computation, written as `memchr`'s mirror. With it          bound the fetch completed a TLS handshake against the real endpoint and the engine          reported `HTTP 400` -- a server answer, which is what proves the whole path works.",
    ),
    (
        "getsockname",
        "M6's network run, found the way an Unbound is meant to be found and the FIRST member of \
         the socket group above to be reached by a run rather than bound ahead of one. With \
         `SO_KEEPALIVE` answering, the engine set the three keep-alive TIMING options on the \
         settings socket -- `TCP_KEEPIDLE`, `TCP_KEEPINTVL`, `TCP_KEEPCNT`, which this layer had \
         no variant for and refused by name -- and once those answered the same thread died one \
         call later: GuestThreadFailure { thread: 8, start_routine: 1292942786308, why: \"the \
         guest called the imported symbol `getsockname` through its thunk at 0x12d070352d0, and \
         nothing in the compatibility layer implements it\" }. A connected client asking which \
         local end it was given. NOT a binding gap of the `pthread_mutex_trylock` kind and not \
         new computation either: `omni_platform::net::Socket::local_address` and this file's own \
         `write_peer` both existed, and the handler is the two of them joined. Its sibling \
         `getpeername` is one line away in the import table, is equally implemented in the seam, \
         and is deliberately NOT bound -- no run has reached it. The file's LAST section, with \
         `getpeername` three lines above it.",
    ),
    (
        "geteuid",
        "M6, the first run in which the client-settings success path ran on: SQLite, on the \
         thread that had just taken its first `fcntl(F_SETLK)` record lock, asking whether it is \
         root -- `robustFchown` `fchown`s every file it creates when it is. GuestThreadFailure { \
         thread: 8, why: \"the guest called the imported symbol `geteuid` through its thunk at \
         0x2495c454ff0, and nothing in the compatibility layer implements it\" }, from link \
         0x22be3e8. NEW, and not computation either: nothing in `omni-bionic` or the host knows \
         an Android uid, because the package manager assigns one at install time and the APK \
         does not carry it. So it is a seam with no default, `Bionic::set_app_uid`, which refuses \
         anything but an application uid. `getuid` is its sibling in the same never-referenced \
         section and is deliberately NOT bound -- no run has reached it.",
    ),
    (
        "localtime_r",
        "M6, on the fetch thread, one step past the PlatformSystemDialogHandler registration on \
         the client-settings success path: GuestThreadFailure { thread: 6, why: \"the guest \
         called the imported symbol `localtime_r` through its thunk at 0x1d4e5f84f60, and \
         nothing in the compatibility layer implements it\" }, from link 0x2264274. The Tier C \
         section of the reachable list, reached only through an address-taken edge. NOT new \
         computation: this runtime's local time is UTC -- no TZ, no zone database, no \
         persist.sys.timezone -- which `clocks::mktime` had recorded along with the sentence \
         that a guest calling localtime would be the test of it. It is `gmtime_r`. `localtime` \
         is its neighbour in the same section and is deliberately NOT bound.",
    ),
    (
        "pwrite",
        "M6, on the SQLite thread, the call after its record lock and `geteuid`: SQLite writes \
         its pages with pwrite where the platform has one, and Android does. GuestThreadFailure \
         { thread: 8, why: \"the guest called the imported symbol `pwrite` through its thunk at \
         0x1d4e5f85dd0, and nothing in the compatibility layer implements it\" }. NOT a binding \
         gap and not quite new: `pread` had a seam method and a Windows backend, and this is \
         their mirror at every layer -- including the saved-and-restored cursor, because \
         `seek_write` moves the Windows file pointer exactly as `seek_read` was measured to \
         (and a mutation dropping the restore fails `pwrite_does_not_move_the_descriptors_offset`, \
         which is that measurement). `pread64`/`pwrite64` are not imported on LP64.",
    ),
    (
        "fseeko",
        "M6, a guest worker's file class seeking (`libroblox.so` link 0x2b59734): GuestThreadFailure \
         { thread: 3, why: \"the guest called the imported symbol `fseeko` through its thunk at \
         0x1b3d07a4f40, and nothing in the compatibility layer implements it\" }. And the run \
         that found it is the one that explained the handoff's MemoryFault: a second worker, \
         sampling a 128-entry table of per-thread records, read one at 0x...3fd0 -- just under a \
         page top, the shape of a struct at the top of a thread's stack -- whose memory was gone, \
         because this layer had killed its thread and unmapped that stack. NEW COMPUTATION: the \
         seam had no seek at all; `Filesystem::seek` is portable `std`, and the stream half is a \
         seek plus the end-of-file indicator cleared (C17 7.21.9.2p5). Its Tier C neighbour \
         `fseek` is imported and deliberately NOT bound.",
    ),
    (
        "ftello",
        "M6, DECODED rather than reached, and said so: it is the call on fseeko's success path \
         at 0x2b5975c, one instruction after the stub `tools/init_reach.py`'s PLT map names as \
         fseeko (the map names a stub only when two encodings agree). The run that reaches \
         fseeko reaches this. `lseek(fd, 0, SEEK_CUR)` on an unbuffered stream. `ftell` is \
         imported and deliberately NOT bound.",
    ),
    (
        "epoll_create1",
        "M6, on the fetch thread straight after `RbxTransport I/O backend chosen: sys` -- the \
         engine's OWN transport, at link 0x23cc788 with flags 0: GuestThreadFailure { thread: 6, \
         why: \"the guest called the imported symbol `epoll_create1` through its thunk at \
         0x1b3d07a5020, and nothing in the compatibility layer implements it\" }. NEW \
         COMPUTATION: an epoll instance is a new descriptor kind in the one table (D30), with \
         close removing a descriptor from every interest list.",
    ),
    (
        "epoll_ctl",
        "M6, DECODED on the same transport rather than reached, and said so: 0x23cc848 is the \
         call right after epoll_create1's owner registers a descriptor -- ADD or MOD, with \
         events built from EPOLLIN/EPOLLOUT and nothing else, and a 16-byte arm64 epoll_event \
         (data at offset 8, `stp xzr, x20, [sp]` then `str w8, [sp]`). Every other event bit is \
         refused by name.",
    ),
    (
        "epoll_wait",
        "M6, DECODED on the same transport: 0x23ce040, maxevents 1024 and a timeout rounded up \
         from microseconds, and 0x28738a0 elsewhere with -1. Level-triggered on the wait `poll` \
         already had; -1 waits in renewable passes that each re-check the stop switch, and a \
         list in which nothing can become ready is refused rather than slept on for ever.",
    ),
    (
        "timerfd_create",
        "M6, the transport's timer, one call after epoll_create1 (link 0x23cc718): \
         GuestThreadFailure { thread: 6, why: \"the guest called the imported symbol \
         `timerfd_create` through its thunk at 0x1f84c7f7f80, and nothing in the compatibility \
         layer implements it\" }, as timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK). NEW \
         COMPUTATION: the first descriptor whose readiness changes with TIME, which nothing \
         announces, so ReadinessSource::Timer tells a waiter to cap its wait at the deadline. \
         CLOCK_MONOTONIC only -- the clock the guest's own clock_gettime reads.",
    ),
    (
        "timerfd_settime",
        "M6, DECODED on the same transport: one-shot, armed both relative (0x23cdea0, flags 0) \
         and absolute (0x23cdf5c, TFD_TIMER_ABSTIME), from a 32-byte itimerspec whose value the \
         engine stores at offset 16. The absolute form is why the timer is on the guest's own \
         monotonic clock and no other.",
    ),
    (
        "fsync",
        "M6, the SQLite thread committing, after its record lock, geteuid and pwrite: \
         GuestThreadFailure { thread: 8, why: \"the guest called the imported symbol `fsync` \
         through its thunk at 0x1f84c7f8620, and nothing in the compatibility layer implements \
         it\" }. File::sync_all for a regular file; EINVAL for a descriptor with nothing to \
         synchronise; a directory refuses by name until a run shows SQLite's directory sync.",
    ),
];

/// Every symbol bound here is an import of `libroblox.so`, no symbol is bound twice, and anything
/// outside Task 1's 188 is named in [`BEYOND_THE_PREDICTION`] with how it was found.
///
/// The list is the specification (`ARCHITECTURE.md` section 5), so a handler bound under a name
/// that is not an import at all is either a typo — which would leave the real symbol `Unbound` and
/// the typo unreachable, both silently — or scope creep.
#[test]
fn every_bound_symbol_is_in_the_reachable_set_and_is_bound_once() {
    let reachable = reachable_imports();
    let every_import = all_imports();
    let beyond: std::collections::BTreeSet<&str> =
        BEYOND_THE_PREDICTION.iter().map(|(symbol, _)| *symbol).collect();
    assert_eq!(beyond.len(), BEYOND_THE_PREDICTION.len(), "a symbol is listed twice");
    let mut seen = std::collections::BTreeSet::new();
    for symbol in Bionic::bound_symbols() {
        assert!(
            every_import.contains(symbol),
            "`{symbol}` is bound but `libroblox.so` does not import it at all"
        );
        assert!(
            reachable.contains(symbol) || beyond.contains(symbol),
            "`{symbol}` is bound, is outside the 188, and is not named in \
             BEYOND_THE_PREDICTION with how it was found"
        );
        assert!(seen.insert(symbol), "`{symbol}` is bound twice");
    }
    for (symbol, _) in BEYOND_THE_PREDICTION {
        assert!(
            !reachable.contains(*symbol),
            "`{symbol}` IS one of the 188, so listing it as beyond the prediction is wrong"
        );
        assert!(seen.contains(symbol), "`{symbol}` is listed as bound and is not bound");
    }
    assert_eq!(
        seen.len(),
        Bionic::bound_symbols().count(),
        "the two tables must not share a symbol: one address can only have one binding"
    );
}

/// The count, stated exactly, with the two tables separated.
///
/// **A pinned figure, not a target.** It exists so that adding or losing a binding is a visible
/// change rather than a number in a report nobody re-derives — this project has had four wrong
/// counts reach its decision record.
#[test]
fn the_bound_count_is_exactly_what_this_phase_claims() {
    let symbols: Vec<&str> = Bionic::bound_symbols().collect();
    assert_eq!(symbols.len(), 221, "bound symbols: {symbols:?}");
    // Phase 1 bound 86 — 84 inline and two re-entrant. Phase 2 added ten: the four `dl*` refusals
    // inline, and `dl_iterate_phdr` plus the five guest-memory calls on the exit path, for 96.
    // Phase 3a adds 23, all inline: five clocks, fourteen process-and-environment, four logging.
    // Phase 3b adds 29, all inline: eighteen descriptor symbols and eleven `FILE *` ones.
    // Phase 3c adds 4 inline (the signal family) and 4 re-entrant (thread lifecycle).
    // Phase 3d adds 8 inline (the network group) and phase 3e 4 more, for 168.
    // **Task 4, the gate, adds the five in `BEYOND_THE_PREDICTION`** — all inline — and moves
    // `dlopen`, `dlsym` and `dlclose` from the fast path to the exit path, because answering them
    // needs the boundary's symbol table and `ImportCall` deliberately cannot reach it. So
    // 157 + 5 - 3 = 159 inline and 11 + 3 = 14 re-entrant.
    // **M4's gate adds three more** -- `strnlen`, `gmtime`, `getcwd` -- all inline, for 162.
    // **M5 adds five**, all inline, for 167. `pipe`, `fcntl` and `write` are the first entries
    // in `BEYOND_THE_PREDICTION` found by *decoding* the guest's instructions (jni-surface.md
    // §5.2) rather than by watching a run reach them, which is why they could be bound before the
    // call that needs them existed. `pthread_attr_setdetachstate` is the fourth and was found the
    // other way round -- M5's gate ran into it as an `Unbound`, which is the failure that shape
    // exists to produce.
    // `strftime` is the fifth, and it is the one that closes a *known* gap rather than finding
    // a new one: M4's gate recorded `nativeInitFastLog` failing on it by name.
    // Each of the thirteen is a symbol `libroblox.so` imports that the static closure did not
    // predict. D17 says 188 is a lower bound; this is by how much, so far.
    // **M6 adds two more, both inline and both found by a guest WORKER THREAD dying on an
    // `Unbound`** rather than by a scripted downcall stopping -- which is the first time that
    // has been the discovery route, and it is what `guest_thread_failures` exists for.
    // `pthread_getattr_np` needed new logic: it is the first member of the attr family that
    // reports a *live* thread, so `pthread_create` had to start recording where it puts a
    // stack. `pthread_mutex_trylock` needed none -- `omni_bionic::mutex::trylock` was written
    // and unit-tested in phase 3c and nothing had ever called it.
    // **`inet_pton` is the one that needed NEW COMPUTATION**, and it is worth separating from
    // the binding gaps around it: every other symbol this session added had an `omni-bionic`
    // function waiting for a caller, and nothing in that crate parsed an address at all. Its
    // opposite `inet_ntop` has been bound since phase 3d and IS one of the 188; this spelling is
    // in the file's LAST section, and a guest worker thread dying on it is what found it.
    // **M6's network phase adds nine more, all inline, for 187**, and the shape of the nine is
    // worth separating. `setsockopt` and `strtoull_l` were found the way an `Unbound` is meant
    // to be found -- a guest WORKER THREAD died on each, which is what `guest_thread_failures`
    // exists for. The other seven -- `connect`, `getsockopt`, `bind`, `shutdown`, `sendto`,
    // `recvfrom`, `__sendto_chk` -- are the rest of ONE client call sequence, bound together
    // and **not yet reached by a run**, which `BEYOND_THE_PREDICTION` says of each of them in
    // as many words. D17's rule is that a symbol is bound when a run has reached it, and this
    // is the place that rule is stretched: the run stops at the first missing member of the
    // sequence and each round trip of that loop is three minutes. The honest statement is the
    // one in the table -- measured for two of the nine, decided for seven.
    //
    // **M6's network run adds seven more, inline, for 194.** The first is the first member of that
    // socket group a run has actually reached rather than one bound ahead of a run: `getsockname`,
    // which the settings-fetch thread called once the three keep-alive TIMING options
    // (`TCP_KEEPIDLE`, `TCP_KEEPINTVL`, `TCP_KEEPCNT`) stopped refusing. It cost no new
    // computation and closed no binding gap -- `Socket::local_address` and this adapter's own
    // `write_peer` both already existed -- so the honest category for it is neither of the two
    // this comment has used before: it is a symbol whose *pieces* were all present and which
    // nothing had asked for. `getpeername` is its sibling, is equally implemented in the seam,
    // and stays Unbound because no run has reached it. The second is `getentropy`, one call
    // further on and the first symbol in this project that is unambiguously TLS: the engine's own
    // OpenSSL seeding its DRBG. Its entropy source has been wired since phase 3a under a
    // different name (`arc4random_buf`), so what it adds is the interface's own 256-byte bound
    // and nothing else.
    //
    // Three symbols LEFT the refusal list in the same change and none of them is new here:
    // `socket`, `getaddrinfo` and `freeaddrinfo` were bound and refusing since phase 3d and now
    // answer, which is a category change rather than a count change. See
    // `the_final_split_of_the_reachable_set_is_what_the_record_claims`.
    //
    // **The settings success path adds `geteuid`, inline, for 197**: SQLite asking whether it is
    // root. Answered from a seam with no default, because no source this runtime has knows an
    // Android uid. See its `BEYOND_THE_PREDICTION` entry. **And `localtime_r`, for 198**, one
    // step further along the same path: `gmtime_r` under the local-time-is-UTC decision.
    // **And `pwrite`, for 199**: SQLite's page writes, `pread`'s mirror at every layer.
    // **`fseeko` and `ftello`, for 201**: a file class's seek and position, the second decoded.
    // **`epoll_create1`, `epoll_ctl`, `epoll_wait`, for 204**: the engine's own transport, the
    // first reached and the other two decoded on the same object.
    // **`timerfd_create`, `timerfd_settime` and `fsync`, for 207**: the transport's timer and
    // SQLite's commit.
    assert_eq!(Bionic::inline_symbols().count(), 207);
    assert_eq!(Bionic::reentrant_symbols().count(), 14);
    // Plus the eighteen `STT_OBJECT` data objects, which are not functions and are not bound to a
    // handler at all, and the two **declared absent** — a weak reference to either resolves to
    // null, which is what the guest's own null test expects. The identity below is what makes the
    // total meaningful: everything bound, minus what the prediction missed, plus the data objects
    // and the two absent ones, is exactly the 188 the initializers were predicted to reach.
    assert_eq!(symbols.len() - BEYOND_THE_PREDICTION.len() + 18 + 2, 188);
    assert_eq!(omni_android::bionic::DATA_OBJECTS.len(), 18);
    assert_eq!(omni_android::bionic::ABSENT_SYMBOLS.len(), 2);

    // **Membership, not just a total** — a count cannot see a substitution, and this project has
    // had a list whose count stayed right while two members were wrong and two were missing. The
    // 23 phase 3a binds are named one by one.
    let bound: std::collections::BTreeSet<&str> = symbols.iter().copied().collect();
    let phase_3a = [
        // clocks
        "clock_gettime",
        "gettimeofday",
        "gmtime_r",
        "nanosleep",
        "usleep",
        // process and environment
        "getpid",
        "sched_getcpu",
        "arc4random_buf",
        "getauxval",
        "getenv",
        "__system_property_get",
        "abort",
        "__stack_chk_fail",
        "_exit",
        "android_set_abort_message",
        "sysconf",
        "sysinfo",
        "prctl",
        "syscall",
        // logging
        "__android_log_print",
        "syslog",
        "openlog",
        "closelog",
    ];
    assert_eq!(phase_3a.len(), 23);
    for symbol in phase_3a {
        assert!(bound.contains(symbol), "`{symbol}` is in phase 3a's scope and is not bound");
    }

    // **Phase 3b's 29, named one by one**, derived from the plan's `3b` row and cross-checked
    // against `tools/os_surface.py`'s `file-io` bucket intersected with the reachable remainder.
    let phase_3b = [
        // the eighteen descriptor symbols
        "__open_2",
        "__write_chk",
        "access",
        "close",
        "closedir",
        "fstat",
        "lstat",
        "mkdir",
        "open",
        "opendir",
        "pread",
        "read",
        "readdir",
        "rename",
        "rmdir",
        "stat",
        "statvfs",
        "unlink",
        // bionic's `FILE *` layer on top of them
        "fclose",
        "fdopen",
        "feof",
        "fflush",
        "fgets",
        "fileno",
        "fopen",
        "fputc",
        "fputs",
        "fread",
        "fwrite",
    ];
    assert_eq!(phase_3b.len(), 29);
    for symbol in phase_3b {
        assert!(bound.contains(symbol), "`{symbol}` is in phase 3b's scope and is not bound");
    }

    // **Phase 3c's eight, named one by one**, derived the same way: the 188 minus everything
    // named in `handlers.rs` and `data.rs`, intersected with the plan's `3c` row. Four of them
    // are answered, three refuse by name and one -- `sigfillset` -- is pure computation over the
    // guest's own `sigset_t` and is implemented in `omni-bionic`.
    let phase_3c = [
        "pthread_create",
        "pthread_detach",
        "pthread_getschedparam",
        "pthread_join",
        "pthread_sigmask",
        "raise",
        "sigaction",
        "sigfillset",
    ];
    assert_eq!(phase_3c.len(), 8);
    for symbol in phase_3c {
        assert!(bound.contains(symbol), "`{symbol}` is in phase 3c's scope and is not bound");
    }
    // The three that create, join and detach a thread are on the **exit path**, and that is F9
    // rather than a preference: a start routine is guest code and `pthread_create` maps the new
    // thread's stack. `pthread_getschedparam` does neither and is inline with the four signal
    // symbols, which also hold no CPU.
    let reentrant: std::collections::BTreeSet<&str> = Bionic::reentrant_symbols().collect();
    for symbol in ["pthread_create", "pthread_join", "pthread_detach"] {
        assert!(reentrant.contains(symbol), "`{symbol}` must be re-entrant (F9)");
    }
    for symbol in ["sigfillset", "sigaction", "raise", "pthread_sigmask", "pthread_getschedparam"] {
        assert!(!reentrant.contains(symbol), "`{symbol}` holds no CPU and belongs inline");
    }

    // **Phase 3d's eight, named one by one**, derived the same way and exactly the plan's `3d`
    // row. Two are answered out of `omni-bionic`, two are implemented over the descriptor table
    // that already existed, and four refuse by name.
    let phase_3d = [
        "eventfd",
        "freeaddrinfo",
        "gai_strerror",
        "getaddrinfo",
        "inet_ntop",
        "poll",
        "select",
        "socket",
    ];
    assert_eq!(phase_3d.len(), 8);
    for symbol in phase_3d {
        assert!(bound.contains(symbol), "`{symbol}` is in phase 3d's scope and is not bound");
    }

    // **Phase 3e's six**, of which four are bound and **two are not bound at all**: a weak
    // reference to `__gcov_dump` or `__gcov_flush` resolves to null, because the guest's own code
    // tests the address before calling and no Android libc supplies either. `bionic::absent` has
    // the decoded instructions and `tests/libroblox.rs` asserts them against the real library.
    let phase_3e_bound = ["clock", "time", "mallinfo", "longjmp"];
    for symbol in phase_3e_bound {
        assert!(bound.contains(symbol), "`{symbol}` is in phase 3e's scope and is not bound");
    }
    let absent: std::collections::BTreeSet<&str> =
        omni_android::bionic::ABSENT_SYMBOLS.iter().map(|a| a.symbol).collect();
    assert_eq!(
        absent,
        ["__gcov_dump", "__gcov_flush"].into_iter().collect::<std::collections::BTreeSet<_>>()
    );
    for symbol in &absent {
        assert!(
            !bound.contains(symbol),
            "`{symbol}` is declared absent and must not also have a handler: a weak reference to \
             it has to resolve to null, and a bound symbol has an address"
        );
    }
    assert_eq!(phase_3e_bound.len() + absent.len(), 6, "the plan's `3e` row");

    // **Nothing is left.** The remainder is asserted as a **set difference against the reachable
    // file** rather than as a total — a count cannot see a substitution, and this project has had
    // a list whose count stayed right while two members were wrong and two were missing — and
    // what it must now equal is exactly the two deliberately-absent symbols.
    let reachable = reachable_imports();
    let data: std::collections::BTreeSet<&str> =
        omni_android::bionic::DATA_OBJECTS.iter().map(|o| o.symbol).collect();
    let remainder: std::collections::BTreeSet<&str> = reachable
        .iter()
        .map(String::as_str)
        .filter(|symbol| !bound.contains(symbol) && !data.contains(symbol))
        .collect();
    assert_eq!(
        remainder, absent,
        "every one of the 188 reachable imports is now serviced, refused by name, placed as a \
         data object, or deliberately absent -- and the only members of that last category are \
         the two `__gcov_*`"
    );
    assert_eq!(
        bound.len() - BEYOND_THE_PREDICTION.len() + data.len() + absent.len(),
        188,
        "the whole reachable set, with the five symbols M3's gate found outside it subtracted"
    );
}

/// **Task 2 review finding F9, asserted rather than trusted to a comment.**
///
/// `ImportCall::mem()` reaches the whole `GuestSpace`, and an inline handler runs inside one of the
/// translating backend's own callbacks with generated code live — where the pager's "the thread
/// running guest code must not hold this space's lock" invariant is reachable and where unmapping
/// or reprotecting a range invalidates memory live translations reference. Nothing in the types
/// prevents `mmap` being moved into the inline table; this is what notices.
///
/// The converse matters too: a symbol on the exit path costs three times as much per call (D17),
/// so the list is pinned in both directions.
#[test]
fn dispatch_paths_are_what_f9_requires() {
    let reentrant: std::collections::BTreeSet<&str> = Bionic::reentrant_symbols().collect();
    for symbol in ["mmap", "munmap", "mprotect", "madvise", "mlock"] {
        assert!(
            reentrant.contains(symbol),
            "`{symbol}` reaches GuestSpace and must be serviced on the exit path (F9)"
        );
    }
    // These three call guest code, which an inline handler structurally cannot (D18).
    for symbol in ["pthread_once", "qsort", "dl_iterate_phdr"] {
        assert!(reentrant.contains(symbol), "`{symbol}` calls guest code");
    }
    // Phase 3c's three, which are on the exit path for **both** of F9's reasons at once: a start
    // routine is guest code, and `pthread_create` maps the new thread's stack. The fourth,
    // `pthread_getschedparam`, reaches neither and stays inline — putting it here for tidiness
    // would cost it 3x per call for nothing, which is the over-correction half of this test.
    for symbol in ["pthread_create", "pthread_join", "pthread_detach"] {
        assert!(reentrant.contains(symbol), "`{symbol}` belongs with thread lifecycle (F9)");
    }
    assert!(
        !reentrant.contains("pthread_getschedparam"),
        "`pthread_getschedparam` runs no guest code and touches no mapping (D17)"
    );
    // Phases 3d and 3e added twelve handlers and **not one of them is re-entrant**: none runs
    // guest code and none reaches `GuestSpace`. `poll` and `select` read and write guest memory,
    // which `memcpy` already does from the fast path, and they sleep, which `nanosleep` already
    // does there too. Pinned in this direction as well as the other, because moving one here
    // would cost it 3x per call for nothing (D17).
    for symbol in [
        "inet_ntop",
        // M6's addition to this group, and inline for the same reason as its opposite: it reads
        // a guest string and writes four or sixteen bytes, which is what `memcpy` already does
        // from the fast path.
        "inet_pton",
        "gai_strerror",
        "poll",
        "select",
        "socket",
        "eventfd",
        "getaddrinfo",
        "freeaddrinfo",
        "time",
        "clock",
        "mallinfo",
        "longjmp",
    ] {
        assert!(
            !reentrant.contains(symbol),
            "`{symbol}` runs no guest code and touches no mapping (D17)"
        );
    }
    // **Task 4 moved three, and for a reason F9 does not cover.** `dlopen`, `dlsym` and
    // `dlclose` run no guest code and touch no mapping — so by F9 alone they belong inline — but
    // answering them needs the boundary's own symbol table, and `ImportCall` holds no boundary at
    // all, deliberately (D18 makes that a type property). The exit path is the only place they
    // can be served from. They are not hot: `libroblox.so` makes fourteen direct `dlopen` calls
    // in the whole image.
    for symbol in ["dlopen", "dlsym", "dlclose"] {
        assert!(
            reentrant.contains(symbol),
            "`{symbol}` needs the boundary's symbol table, which only the exit path can reach"
        );
    }
    assert_eq!(reentrant.len(), 14, "nothing else belongs on the slow path: {reentrant:?}");
    let inline: std::collections::BTreeSet<&str> = Bionic::inline_symbols().collect();
    // `dlerror` stays on the fast path: it reads a thread-local string and needs no table.
    assert!(inline.contains("dlerror"), "`dlerror` has no reason to exit the run loop");
    assert!(inline.is_disjoint(&reentrant));
}

/// Every undefined symbol the reachable-import file lists, across all eight of its sections.
///
/// The whole of `libroblox.so`'s import table: **565**, which is the figure Global Constraint 3
/// pins. It is what a bound symbol has to be in, now that the 188 is known to be a lower bound.
fn all_imports() -> std::collections::BTreeSet<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/research/init-reachable-imports.txt");
    let text = std::fs::read_to_string(path).expect("the reachable-import list");
    let mut out = std::collections::BTreeSet::new();
    let mut started = false;
    for line in text.lines() {
        if line.starts_with("###") {
            started = true;
            continue;
        }
        let symbol = line.trim();
        if started && !symbol.is_empty() {
            out.insert(symbol.to_string());
        }
    }
    assert_eq!(out.len(), 565, "libroblox.so imports 565 symbols (Global Constraint 3)");
    out
}

/// Parse the first six sections of the reachable-import list: the 188 symbols that are
/// statically reachable from the 3,594 `init_array` roots.
fn reachable_imports() -> std::collections::BTreeSet<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/research/init-reachable-imports.txt");
    let text = std::fs::read_to_string(path).expect("the reachable-import list");
    let mut out = std::collections::BTreeSet::new();
    let mut section = 0usize;
    for line in text.lines() {
        if line.starts_with("###") {
            section += 1;
            continue;
        }
        let symbol = line.trim();
        if symbol.is_empty() || section == 0 || section > 6 {
            continue;
        }
        out.insert(symbol.to_string());
    }
    assert_eq!(out.len(), 188, "the reachable set's first six sections are 188 symbols");
    out
}

// =================================================================== strings and memory

#[test]
fn strlen_reads_a_guest_string_through_a_real_thunk() {
    let _guard = serialized();
    let f = fixture();
    let at = f.cstring(f.guest.data + 0x100, b"the quick brown fox");
    let n = value_of(&f, "strlen", |asm| {
        asm.mov(0, at as u64);
    });
    assert_eq!(n, 19);
}

/// **`strcmp` returns bionic's byte difference, and the adapter must not touch it.**
///
/// The C standard fixes only the sign; bionic fixes the magnitude, and `omni-bionic` follows
/// bionic (pinned there by mutation row `bionic-B1`). A handler that normalised to `-1/0/1`
/// would be changing something guest code can observe. `'a' - 'z'` is `-25`, and it has to arrive
/// in `X0` **sign-extended**, because `Ret::u64` would deliver `0xFFFF_FFE7` as a large positive
/// `int` and turn every "less than" into "greater than".
#[test]
fn strcmp_delivers_the_byte_difference_sign_extended() {
    let _guard = serialized();
    let f = fixture();
    let a = f.cstring(f.guest.data + 0x100, b"a");
    let b = f.cstring(f.guest.data + 0x140, b"z");
    let less = value_of(&f, "strcmp", |asm| {
        asm.mov(0, a as u64);
        asm.mov(1, b as u64);
    });
    assert_eq!(less as i64, -25, "'a' - 'z' = -25, not -1 and not 0xFFFFFFE7");
    let more = value_of(&f, "strcmp", |asm| {
        asm.mov(0, b as u64);
        asm.mov(1, a as u64);
    });
    assert_eq!(more as i64, 25);
    let same = value_of(&f, "strcmp", |asm| {
        asm.mov(0, a as u64);
        asm.mov(1, a as u64);
    });
    assert_eq!(same, 0);
}

/// `memcmp` carries the same convention, through a different module.
#[test]
fn memcmp_delivers_the_byte_difference_sign_extended() {
    let _guard = serialized();
    let f = fixture();
    let a = f.guest.data + 0x100;
    let b = f.guest.data + 0x140;
    f.guest.write_bytes(a, &[1, 2, 3, 4]);
    f.guest.write_bytes(b, &[1, 2, 200, 4]);
    let diff = value_of(&f, "memcmp", |asm| {
        asm.mov(0, a as u64);
        asm.mov(1, b as u64);
        asm.mov(2, 4);
    });
    assert_eq!(diff as i64, 3 - 200, "bionic's byte difference, not a clamped -1");
}

#[test]
fn memcpy_moves_bytes_and_returns_its_destination() {
    let _guard = serialized();
    let f = fixture();
    let src = f.guest.data + 0x100;
    let dst = f.guest.data + 0x200;
    let payload: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(7).wrapping_add(3)).collect();
    f.guest.write_bytes(src, &payload);
    let returned = value_of(&f, "memcpy", |asm| {
        asm.mov(0, dst as u64);
        asm.mov(1, src as u64);
        asm.mov(2, 64);
    });
    assert_eq!(returned, dst as u64, "memcpy returns dst");
    let mut copied = vec![0u8; 64];
    for (index, slot) in copied.iter_mut().enumerate() {
        *slot = (f.guest.read_u64((dst + index) & !7) >> (8 * ((dst + index) & 7))) as u8;
    }
    assert_eq!(copied, payload);
}

/// **`long` is 64-bit on the guest and 32-bit on host Windows.** A `strtol` handler that returned
/// 32 bits would truncate, and the truncation is invisible for every small value — which is every
/// value a casual test uses.
#[test]
fn strtol_returns_a_full_64_bit_long() {
    let _guard = serialized();
    let f = fixture();
    let text = f.cstring(f.guest.data + 0x100, b"1234567890123");
    let value = value_of(&f, "strtol", |asm| {
        asm.mov(0, text as u64);
        asm.mov(1, 0); // endptr
        asm.mov(2, 10); // base
    });
    assert_eq!(value, 1_234_567_890_123, "a value that does not fit in 32 bits");
    let negative = f.cstring(f.guest.data + 0x140, b"-9007199254740993");
    let value = value_of(&f, "strtol", |asm| {
        asm.mov(0, negative as u64);
        asm.mov(1, 0);
        asm.mov(2, 10);
    });
    assert_eq!(value as i64, -9_007_199_254_740_993);
}

// =================================================================== errno, really

/// **`errno` is guest-visible storage, and this proves it end to end.**
///
/// The guest calls `strtol` on a number too large for a `long`, which POSIX says sets `ERANGE`;
/// then it calls `__errno()` and dereferences what comes back. Nothing about this works unless
/// the per-thread block is mapped, `__errno` returns *this* thread's slot, and `set_errno` wrote
/// through to it.
///
/// `ERANGE` is asserted as **34**, the Linux number. The host is Windows, where `ERANGE` is 34 as
/// well — so this assertion would pass by luck. `ETIMEDOUT` is the one that would not (110 against
/// Windows' 121/10060), and `omni-bionic`'s `errno.rs` pins every constant.
#[test]
fn errno_is_written_where_the_guest_can_read_it() {
    let _guard = serialized();
    let f = fixture();
    let text = f.cstring(f.guest.data + 0x100, b"999999999999999999999999");
    let strtol = f.thunk("strtol");
    let errno = f.thunk("__errno");

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(0, text as u64);
    asm.mov(1, 0);
    asm.mov(2, 10);
    asm.bl(strtol);
    asm.push(str_imm(0, 22, 0)); // the saturated value
    asm.bl(errno);
    asm.push(str_imm(0, 22, 8)); // the errno cell's address
    asm.push(ldr_w(1, 0, 0)); // *errno
    asm.push(str_imm(1, 22, 16));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");

    assert_eq!(f.guest.read_u64(f.guest.data) as i64, i64::MAX, "strtol saturates on overflow");
    let cell = f.guest.read_u64(f.guest.data + 8) as usize;
    assert_eq!(cell, f.bionic.arena(), "the first attached thread gets the first block");
    assert_eq!(f.guest.read_u64(f.guest.data + 16), 34, "ERANGE, the Linux value");
}

/// `strerror` returns a pointer into **this thread's** scratch, and the bytes are really there.
///
/// `EINVAL` is used rather than `ETIMEDOUT` because `omni-bionic`'s message table holds ten codes
/// and 110 is not one of them: it answers `Unknown error`, which is bionic's own fallback shape
/// and is pinned here as the second half of the test rather than quietly avoided.
#[test]
fn strerror_returns_a_readable_message_in_per_thread_scratch() {
    let _guard = serialized();
    let f = fixture();
    let pointer = value_of(&f, "strerror", |asm| {
        asm.mov(0, 22); // EINVAL, the Linux value
    }) as usize;
    assert!(pointer >= f.bionic.arena(), "the message must live in the adapter's own arena");
    assert_eq!(f.read_cstring(pointer), b"Invalid argument");

    // A code the table does not carry falls back rather than returning an empty string or a stale
    // one: the previous call's message is still in the buffer and must be overwritten.
    let pointer = value_of(&f, "strerror", |asm| {
        asm.mov(0, 110); // ETIMEDOUT: a Linux value the table does not hold
    }) as usize;
    assert_eq!(f.read_cstring(pointer), b"Unknown error");
}

// =================================================================== pthread

#[test]
fn pthread_self_is_stable_and_is_never_the_reserved_zero() {
    let _guard = serialized();
    let f = fixture();
    let first = value_of(&f, "pthread_self", |_| {});
    let second = value_of(&f, "pthread_self", |_| {});
    assert_ne!(first, 0, "GuestThreadId::NONE is reserved and no live thread has it");
    assert_eq!(first, second, "one host thread keeps one pthread_t");
}

/// An uncontended `pthread_mutex_init` / `lock` / `unlock` cycle, all three through real thunks,
/// with the guest observing the return code of each.
#[test]
fn a_mutex_round_trip_locks_and_unlocks() {
    let _guard = serialized();
    let f = fixture();
    let mutex = f.guest.data + 0x200;
    f.guest.write_bytes(mutex, &[0u8; 40]);

    let init = f.thunk("pthread_mutex_init");
    let lock = f.thunk("pthread_mutex_lock");
    let unlock = f.thunk("pthread_mutex_unlock");

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(0, mutex as u64);
    asm.mov(1, 0); // default attributes
    asm.bl(init);
    asm.push(str_imm(0, 22, 0));
    asm.mov(0, mutex as u64);
    asm.bl(lock);
    asm.push(str_imm(0, 22, 8));
    asm.mov(0, mutex as u64);
    asm.bl(unlock);
    asm.push(str_imm(0, 22, 16));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");
    assert_eq!(f.guest.read_u64(f.guest.data), 0, "pthread_mutex_init");
    assert_eq!(f.guest.read_u64(f.guest.data + 8), 0, "pthread_mutex_lock");
    assert_eq!(f.guest.read_u64(f.guest.data + 16), 0, "pthread_mutex_unlock");
    // The lock was uncontended, so nothing should have blocked. **This is a watch, not a
    // detector**: it rises whenever the sync layer blocks and stays at zero under every defect
    // this adapter could have, so it says "no thread slept" and nothing more.
    let (waits, _) = f.bionic.futex().activity();
    assert_eq!(waits, 0, "an uncontended lock must not enter the futex");
}

/// `pthread_key_create` writes a 32-bit key, and `setspecific`/`getspecific` round-trip a value
/// that is wider than 32 bits — which is the part a `void *` handler can silently truncate.
#[test]
fn a_tls_key_round_trips_a_full_64_bit_value() {
    let _guard = serialized();
    let f = fixture();
    let key_cell = f.guest.data + 0x200;
    let value = 0xDEAD_BEEF_CAFE_0001u64;

    let create = f.thunk("pthread_key_create");
    let set = f.thunk("pthread_setspecific");
    let get = f.thunk("pthread_getspecific");

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(0, key_cell as u64);
    asm.mov(1, 0); // no destructor
    asm.bl(create);
    asm.push(str_imm(0, 22, 0));
    asm.push(ldr_w(0, 22, 0x200)); // the key the handler wrote
    asm.mov(1, value);
    asm.bl(set);
    asm.push(str_imm(0, 22, 8));
    asm.push(ldr_w(0, 22, 0x200));
    asm.bl(get);
    asm.push(str_imm(0, 22, 16));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");
    assert_eq!(f.guest.read_u64(f.guest.data), 0, "pthread_key_create");
    assert_eq!(f.guest.read_u64(f.guest.data + 8), 0, "pthread_setspecific");
    assert_eq!(f.guest.read_u64(f.guest.data + 16), value, "the full 64-bit void *");
}

/// **`pthread_once` runs guest code**, which an inline handler structurally cannot do, so it is on
/// the exit path. The initialiser increments a counter in guest memory; calling `pthread_once`
/// three times must leave it at one.
#[test]
fn pthread_once_runs_the_guest_initialiser_exactly_once() {
    let _guard = serialized();
    let f = fixture();
    let once_word = f.guest.data + 0x200;
    let counter = f.guest.data + 0x208;
    f.guest.write_u32(once_word, 0);
    f.guest.write_u64(counter, 0);

    // `void init(void) { ++*counter; }`
    let init_at = f.guest.next_entry();
    let mut init = Asm::at(init_at);
    init.mov(9, counter as u64);
    init.push(ldr_imm(10, 9, 0));
    init.mov(11, 1);
    init.push(add_reg(10, 10, 11));
    init.push(str_imm(10, 9, 0));
    init.push(ret(30));
    f.guest.load(init.words());

    let once = f.thunk("pthread_once");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    for _ in 0..3 {
        asm.mov(0, once_word as u64);
        asm.mov(1, init_at as u64);
        asm.bl(once);
    }
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");
    assert_eq!(f.guest.read_u64(counter), 1, "the initialiser ran once, not three times");
    // It really went out through the exit path and really called back into the guest.
    let crossings = f.boundary.crossings();
    assert_eq!(crossings.exits, 3, "three pthread_once calls, all on the exit path");
    assert_eq!(crossings.guest_calls, 1, "one call into the guest initialiser");
}

/// **`qsort` with a guest comparator**, the other direction of the boundary. Four `int`s, sorted
/// ascending by a comparator the guest supplies.
#[test]
fn qsort_sorts_through_a_guest_comparator() {
    let _guard = serialized();
    let f = fixture();
    let base = f.guest.data + 0x200;
    let input: [i32; 4] = [7, -3, 42, 0];
    for (index, value) in input.iter().enumerate() {
        f.guest.write_u32(base + index * 4, *value as u32);
    }

    // `int cmp(const int *a, const int *b) { return *a - *b; }`
    let cmp_at = f.guest.next_entry();
    let mut cmp = Asm::at(cmp_at);
    cmp.push(ldr_w(2, 0, 0));
    cmp.push(ldr_w(3, 1, 0));
    cmp.push(sub_reg(0, 2, 3));
    cmp.push(ret(30));
    f.guest.load(cmp.words());

    let qsort = f.thunk("qsort");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, base as u64);
    asm.mov(1, 4); // nmemb
    asm.mov(2, 4); // size
    asm.mov(3, cmp_at as u64);
    asm.bl(qsort);
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");

    let sorted: Vec<i32> = (0..4)
        .map(|i| (f.guest.read_u64((base + i * 4) & !7) >> (32 * (((base + i * 4) & 7) / 4))) as u32 as i32)
        .collect();
    assert_eq!(sorted, [-3, 0, 7, 42]);
    assert!(f.boundary.crossings().guest_calls >= 3, "the comparator really ran in the guest");
}

// =================================================================== printf

#[test]
fn snprintf_formats_integers_strings_and_doubles_from_a_real_variadic_call() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let fmt = f.cstring(f.guest.data + 0x100, b"[%d] %s %.2f %#x");
    let text = f.cstring(f.guest.data + 0x140, b"omni");
    let double_at = f.guest.data + 0x180;
    f.guest.write_f64(double_at, 1.5);

    let snprintf = f.thunk("snprintf");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(0, out as u64);
    asm.mov(1, 64);
    asm.mov(2, fmt as u64);
    asm.mov(3, u64::from((-7i32) as u32)); // %d, in X3 as a variadic int
    asm.mov(4, text as u64); // %s
    asm.mov(9, double_at as u64);
    asm.push(ldr_d(0, 9, 0)); // %.2f, in V0 -- AAPCS64 puts variadic FP in V0-V7
    asm.mov(5, 0xABC); // %#x
    asm.bl(snprintf);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");

    let written = f.guest.read_u64(f.guest.data) as i64;
    let produced = String::from_utf8(f.read_cstring(out)).expect("ASCII output");
    assert_eq!(produced, "[-7] omni 1.50 0xabc");
    assert_eq!(written, produced.len() as i64, "snprintf returns the length it wrote");
}

/// `snprintf(NULL, 0, ...)` is the documented way to ask how long a result would be, and it must
/// not touch the destination. The return is the **full** length, not the truncated one: a handler
/// that returned the truncated length makes every caller that grows its buffer loop forever.
#[test]
fn snprintf_truncates_but_reports_the_full_length() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    f.guest.write_bytes(out, &[0xEE; 16]);
    let fmt = f.cstring(f.guest.data + 0x100, b"0123456789");

    let measured = value_of(&f, "snprintf", |asm| {
        asm.mov(0, 0); // NULL destination
        asm.mov(1, 0); // capacity 0
        asm.mov(2, fmt as u64);
    }) as i64;
    assert_eq!(measured, 10);
    // Nothing was written.
    assert_eq!(f.guest.read_u64(out), 0xEEEE_EEEE_EEEE_EEEE);

    let truncated = value_of(&f, "snprintf", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 5); // room for four characters and a NUL
        asm.mov(2, fmt as u64);
    }) as i64;
    assert_eq!(truncated, 10, "the length that would have been written");
    assert_eq!(f.read_cstring(out), b"0123");
}

/// A null `%s` argument prints `(null)`, which is what bionic does. It is **not** a refusal: the
/// pointer is never dereferenced, so there is nothing to refuse, and a boundary that rejected it
/// would reject a program bionic runs.
#[test]
fn a_null_string_argument_prints_the_bionic_placeholder() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let fmt = f.cstring(f.guest.data + 0x100, b"<%s>");
    let written = value_of(&f, "snprintf", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 64);
        asm.mov(2, fmt as u64);
        asm.mov(3, 0); // the null const char *
    }) as i64;
    assert_eq!(f.read_cstring(out), b"<(null)>");
    assert_eq!(written, 8);
}

/// `vsnprintf` with a `va_list` the **guest** built, which is the shape AAPCS64 actually uses:
/// the record is 32 bytes, so `X3` holds a pointer to it, not the record.
#[test]
fn vsnprintf_walks_a_va_list_the_guest_built() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x600;
    let fmt = f.cstring(f.guest.data + 0x080, b"%d/%s/%g");
    let text = f.cstring(f.guest.data + 0x0C0, b"mid");
    let gr_save = f.guest.data + 0x100; // X0-X7 as a variadic prologue spilled them
    let vr_save = f.guest.data + 0x200; // Q0-Q7
    let va_list = f.guest.data + 0x300;
    let overflow = f.guest.data + 0x400;
    let double_at = f.guest.data + 0x040;
    f.guest.write_f64(double_at, 0.125);

    let vsnprintf = f.thunk("vsnprintf");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    // The general save area: the %d, then the %s pointer.
    asm.mov(9, u64::from((-11i32) as u32));
    asm.push(str_imm(9, 22, 0x100));
    asm.mov(9, text as u64);
    asm.push(str_imm(9, 22, 0x108));
    // The SIMD save area: one double in Q0's 16-byte slot.
    asm.push(ldr_d(3, 22, 0x040));
    asm.push(str_d(3, 22, 0x200));
    // The va_list record itself.
    asm.mov(9, overflow as u64);
    asm.push(str_imm(9, 22, 0x300)); // __stack
    asm.mov(9, (gr_save + 64) as u64);
    asm.push(str_imm(9, 22, 0x308)); // __gr_top
    asm.mov(9, (vr_save + 128) as u64);
    asm.push(str_imm(9, 22, 0x310)); // __vr_top
    asm.mov(9, u64::from((-64i32) as u32));
    asm.push(str_w(9, 22, 0x318)); // __gr_offs
    asm.mov(9, u64::from((-128i32) as u32));
    asm.push(str_w(9, 22, 0x31C)); // __vr_offs

    asm.mov(0, out as u64);
    asm.mov(1, 64);
    asm.mov(2, fmt as u64);
    asm.mov(3, va_list as u64);
    asm.bl(vsnprintf);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");

    let produced = String::from_utf8(f.read_cstring(out)).expect("ASCII output");
    assert_eq!(produced, "-11/mid/0.125", "read from Q0's 16-byte slot, not from an 8-byte step");
    assert_eq!(f.guest.read_u64(f.guest.data) as i64, produced.len() as i64);
}

// =================================================================== refusals

/// **Task 2 review F6, end to end.** `long double` is a 128-bit quad on Android/LP64 with a
/// 16-byte variadic slot, and nothing in this stack can read one. The refusal names the symbol,
/// the guest address and the conversion — and it happens before any argument is read, so no
/// number is produced out of the wrong bank.
#[test]
fn a_long_double_conversion_is_refused_and_names_itself() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let fmt = f.cstring(f.guest.data + 0x100, b"value: %Lf");
    let error = refusal_of(&f, "snprintf", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 64);
        asm.mov(2, fmt as u64);
    });
    assert_eq!(error.symbol(), Some("snprintf"));
    assert_eq!(error.guest_address(), Some(f.thunk("snprintf")));
    let text = error.to_string();
    assert!(text.contains("%Lf"), "the refusal must name the conversion: {text}");
    assert!(text.contains("long double"), "{text}");
}

/// A symbol this layer does not implement stays `Unbound`, and its call names itself. That is the
/// design, not a gap: a `setjmp` bound to a stub returning a plausible zero would surface three
/// thousand initializers later somewhere unrelated.
///
/// **The symbol here has moved three times, and that churn is the test working.** It was `fopen`
/// until phase 3b bound it, then `socket` until phase 3d bound it — as a refusal, which is a
/// *different* statement from `Unbound`. Every one of the 188 the initializers reach is now
/// serviced, refused by name, placed as a data object or deliberately absent, so the example is
/// now one of the 377 imports **outside** the reachable set: `setjmp`, which `libroblox.so`
/// imports and which only an address-taken edge reaches. Those are exactly the calls
/// `Binding::Unbound` exists for — D17 records 188 as a *lower bound* with 17,698 unresolvable
/// indirect call sites behind it, so a symbol outside the prediction must name itself rather than
/// branch to zero.
#[test]
fn a_symbol_this_phase_does_not_implement_is_unbound_and_says_so() {
    let _guard = serialized();
    let f = fixture();
    let thunk = f.boundary.slot_named("setjmp").map(|s| s.address);
    assert!(thunk.is_none(), "setjmp is outside the reachable 188 and must not be bound");

    // One that *is* declared, because the loader would have asked for it: bind it as the loader
    // would and confirm the call names it.
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(256);
    bionic.bind_into(&builder).expect("bind");
    let unbound = builder.declare_function("setjmp").expect("a slot");
    let boundary = builder.finish();
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.bl(unbound);
    asm.push(ret(21));
    guest.load(asm.words());
    let mut cpu = guest.thread(&boundary);
    let _active = bionic.activate().expect("a thread block");
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("setjmp is not implemented");
    assert!(matches!(error, AbiError::Unbound { .. }), "{error:?}");
    assert_eq!(error.symbol(), Some("setjmp"));
    assert_eq!(error.guest_address(), Some(unbound));
}

/// One symbol counted as an answer, with the argument it answers for.
type Answer<'a> = (&'a str, &'a dyn Fn(&mut Asm));

/// **`fprintf` and `vfprintf` write through a real stream**, which is what M3's gate needed and
/// what HANDOFF called "one binding away" for three phases.
///
/// Asserted on the **bytes in the file**, not on the return value: a handler that returned a
/// plausible length and wrote nothing would satisfy any assertion about the result, and the
/// formatted text is the whole point. The `%d`/`%s` conversions go through the same
/// `format::render` `snprintf` uses, so what is new here is only the destination.
#[test]
fn fprintf_and_vfprintf_write_through_a_real_stream() {
    let _guard = serialized();
    let (f, scratch) = rooted("fprintf");
    let path = f.cstring(f.guest.data + 0x100, b"out.txt");
    let mode = f.cstring(f.guest.data + 0x140, b"w");
    let fmt = f.cstring(f.guest.data + 0x180, b"[%s=%d]");
    let word = f.cstring(f.guest.data + 0x1C0, b"answer");

    let stream = value_of(&f, "fopen", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, mode as u64);
    });
    assert_ne!(stream, 0, "fopen must give a stream to write through");

    let written = value_of(&f, "fprintf", |asm| {
        asm.mov(0, stream);
        asm.mov(1, fmt as u64);
        asm.mov(2, word as u64);
        asm.mov(3, 42);
    });
    assert_eq!(written as i64 as i32, 11, "`[answer=42]` is eleven bytes, and fprintf returns how many it wrote");

    // The `v` form over a guest `va_list` the test builds, which is the half `snprintf` cannot
    // exercise: `X3` holds a *pointer* to the 32-byte record, not the record.
    let va = f.guest.data + 0x200;
    let overflow = f.guest.data + 0x280;
    f.guest.write_u64(overflow, word as u64);
    f.guest.write_u64(overflow + 8, 7);
    f.guest.write_u64(va, overflow as u64); // __stack
    f.guest.write_u64(va + 8, 0); // __gr_top
    f.guest.write_u64(va + 16, 0); // __vr_top
    f.guest.write_u64(va + 24, 0); // __gr_offs / __vr_offs, both exhausted
    let written = value_of(&f, "vfprintf", |asm| {
        asm.mov(0, stream);
        asm.mov(1, fmt as u64);
        asm.mov(2, va as u64);
    });
    assert_eq!(written as i64 as i32, 10, "`[answer=7]` through a va_list");

    assert_eq!(value_of(&f, "fclose", |asm| { asm.mov(0, stream); }) as i64 as i32, 0);
    let contents = std::fs::read(scratch.path("out.txt")).expect("the file the guest wrote");
    assert_eq!(
        String::from_utf8_lossy(&contents),
        "[answer=42][answer=7]",
        "both calls formatted host-side and the bytes reached the file"
    );
}

/// The five printf-family symbols that are *bound* but cannot be serviced refuse by name, and the
/// reason says which missing piece. `Unbound` would have said only "not implemented".
#[test]
fn the_unservable_printf_family_refuses_with_the_missing_piece_named() {
    let _guard = serialized();
    let f = fixture();
    // **`fprintf` and `vfprintf` are no longer here**, and the churn is the test working: phase
    // 3b built the stream layer they named as missing, the refusal text was corrected to say so,
    // and M3's gate bound them onto it. Three are left, and each names something that still does
    // not exist rather than something that does.
    for (symbol, needle) in [
        ("vasprintf", "allocator"),
        ("sscanf", "scanf"),
        ("fscanf", "scanf"),
    ] {
        let error = refusal_of(&f, symbol, |asm| {
            asm.mov(0, 0);
            asm.mov(1, 0);
            asm.mov(2, 0);
        });
        assert_eq!(error.symbol(), Some(symbol));
        assert_eq!(error.guest_address(), Some(f.thunk(symbol)));
        let text = error.to_string();
        assert!(text.contains(needle), "`{symbol}` must say what is missing: {text}");
        assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    }
}

// =================================================================== signals (phase 3c)

/// **`sigfillset` really fills the guest's `sigset_t`, and stops at its end.**
///
/// Asserted on the *bytes*, not on the return value: a handler that returned 0 and wrote nothing
/// would pass any assertion about the result, and the guest would carry an uninitialised set
/// forward. The guard word after the set is what catches a write sized from the wrong constant.
#[test]
fn sigfillset_fills_every_bit_of_the_guests_sigset() {
    let _guard = serialized();
    let f = fixture();
    let set = f.guest.data + 0x200;
    f.guest.write_u64(set, 0);
    f.guest.write_u64(set + 8, 0x1234_5678_9ABC_DEF0);
    let result = value_of(&f, "sigfillset", |asm| {
        asm.mov(0, set as u64);
    });
    assert_eq!(result, 0, "sigfillset returns 0 on success");
    assert_eq!(f.guest.read_u64(set), u64::MAX, "every one of the 64 signal bits");
    assert_eq!(
        f.guest.read_u64(set + 8),
        0x1234_5678_9ABC_DEF0,
        "and nothing past sizeof(sigset_t), which is 8 on LP64"
    );
}

/// A null `set` is bionic's own `-1` with `EINVAL`, read back through `__errno` the way the guest
/// would read it. It is not a refusal: guest code has a branch for this one.
#[test]
fn sigfillset_with_a_null_set_is_minus_one_with_einval() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, 0);
    asm.bl(f.thunk("sigfillset"));
    asm.mov(22, out as u64);
    asm.push(str_imm(0, 22, 0));
    asm.bl(f.thunk("__errno"));
    asm.push(ldr_w(1, 0, 0));
    asm.push(str_imm(1, 22, 8));
    asm.push(ret(21));
    f.guest.load(asm.words());
    let mut cpu = f.guest.thread(&f.boundary);
    assert!(matches!(f.run(&mut cpu, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out) as i64, -1, "sigfillset(NULL) is -1, sign-extended");
    assert_eq!(f.guest.read_u64(out + 8), 22, "EINVAL is Linux's 22");
}

/// A `sigset_t` the guest cannot write to is a typed refusal naming the symbol, not a host
/// access violation and not a silent success.
#[test]
fn sigfillset_on_read_only_guest_memory_is_a_typed_refusal() {
    let _guard = serialized();
    let f = fixture();
    let error = refusal_of(&f, "sigfillset", |asm| {
        asm.mov(0, f.guest.readonly as u64);
    });
    assert_eq!(error.symbol(), Some("sigfillset"));
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
}

/// **The three that cannot be delivered refuse by name, and say what is missing.**
///
/// `Unbound` would have said only "nothing implements this". A refusal says *why*, names the
/// signal number the guest asked about, and — for `raise` — says explicitly that mapping
/// `SIGABRT` onto `abort`'s reported termination was considered and declined. A `0` from any of
/// these is the failure Global Constraint 1 is about: it is not observable until the thing the
/// guest registered for actually happens.
#[test]
fn the_undeliverable_signal_family_refuses_by_name() {
    let _guard = serialized();
    let f = fixture();
    // SIGSEGV(11) for sigaction, SIGABRT(6) for raise, SIG_BLOCK(0) for the mask.
    for (symbol, needles, setup) in [
        (
            "sigaction",
            vec!["SIGSEGV", "no guest signal delivery", "installing a handler"],
            vec![11u64, 0x4000, 0x5000],
        ),
        ("raise", vec!["SIGABRT", "`abort`"], vec![6, 0, 0]),
        ("pthread_sigmask", vec!["SIG_BLOCK", "D19"], vec![0, 0x4000, 0]),
    ] {
        let error = refusal_of(&f, symbol, |asm| {
            for (index, value) in setup.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
        });
        assert_eq!(error.symbol(), Some(symbol));
        assert_eq!(error.guest_address(), Some(f.thunk(symbol)));
        assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
        let text = error.to_string();
        for needle in needles {
            assert!(text.contains(needle), "`{symbol}` must say `{needle}`: {text}");
        }
    }
}

/// `sigaction`'s **query** form is refused too, and says which form it was.
///
/// It is the one that looks answerable — nothing can have installed a handler, so `SIG_DFL` is
/// arithmetically true — and answering it would mean writing a `struct sigaction` whose layout
/// has never been checked against a header on this machine, to describe a table that does not
/// exist.
#[test]
fn the_query_form_of_sigaction_is_refused_and_names_itself_as_a_query() {
    let _guard = serialized();
    let f = fixture();
    let error = refusal_of(&f, "sigaction", |asm| {
        asm.mov(0, 13); // SIGPIPE
        asm.mov(1, 0); // act == NULL: a query
        asm.mov(2, f.guest.data as u64 + 0x400);
    });
    let text = error.to_string();
    assert!(text.contains("querying the current disposition"), "{text}");
    assert!(text.contains("SIGPIPE"), "{text}");
}

/// A handler on a thread with no instance published refuses by name rather than inventing a
/// default state. A per-call default would give two guest threads their own private copy of the
/// same mutex, which no later test can see.
#[test]
fn a_handler_without_an_activation_refuses_rather_than_defaulting() {
    let _guard = serialized();
    let f = fixture();
    let at = f.cstring(f.guest.data + 0x100, b"abc");
    let entry = call_one(&f, "strlen", |asm| {
        asm.mov(0, at as u64);
    });
    let mut cpu = f.guest.thread(&f.boundary);
    // Deliberately no `activate()`.
    let error = f.boundary.run(&mut cpu, entry, BUDGET).expect_err("no bionic state is installed");
    assert!(matches!(error, AbiError::BionicNotActive { .. }), "{error:?}");
    assert_eq!(error.symbol(), Some("strlen"));
    assert_eq!(error.guest_address(), Some(f.thunk("strlen")));
}

// =================================================================== hostile arguments

/// One null-pointer case: the symbol, and the register setup that reaches it.
type NullCase = Box<dyn Fn(&mut Asm)>;

/// A null pointer is the most ordinary thing guest code passes, and every one of these has to be
/// a typed refusal naming the symbol rather than a host access violation.
///
/// **Two independent defences, and the first one wins.** `omni-bionic`'s own `checked_range`
/// rejects a non-empty access at address zero before the pointer ever reaches `omni_mem::admit`,
/// so the refusal is [`AbiError::Refused`] naming the symbol rather than `BadPointer` naming
/// which of `admit`'s rules said no. That is not a weaker answer — it is the crate refusing input
/// it can tell is invalid without asking the address space — and it means a backend that forgot
/// to check would still not turn `memcpy(NULL, x, 16)` into a host store.
#[test]
fn null_pointers_are_refused_by_name_for_every_shape_of_handler() {
    let _guard = serialized();
    let f = fixture();
    let good = f.cstring(f.guest.data + 0x100, b"abc");

    let cases: Vec<(&str, NullCase)> = vec![
        // A string reader.
        (
            "strlen",
            Box::new(|asm: &mut Asm| {
                asm.mov(0, 0);
            }),
        ),
        // A two-pointer comparison: the null is the *second* argument.
        (
            "strcmp",
            Box::new(move |asm: &mut Asm| {
                asm.mov(0, good as u64);
                asm.mov(1, 0);
            }),
        ),
        // A writer.
        (
            "memset",
            Box::new(|asm: &mut Asm| {
                asm.mov(0, 0);
                asm.mov(1, 0);
                asm.mov(2, 16);
            }),
        ),
        // A copy with a real source and a null destination.
        (
            "memcpy",
            Box::new(move |asm: &mut Asm| {
                asm.mov(0, 0);
                asm.mov(1, good as u64);
                asm.mov(2, 16);
            }),
        ),
        // A number parser, which goes through the context rather than plain memory.
        (
            "strtol",
            Box::new(|asm: &mut Asm| {
                asm.mov(0, 0);
                asm.mov(1, 0);
                asm.mov(2, 10);
            }),
        ),
        // A synchronization primitive, whose first access is a compare-and-swap.
        (
            "pthread_mutex_lock",
            Box::new(|asm: &mut Asm| {
                asm.mov(0, 0);
            }),
        ),
    ];
    for (symbol, setup) in cases {
        let error = refusal_of(&f, symbol, |asm| setup(asm));
        assert_eq!(error.symbol(), Some(symbol), "{error:?}");
        assert_eq!(error.guest_address(), Some(f.thunk(symbol)), "{error:?}");
        assert!(
            matches!(error, AbiError::Refused { .. } | AbiError::BadPointer { .. }),
            "`{symbol}` with a null pointer must be a typed refusal: {error:?}"
        );
    }

    // A null format string. bionic's own printf crashes here; a refusal is the only other honest
    // answer, and inventing "(null)" would be an invention.
    let error = refusal_of(&f, "snprintf", |asm| {
        asm.mov(0, f.guest.data as u64 + 0x300);
        asm.mov(1, 64);
        asm.mov(2, 0);
    });
    assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    assert!(error.to_string().contains("null format"), "{error}");

    // **`memcpy(NULL, NULL, 0)` is legal C and must NOT be refused.** The over-correction: a
    // boundary that rejected every null pointer would reject a correct program.
    let returned = value_of(&f, "memcpy", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
    });
    assert_eq!(returned, 0, "a zero-length memcpy at null is defined behaviour and returns dst");
}

/// A pointer into address space nothing has mapped, and a length that runs off the end of a
/// mapping that *is* there. Both are refused before any host byte is touched.
#[test]
fn wild_pointers_and_lying_lengths_are_refused() {
    let _guard = serialized();
    let f = fixture();
    let unmapped = f.guest.unmapped;

    let error = refusal_of(&f, "strlen", |asm| {
        asm.mov(0, unmapped as u64);
    });
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");

    // A length that starts inside a mapping and ends outside it.
    let error = refusal_of(&f, "memcpy", |asm| {
        asm.mov(0, f.guest.data as u64);
        asm.mov(1, f.guest.data as u64 + 0x100);
        asm.mov(2, 1 << 40);
    });
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");

    // A length that would wrap the 64-bit address space.
    let error = refusal_of(&f, "memcmp", |asm| {
        asm.mov(0, f.guest.data as u64 + 0x100);
        asm.mov(1, f.guest.data as u64 + 0x200);
        asm.mov(2, u64::MAX);
    });
    assert!(
        matches!(error, AbiError::BadPointer { .. } | AbiError::Refused { .. }),
        "{error:?}"
    );
}

/// A guest's own read-only pages handed over as an output buffer. This is refused on the
/// *protection* rule, which is the distinction that matters: the address is mapped, and a handler
/// that only checked "is it mapped" would take a host access violation on the store.
#[test]
fn a_read_only_destination_is_refused_for_writing() {
    let _guard = serialized();
    let f = fixture();
    let src = f.cstring(f.guest.data + 0x100, b"source");
    let error = refusal_of(&f, "strcpy", |asm| {
        asm.mov(0, f.guest.readonly as u64);
        asm.mov(1, src as u64);
    });
    match error {
        AbiError::BadPointer { access, .. } => assert_eq!(access, "writing"),
        other => panic!("{other:?}"),
    }
}

/// A string with no NUL in it, in a mapping with nothing after it.
///
/// **What bounds this walk is the mapping, not a cap**, and the test says so because the
/// difference matters. `omni-bionic`'s `strlen` reads one byte at a time until it finds a NUL or
/// faults, exactly as the real one does, so it will happily walk from one mapping into an
/// adjacent one — the first attempt at this test did, into the read-only page the harness maps
/// next door, and returned 4096. The refusal arrives only where the address space runs out.
///
/// The consequence, stated rather than fixed: a guest that passes an unterminated pointer into a
/// large mapped region makes one handler scan that whole region, one `admit` per byte. It
/// terminates and it cannot abort, but it is unbounded work chosen by the guest. The FORTIFY form
/// below is the bounded one, and `GuestMem::cstr` — which the `printf` family uses — has the
/// boundary's own 64 KiB `STRING_LIMIT`.
#[test]
fn an_unterminated_string_faults_at_the_end_of_its_mapping() {
    use omni_mem::{CommitPolicy, Placement, Protection};
    let _guard = serialized();
    let f = fixture();
    // A mapping placed in free address space, with the space after it left free, so that the
    // walk has somewhere to run out rather than somewhere to continue.
    let page = f.guest.space.page_size();
    let island = f
        .guest
        .space
        .map_anonymous(
            Placement::Fixed(f.guest.unmapped & !(page - 1)),
            page,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("an island mapping");
    assert!(
        f.guest.space.region_at(island + page).is_none_or(|r| r.is_free()),
        "the test needs free space after the island, or the walk would continue into it"
    );
    f.guest.write_bytes(island, &vec![0x41u8; page]);

    let error = refusal_of(&f, "strlen", |asm| {
        asm.mov(0, island as u64);
    });
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    assert_eq!(error.symbol(), Some("strlen"));

    // The FORTIFY form is the one with a bound, and it reports the overflow it detected rather
    // than a pointer problem: the string is longer than the object it is supposed to be in.
    let error = refusal_of(&f, "__strlen_chk", |asm| {
        asm.mov(0, island as u64);
        asm.mov(1, 16);
    });
    let text = error.to_string();
    assert!(text.contains("__strlen_chk"), "{text}");
}

/// **A compare-and-swap on an unaligned guest address is a refusal, not a slow path.** A 32-bit
/// atomic on an unaligned address is undefined behaviour in Rust, and on the guest's own hardware
/// `LDXR`/`STXR` take an alignment fault there too — so refusing is what the guest would see on a
/// real device, and it is the only answer here that is not undefined behaviour.
#[test]
fn an_unaligned_mutex_word_is_refused_rather_than_atomically_accessed() {
    let _guard = serialized();
    let f = fixture();
    let misaligned = f.guest.data + 0x201; // deliberately odd
    f.guest.write_bytes(f.guest.data + 0x200, &[0u8; 48]);
    let error = refusal_of(&f, "pthread_mutex_lock", |asm| {
        asm.mov(0, misaligned as u64);
    });
    let text = error.to_string();
    assert!(text.contains("align"), "the refusal must say what is wrong: {text}");
    assert_eq!(error.symbol(), Some("pthread_mutex_lock"));

    // **The over-correction check, and the address is chosen for it.** `0x204` is 4-byte aligned
    // and *not* 8-byte aligned, which is legal for a `pthread_mutex_t` — bionic's holds an `int`.
    // A check tightened to 8 bytes would look correct and would refuse a mutex a real program
    // has, and `data + 0x200` would not have caught it.
    let aligned = f.guest.data + 0x204;
    f.guest.write_bytes(aligned, &[0u8; 40]);
    let code = value_of(&f, "pthread_mutex_lock", |asm| {
        asm.mov(0, aligned as u64);
    });
    assert_eq!(code, 0, "a 4-byte-aligned mutex must still lock");
}

/// A `va_list` whose offsets are outside what an AArch64 register save area can have. Both
/// fields are guest-written, so both are range-checked, and the refusal names which one.
#[test]
fn a_hostile_va_list_is_refused_with_the_field_named() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x600;
    let fmt = f.cstring(f.guest.data + 0x080, b"%d");
    let va_list = f.guest.data + 0x300;

    let vsnprintf = f.thunk("vsnprintf");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(9, f.guest.data as u64 + 0x400);
    asm.push(str_imm(9, 22, 0x300)); // __stack
    asm.mov(9, f.guest.data as u64 + 0x140);
    asm.push(str_imm(9, 22, 0x308)); // __gr_top
    asm.push(str_imm(9, 22, 0x310)); // __vr_top
    asm.mov(9, u64::from((-2_000_000i32) as u32));
    asm.push(str_w(9, 22, 0x318)); // __gr_offs: far outside -64..=0
    asm.mov(9, 0);
    asm.push(str_w(9, 22, 0x31C));
    asm.mov(0, out as u64);
    asm.mov(1, 64);
    asm.mov(2, fmt as u64);
    asm.mov(3, va_list as u64);
    asm.bl(vsnprintf);
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    let error = f.boundary_run_err(&mut cpu, entry);
    match error {
        AbiError::BadVaList { field, .. } => assert_eq!(field, "__gr_offs"),
        other => panic!("{other:?}"),
    }
}

impl Fixture {
    fn boundary_run_err(&self, cpu: &mut dyn GuestCpu, entry: omni_cpu::GuestAddr) -> AbiError {
        let _active = self.bionic.activate().expect("a thread block");
        self.boundary.run(cpu, entry, BUDGET).expect_err("this program must be refused")
    }
}

/// **A guest-chosen field width is an allocation the guest picked**, and the one that aborts is
/// the one that is not caught. Refused, with nothing written to the destination.
#[test]
fn a_hostile_printf_width_is_refused_and_writes_nothing() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    f.guest.write_bytes(out, &[0xEE; 16]);
    let fmt = f.cstring(f.guest.data + 0x100, b"%999999999999999999999d");
    let error = refusal_of(&f, "snprintf", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 64);
        asm.mov(2, fmt as u64);
        asm.mov(3, 1);
    });
    assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    assert!(error.to_string().contains("wide"), "{error}");
    assert_eq!(f.guest.read_u64(out), 0xEEEE_EEEE_EEEE_EEEE, "nothing was written");
}

/// `%n` writes through a pointer the format string names, and is the classic format-string
/// exploit primitive. bionic does not support it; neither does this, and it is refused by name.
#[test]
fn percent_n_is_refused() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let fmt = f.cstring(f.guest.data + 0x100, b"abc%n");
    let error = refusal_of(&f, "snprintf", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 64);
        asm.mov(2, fmt as u64);
        asm.mov(3, f.guest.data as u64 + 0x200);
    });
    assert!(error.to_string().contains("%n"), "{error}");
}

/// The FORTIFY refusal: `__vsnprintf_chk` is told both what the caller passed and what the
/// compiler could prove, and a caller passing more than the destination holds is a **detected
/// buffer overflow in guest code**. bionic answers it with `__fortify_fatal`.
#[test]
fn a_fortify_check_that_fires_is_reported_as_the_overflow_it_is() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x600;
    let fmt = f.cstring(f.guest.data + 0x080, b"x");
    let va_list = f.guest.data + 0x300;

    let error = refusal_of(&f, "__vsnprintf_chk", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 64); // supplied size
        asm.mov(2, 0); // flags
        asm.mov(3, 8); // what the compiler proved: eight bytes
        asm.mov(4, fmt as u64);
        asm.mov(5, va_list as u64);
    });
    let text = error.to_string();
    assert!(text.contains("FORTIFY"), "{text}");
    assert!(text.contains("64") && text.contains('8'), "both sizes must be named: {text}");
}

// =================================================================== the arena's bound

/// The 65th guest thread is a refusal, not a second thread sharing the 1st one's `errno` slot.
///
/// Sharing would be silent: two threads would see each other's `errno` and each other's
/// `strerror` buffer, and the only symptom would be an occasional wrong error number.
#[test]
fn the_thread_arena_refuses_rather_than_sharing_a_block() {
    use omni_android::bionic::MAX_GUEST_THREADS;
    let _guard = serialized();
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");

    // Two barriers, because the order matters and a race here would make the test flaky in the
    // direction that hides the bug: `ready` is passed only once all 64 have a block, and `done`
    // keeps every one of them holding it until this thread has made its attempt. A single
    // barrier would let this thread take a slot first and a spawned thread take the refusal.
    let ready = Arc::new(std::sync::Barrier::new(MAX_GUEST_THREADS + 1));
    let done = Arc::new(std::sync::Barrier::new(MAX_GUEST_THREADS + 1));
    let mut handles = Vec::new();
    for _ in 0..MAX_GUEST_THREADS {
        let bionic = Arc::clone(&bionic);
        let ready = Arc::clone(&ready);
        let done = Arc::clone(&done);
        handles.push(std::thread::spawn(move || {
            let active = bionic.activate().expect("a block for each of the first 64");
            ready.wait();
            done.wait();
            drop(active);
        }));
    }
    ready.wait();
    // The 65th, from this thread, with all 64 blocks held.
    let overflowed = bionic.activate();
    done.wait();
    for handle in handles {
        handle.join().expect("each thread finishes");
    }

    match overflowed {
        Err(AbiError::Refused { why, .. }) => {
            assert!(why.contains("errno"), "the refusal must say what would be shared: {why}");
        }
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("the 65th thread was given a block, so two threads share an errno slot"),
    }
    assert_eq!(bionic.attached(), MAX_GUEST_THREADS);
}

/// Every attached thread gets its **own** block, and the blocks do not overlap. One `errno` slot
/// shared between two threads is the failure this arena exists to prevent.
#[test]
fn each_thread_gets_a_distinct_block() {
    let _guard = serialized();
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let bionic = Arc::clone(&bionic);
        let seen = Arc::clone(&seen);
        handles.push(std::thread::spawn(move || {
            let _active = bionic.activate().expect("a block");
            let id = bionic.current_thread().expect("an identity");
            seen.lock().unwrap().push(id);
        }));
    }
    for handle in handles {
        handle.join().expect("each thread finishes");
    }
    let ids = seen.lock().unwrap().clone();
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 8, "eight threads, eight distinct pthread_t values: {ids:?}");
    assert!(ids.iter().all(|id| id.0 != 0), "no thread may get the reserved zero");
}

/// The clock is wired even though nothing this phase binds reads it: `pthread_cond_timedwait` and
/// the timed lock forms are its callers and they are Tier C. Exercised here so the next phase
/// finds a capability that works rather than one that was never run.
#[test]
fn the_clock_is_monotonic_and_has_a_wall_clock_beside_it() {
    use omni_bionic::threads::Clock;
    let _guard = serialized();
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let first = bionic.clock().now_monotonic();
    let second = bionic.clock().now_monotonic();
    assert!(second >= first, "CLOCK_MONOTONIC must never go backwards");
    // 2020-01-01 in seconds since the epoch. A wall clock reading below it is a host whose clock
    // is not set, which is worth knowing about rather than asserting a range around "now".
    assert!(bionic.clock().now_realtime().as_secs() > 1_577_836_800);
}

/// `rand` is `omni-bionic`'s LCG and is **not** bit-exact with bionic's. Pinned here so the
/// sequence is a stated fact rather than an accident, and so that anything which later starts
/// depending on bionic's exact stream fails visibly instead of drifting.
#[test]
fn rand_produces_the_documented_lcg_sequence_which_is_not_bionics() {
    let _guard = serialized();
    let f = fixture();
    let first = value_of(&f, "rand", |_| {}) as i64;
    let second = value_of(&f, "rand", |_| {}) as i64;
    assert_eq!(
        (first, second),
        (1_103_527_590, 377_401_575),
        "the LCG from state 1; NOT bionic's sequence, and nothing in the engine may check it \
         against bionic's"
    );
    assert!(first >= 0 && second >= 0, "rand returns [0, RAND_MAX], never a negative int");
}

// ================================================= phase 2: data symbols, dl*, guest memory

use omni_android::bionic::{GuestProcess, DATA_OBJECTS, DL_PHDR_INFO_BYTES, FILE_BYTES};
use omni_elf::loader::DlPhdrInfo;

/// One synthetic loaded image, as a host would describe a real one.
struct Image {
    name: &'static str,
    addr: omni_cpu::GuestAddr,
    phdr: omni_cpu::GuestAddr,
    phnum: u16,
}

/// A fixture with the handlers bound **and** the eighteen data objects placed, plus whatever
/// images the test wants `dl_iterate_phdr` to enumerate.
///
/// The stack canary comes from the backend's own TLS arena rather than from a number this file
/// chose, which is the whole point of `__stack_chk_guard`: a function that loads the global must
/// see what `[TPIDR_EL0, #0x28]` holds (D13).
fn fixture_with(images: &[Image]) -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(256);
    bionic.bind_into(&builder).expect("bind every handler");
    bionic.set_log_to_stderr(false);
    let stack_guard = guest.backend.tls().stack_guard();
    bionic
        .declare_data_into(&builder, &GuestProcess { stack_guard })
        .expect("declare and fill the eighteen data objects");
    for image in images {
        bionic
            .register_image(&DlPhdrInfo {
                name: image.name.to_string(),
                addr: image.addr,
                phdr: image.phdr,
                phnum: image.phnum,
            })
            .expect("register an image");
    }
    let boundary = builder.finish();
    Fixture { guest, bionic, boundary }
}

/// Assemble a program with `X21` holding the caller's return address, and run it.
fn program(f: &Fixture, build: impl FnOnce(&mut Asm)) -> omni_cpu::GuestAddr {
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    build(&mut asm);
    asm.push(ret(21));
    f.guest.load(asm.words());
    entry
}

fn run_program(f: &Fixture, entry: omni_cpu::GuestAddr) -> Result<ExitReason, AbiError> {
    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry)
}

// ------------------------------------------------------------------ the data symbols

/// **The eighteen are placed, sized and filled**, read back through the boundary's own memory.
///
/// Asserted on contents rather than on addresses, because an address proves only that
/// `declare_data` was called and every one of these has a value guest code will act on.
#[test]
fn the_data_objects_hold_the_values_the_guest_will_read() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let guard = f.guest.backend.tls().stack_guard();

    // `__stack_chk_guard` must be the canary D13 programmed, or a stack-protected function that
    // loads the global form and one that loads `[TPIDR_EL0, #0x28]` disagree — and the second
    // kind is 1,276 of `libroblox.so`'s 1,282 thread-pointer reads.
    assert_ne!(guard, 0);
    assert_eq!(f.guest.read_u64(f.thunk("__stack_chk_guard")), guard);

    // `stdin`/`stdout`/`stderr` are `FILE *` into `__sF`, one `FILE` apart.
    let sf = f.thunk("__sF");
    for (index, symbol) in ["stdin", "stdout", "stderr"].into_iter().enumerate() {
        assert_eq!(
            f.guest.read_u64(f.thunk(symbol)) as usize,
            sf + index * FILE_BYTES,
            "`{symbol}` must point at __sF[{index}]"
        );
    }
    // And the three do not overlap: the object is three `FILE`s wide.
    let sf_object = DATA_OBJECTS.iter().find(|o| o.symbol == "__sF").expect("__sF");
    assert_eq!(sf_object.len, 3 * FILE_BYTES);

    // `environ` points at a vector whose first entry is the terminating null: an empty
    // environment, which is a fact about this process rather than a placeholder. A null
    // `environ` would be the wrong answer — POSIX-shaped code walks it without checking.
    let vector = f.guest.read_u64(f.thunk("environ")) as usize;
    assert_ne!(vector, 0, "environ itself must not be null");
    assert_eq!(f.guest.read_u64(vector), 0, "the vector is one terminating null");

    // `in6addr_any` is `::` and `in6addr_loopback` is `::1`.
    let any = f.thunk("in6addr_any");
    assert_eq!(f.guest.read_u64(any), 0);
    assert_eq!(f.guest.read_u64(any + 8), 0);
    let loopback = f.thunk("in6addr_loopback");
    assert_eq!(f.guest.read_u64(loopback), 0);
    assert_eq!(
        f.guest.read_u64(loopback + 8).to_be(),
        1,
        "::1 is fifteen zero bytes and then a one, in network order"
    );
    assert_ne!(f.guest.read_u64(loopback + 8), f.guest.read_u64(any + 8));

    // Every `AMEDIAFORMAT_KEY_*` points at a distinct non-empty string.
    let mut keys = std::collections::BTreeSet::new();
    for object in DATA_OBJECTS.iter().filter(|o| o.symbol.starts_with("AMEDIAFORMAT_KEY_")) {
        let string = f.guest.read_u64(f.thunk(object.symbol)) as usize;
        assert_ne!(string, 0, "`{}` must not be null: the engine strcmps it", object.symbol);
        let text = f.read_cstring(string);
        assert!(!text.is_empty(), "`{}` points at an empty string", object.symbol);
        assert!(keys.insert(text.clone()), "two keys share {:?}", String::from_utf8_lossy(&text));
    }
    assert_eq!(keys.len(), 10);
    assert_eq!(f.guest.read_u64(f.thunk("AMEDIAFORMAT_KEY_MIME")) as usize, {
        let at = f.guest.read_u64(f.thunk("AMEDIAFORMAT_KEY_MIME")) as usize;
        assert_eq!(f.read_cstring(at), b"mime");
        at
    });
}

/// A zero canary compares equal to a zeroed stack slot, so a guest stack overflow that wrote
/// zeroes would pass every `__stack_chk_fail` check. `omni-cpu` refuses to *generate* one; this
/// refuses to *store* one, and the two refusals have to agree or the global and the TLS copy
/// diverge in the one case that matters.
#[test]
fn a_zero_stack_canary_is_refused_rather_than_stored() {
    let _guard = serialized();
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(256);
    let error = bionic
        .declare_data_into(&builder, &GuestProcess { stack_guard: 0 })
        .expect_err("a zero canary must be refused");
    assert_eq!(error.symbol(), Some("__stack_chk_guard"));
    assert!(error.to_string().contains("zero"), "{error}");
}

/// **A data symbol that is *called* is still `DataSymbolCalled`.** The eighteen are addresses to
/// load from; executing whatever `__sF` holds is the one response that must not happen, and now
/// that the objects have contents there is something there to execute.
#[test]
fn calling_a_filled_data_symbol_is_still_refused_by_name() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    for symbol in ["__sF", "environ", "AMEDIAFORMAT_KEY_MIME"] {
        let target = f.thunk(symbol);
        // `BLR` rather than `BL`: the data area is nowhere near the code region and a `BL`
        // displacement is +/-128 MB.
        let entry = program(&f, |asm| {
            asm.mov(9, target as u64);
            asm.push(blr(9));
        });
        match run_program(&f, entry) {
            Err(AbiError::DataSymbolCalled { symbol: named, address }) => {
                assert_eq!(named, symbol);
                assert_eq!(address, target);
            }
            other => panic!("`{symbol}`: {other:?}"),
        }
    }
}

// ------------------------------------------------------------------ dl_iterate_phdr

/// Bytes of one record the guest callback writes out.
const RECORD_BYTES: usize = 48;

/// A guest `int (*)(struct dl_phdr_info *, size_t, void *)` that copies six fields of every
/// object it is handed into a cursor the third argument points at, and returns `answer`.
///
/// **This is what makes the test about the struct layout rather than about the handler.** The
/// callback reads `dlpi_addr` at `+0`, `dlpi_name` at `+8`, `dlpi_phdr` at `+16`, `dlpi_phnum` at
/// `+24` and `dlpi_adds` at `+32` with real `LDR` instructions, exactly as a guest unwinder does.
/// A handler that wrote the fields in the wrong order would put the name where the bias belongs
/// and this would see it.
fn dl_callback(guest: &Guest, answer: i64) -> omni_cpu::GuestAddr {
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(ldr_imm(3, 2, 0)); // X3 = cursor
    asm.push(str_imm(1, 3, 0)); // the `size` argument
    asm.push(ldr_imm(4, 0, 0));
    asm.push(str_imm(4, 3, 8)); // dlpi_addr
    asm.push(ldr_imm(4, 0, 8));
    asm.push(str_imm(4, 3, 16)); // dlpi_name
    asm.push(ldr_imm(4, 0, 16));
    asm.push(str_imm(4, 3, 24)); // dlpi_phdr
    asm.push(ldr_w(4, 0, 24));
    asm.push(str_imm(4, 3, 32)); // dlpi_phnum, zero-extended
    asm.push(ldr_imm(4, 0, 32));
    asm.push(str_imm(4, 3, 40)); // dlpi_adds
    asm.push(add_imm(3, 3, RECORD_BYTES as u32));
    asm.push(str_imm(3, 2, 0));
    asm.mov(0, answer as u64);
    asm.push(ret(30));
    guest.load(asm.words())
}

/// `dl_iterate_phdr` enumerates every registered image, in registration order, with the fields
/// where AArch64 bionic puts them.
#[test]
fn dl_iterate_phdr_enumerates_the_real_loaded_image() {
    let _guard = serialized();
    let images = [
        Image { name: "libroblox.so", addr: 0x1234_0000, phdr: 0x1234_0040, phnum: 9 },
        Image { name: "libzstd-jni.so", addr: 0x5000_0000, phdr: 0x5000_0040, phnum: 7 },
    ];
    let f = fixture_with(&images);

    let cursor = f.guest.data + 0x400;
    let records = f.guest.data + 0x800;
    f.guest.write_u64(cursor, records as u64);
    let callback = dl_callback(&f.guest, 0);

    let returned = value_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, callback as u64);
        asm.mov(1, cursor as u64);
    });
    assert_eq!(returned, 0, "every callback returned zero, so the walk completes and returns zero");
    assert_eq!(
        f.guest.read_u64(cursor) as usize,
        records + images.len() * RECORD_BYTES,
        "the callback must have run once per registered image"
    );

    for (index, image) in images.iter().enumerate() {
        let at = records + index * RECORD_BYTES;
        assert_eq!(
            f.guest.read_u64(at) as usize,
            DL_PHDR_INFO_BYTES,
            "the `size` argument is sizeof(struct dl_phdr_info)"
        );
        assert_eq!(f.guest.read_u64(at + 8) as usize, image.addr, "dlpi_addr is the load bias");
        let name = f.guest.read_u64(at + 16) as usize;
        assert_ne!(name, 0, "dlpi_name is a pointer the callback dereferences");
        assert_eq!(f.read_cstring(name), image.name.as_bytes());
        assert_eq!(f.guest.read_u64(at + 24) as usize, image.phdr, "dlpi_phdr");
        assert_eq!(f.guest.read_u64(at + 32), u64::from(image.phnum), "dlpi_phnum");
        assert_eq!(
            f.guest.read_u64(at + 40),
            images.len() as u64,
            "dlpi_adds is how many objects have ever been added; nothing here can dlopen"
        );
    }
    // The walk really left the run loop once per object plus once for the call itself.
    assert!(f.boundary.crossings().guest_calls >= images.len() as u64);
}

/// A callback that answers non-zero stops the walk and its value is returned — which is the
/// contract the unwinder relies on: it answers non-zero the moment it finds the object holding
/// the address it is looking for.
#[test]
fn a_callback_that_answers_non_zero_stops_the_walk() {
    let _guard = serialized();
    let images = [
        Image { name: "first.so", addr: 0x1000_0000, phdr: 0x1000_0040, phnum: 4 },
        Image { name: "second.so", addr: 0x2000_0000, phdr: 0x2000_0040, phnum: 4 },
        Image { name: "third.so", addr: 0x3000_0000, phdr: 0x3000_0040, phnum: 4 },
    ];
    let f = fixture_with(&images);
    let cursor = f.guest.data + 0x400;
    let records = f.guest.data + 0x800;
    f.guest.write_u64(cursor, records as u64);
    let callback = dl_callback(&f.guest, 7);

    let returned = value_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, callback as u64);
        asm.mov(1, cursor as u64);
    });
    assert_eq!(returned as i64 as i32, 7, "the callback's own answer is returned");
    assert_eq!(
        f.guest.read_u64(cursor) as usize,
        records + RECORD_BYTES,
        "exactly one object was reported before the walk stopped"
    );
    assert_eq!(f.guest.read_u64(records + 8) as usize, images[0].addr, "and it was the first");
}

/// **The refusal that keeps `dl_iterate_phdr` from becoming a stub.**
///
/// Reporting a process with no objects is a *success*: the call returns zero, which is what it
/// returns when every callback declined. The guest's statically-linked unwinder would then find
/// no `.eh_frame` and every `throw` would fail to find a landing pad, thousands of initializers
/// from here. So an adapter with nothing registered refuses and says what to call.
#[test]
fn dl_iterate_phdr_with_no_registered_image_refuses_rather_than_reporting_an_empty_process() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let callback = dl_callback(&f.guest, 0);
    let cursor = f.guest.data + 0x400;
    let error = refusal_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, callback as u64);
        asm.mov(1, cursor as u64);
    });
    assert_eq!(error.symbol(), Some("dl_iterate_phdr"));
    let text = error.to_string();
    assert!(text.contains("register_image"), "the refusal must say what to call: {text}");
    assert!(text.contains("eh_frame"), "and why it matters: {text}");
}

/// Hostile: a null callback, and a callback pointing at memory nothing has mapped. Neither may
/// panic, and the two are told apart — one is a refusal, the other is a stopped guest callback.
#[test]
fn a_hostile_dl_iterate_phdr_callback_is_a_typed_error_and_not_a_panic() {
    let _guard = serialized();
    let images = [Image { name: "only.so", addr: 0x1000_0000, phdr: 0x1000_0040, phnum: 4 }];
    let f = fixture_with(&images);
    let cursor = f.guest.data + 0x400;
    f.guest.write_u64(cursor, (f.guest.data + 0x800) as u64);

    let null = refusal_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, 0);
        asm.mov(1, cursor as u64);
    });
    assert!(matches!(null, AbiError::Refused { .. }), "{null:?}");
    assert!(null.to_string().contains("0x0"), "{null}");

    let wild = refusal_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, f.guest.unmapped as u64);
        asm.mov(1, cursor as u64);
    });
    assert!(
        matches!(wild, AbiError::GuestCallbackStopped { .. }),
        "an unmapped callback is the guest's own fault and is reported as one: {wild:?}"
    );

    // And a `data` pointer the callback will fault on is the callback's failure too, not ours.
    // Assembled first: `call_one` fixes its own entry address before the setup closure runs, so a
    // closure that loaded another program would move the code out from under its own branches.
    let callback = dl_callback(&f.guest, 0);
    let bad_data = refusal_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, callback as u64);
        asm.mov(1, f.guest.unmapped as u64);
    });
    assert!(matches!(bad_data, AbiError::GuestCallbackStopped { .. }), "{bad_data:?}");
}

// ------------------------------------------------------------------ dlopen and friends

/// **`dlopen` and `dlsym` answer for the libraries this layer *is*, and refuse to load a file.**
///
/// Phase 2 refused all three, on the argument that a plausible handle is worse than a refusal.
/// That is right about a handle to a **file** and wrong about the case M3's gate found at
/// `init_array[3096]`: `dlopen("libc.so")` then `dlsym(h, "getauxval")` then `f(AT_HWCAP)` — the
/// engine's atomics feature detection. `libc.so` is already loaded on a device and `dlopen` of it
/// is a lookup; here, this layer *is* `libc.so`.
///
/// The scope is the **guest's own** `DT_VERNEED`, not a list here, which is why `libc.so` finds
/// `getauxval` and a handle for a library the guest does not attribute it to does not.
#[test]
fn the_dl_family_answers_for_the_libraries_this_layer_supplies() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    // The fixture's boundary was built by binding handlers, not by a loader, so no slot carries a
    // `DT_VERNEED` library. That is the honest state for a synthetic guest: `dlopen` of a named
    // library answers NULL because this boundary supplies none, and the real-library assertions
    // live in `tests/libroblox.rs` where the loader has actually attributed the imports.
    let name = f.cstring(f.guest.data + 0x100, b"libvulkan.so");
    let wanted = f.cstring(f.guest.data + 0x180, b"strlen");

    // `dlopen(NULL)` is the global scope, and it is a real handle.
    let global = value_of(&f, "dlopen", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 2); // RTLD_NOW
    });
    assert_ne!(global, 0, "dlopen(NULL) is the global scope and always exists");

    // A library this runtime does not have is **NULL with `dlerror` set**, not a refusal: NULL is
    // the true answer, and it is the one the compiler emitted a null test for.
    assert_eq!(
        value_of(&f, "dlopen", |asm| {
            asm.mov(0, name as u64);
            asm.mov(1, 2);
        }),
        0,
        "a library this runtime does not supply is NULL"
    );
    let message = f.read_cstring(value_of(&f, "dlerror", |_| {}) as omni_cpu::GuestAddr);
    let message = String::from_utf8_lossy(&message).into_owned();
    assert!(message.contains("libvulkan.so"), "dlerror must name it: {message}");
    // Cleared by reading, as bionic's is: the idiom `dlerror(); p = dlsym(..); if (dlerror())`
    // depends on both halves.
    assert_eq!(value_of(&f, "dlerror", |_| {}), 0, "dlerror is cleared by reading");

    // `dlsym` in the global scope finds what this layer supplies, and the address it returns is a
    // thunk slot — an address the guest can branch to, which is what makes this an implementation.
    let found = value_of(&f, "dlsym", |asm| {
        asm.mov(0, global);
        asm.mov(1, wanted as u64);
    });
    assert_eq!(
        found as omni_cpu::GuestAddr,
        f.thunk("strlen"),
        "dlsym returns the symbol's thunk address"
    );
    // A symbol nothing supplies is NULL with `dlerror` set, which is `dlsym`'s ordinary answer.
    let absent = f.cstring(f.guest.data + 0x200, b"vkGetInstanceProcAddr");
    assert_eq!(
        value_of(&f, "dlsym", |asm| {
            asm.mov(0, global);
            asm.mov(1, absent as u64);
        }),
        0
    );
    let message = f.read_cstring(value_of(&f, "dlerror", |_| {}) as omni_cpu::GuestAddr);
    assert!(
        String::from_utf8_lossy(&message).contains("vkGetInstanceProcAddr"),
        "dlerror must name the symbol"
    );

    // `dlclose` of a real handle succeeds: nothing was loaded, so nothing is unloaded, and the
    // libraries this layer *is* cannot be unloaded — which is also true of `libc.so` on a device.
    assert_eq!(value_of(&f, "dlclose", |asm| { asm.mov(0, global); }), 0);
}

/// **A handle this layer never issued is a refusal, not NULL** — and a `dlsym` miss is not a
/// refusal.
///
/// This is where phase 2's argument survives intact. NULL from `dlsym` means "no such symbol",
/// which guest code routinely treats as an absent optional capability; answering it for a handle
/// that came from somewhere else would let a wrong *handle* be read as a missing *feature*.
#[test]
fn a_dlsym_on_a_handle_this_layer_never_issued_refuses() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let wanted = f.cstring(f.guest.data + 0x180, b"strlen");

    let sym = refusal_of(&f, "dlsym", |asm| {
        asm.mov(0, 0x1234);
        asm.mov(1, wanted as u64);
    });
    assert_eq!(sym.symbol(), Some("dlsym"));
    assert!(sym.to_string().contains("0x1234"), "{sym}");

    let close = refusal_of(&f, "dlclose", |asm| {
        asm.mov(0, 0x1234);
    });
    assert_eq!(close.symbol(), Some("dlclose"));
}

/// Hostile: an unterminated `dlopen` name, a wild pointer, and an unterminated `dlsym` name.
///
/// None is a panic and none is a refusal: an unreadable name is a *failed lookup*, which is what
/// the kernel-side of a real `dlopen` produces for a path it cannot read, and `dlerror` carries
/// the description.
#[test]
fn a_hostile_dl_argument_is_described_rather_than_crashing() {
    let _guard = serialized();
    let f = fixture_with(&[]);

    assert_eq!(
        value_of(&f, "dlopen", |asm| {
            asm.mov(0, f.guest.unmapped as u64);
            asm.mov(1, 2);
        }),
        0,
        "a wild name pointer is a failed dlopen, not a crash"
    );
    let message = f.read_cstring(value_of(&f, "dlerror", |_| {}) as omni_cpu::GuestAddr);
    assert!(
        String::from_utf8_lossy(&message).contains("not a readable string"),
        "dlerror must describe it"
    );

    // A string with no NUL anywhere in its region.
    let island = f.guest.readonly;
    let global = value_of(&f, "dlopen", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 2);
    });
    assert_eq!(
        value_of(&f, "dlsym", |asm| {
            asm.mov(0, global);
            asm.mov(1, island as u64);
        }),
        0,
        "an unterminated symbol name is a failed lookup, not a crash"
    );
}

// ------------------------------------------------------------------ the guest-memory group

/// `PROT_READ | PROT_WRITE`.
const PROT_RW: u64 = 3;
/// `MAP_PRIVATE | MAP_ANONYMOUS`.
const MAP_ANON_PRIVATE: u64 = 0x22;

/// Call `mmap(addr, length, prot, flags, fd, offset)` from real guest code.
fn guest_mmap(
    f: &Fixture,
    addr: u64,
    length: u64,
    prot: u64,
    flags: u64,
    fd: i64,
    offset: u64,
) -> u64 {
    value_of(f, "mmap", |asm| {
        asm.mov(0, addr);
        asm.mov(1, length);
        asm.mov(2, prot);
        asm.mov(3, flags);
        asm.mov(4, fd as u64);
        asm.mov(5, offset);
    })
}

/// The refusal form of [`guest_mmap`].
fn guest_mmap_refusal(
    f: &Fixture,
    addr: u64,
    length: u64,
    prot: u64,
    flags: u64,
    fd: i64,
) -> AbiError {
    refusal_of(f, "mmap", |asm| {
        asm.mov(0, addr);
        asm.mov(1, length);
        asm.mov(2, prot);
        asm.mov(3, flags);
        asm.mov(4, fd as u64);
        asm.mov(5, 0);
    })
}

/// **`MAP_ANONYMOUS` with a non-negative `fd`, which Linux ignores and this layer once refused.**
///
/// `mmap(2)`: "the fd argument is ignored; however, some implementations require fd to be -1 ...
/// portable applications should ensure this". The kernel's anonymous path never looks at it, and
/// passing `0` is legal and ordinary — the engine's own allocator does exactly that.
///
/// **M3's gate is what found it, and the cost was the whole milestone**: `libroblox.so` imports no
/// allocator at all (D17), so guest `mmap` *is* the heap seam, and every allocation the engine
/// made was being refused as "a file-backed mapping". The gate went from 188 initializers to 3,096
/// of 3,594 on this one clause.
///
/// It was invisible to every test here because every one of them passes `-1`, which is what the
/// manual page tells applications to do and what nothing is obliged to do. Both spellings are
/// asserted, and so is the direction that must **not** change: no `MAP_ANONYMOUS` is still a
/// refusal whatever `fd` says.
#[test]
fn an_anonymous_mmap_ignores_fd_the_way_linux_does() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    for fd in [-1i64, 0, 7] {
        let at = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, fd, 0);
        assert_ne!(at, u64::MAX, "MAP_ANONYMOUS with fd {fd} must not be MAP_FAILED");
        assert_eq!(at % f.guest.space.page_size() as u64, 0);
    }
    // The over-correction: a mapping without `MAP_ANONYMOUS` is file-backed whatever `fd` is, and
    // is still refused by name.
    for fd in [-1i64, 0, 7] {
        let error = guest_mmap_refusal(&f, 0, length, PROT_RW, 0x02 /* MAP_PRIVATE */, fd);
        assert_eq!(error.symbol(), Some("mmap"), "fd {fd}");
        assert!(error.to_string().contains("file-backed"), "fd {fd}: {error}");
    }
}

/// **The heap seam.** The guest asks for anonymous memory, writes to it, and reads it back — all
/// in translated ARM64, so the mapping the handler made is the one the demand pager serves.
#[test]
fn a_guest_mmap_returns_memory_the_guest_can_write_and_read_back() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 128 * 1024;
    let at = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(at, u64::MAX, "MAP_FAILED");
    assert_ne!(at, 0);
    assert_eq!(at % f.guest.space.page_size() as u64, 0, "mmap returns page-aligned memory");

    // Fresh anonymous memory reads as zero, which the allocator this feeds depends on.
    let probe = f.guest.data + 0x400;
    let entry = program(&f, |asm| {
        asm.mov(9, at);
        asm.mov(10, probe as u64);
        asm.push(ldr_imm(11, 9, 0));
        asm.push(str_imm(11, 10, 0)); // what was there before any write
        asm.mov(11, 0x0BAD_F00D_DEAD_BEEF);
        asm.push(str_imm(11, 9, 0));
        asm.push(ldr_imm(12, 9, 0));
        asm.push(str_imm(12, 10, 8));
        // And the last page of the mapping, so the whole length is really there.
        asm.mov(13, at + length - 8);
        asm.push(str_imm(11, 13, 0));
        asm.push(ldr_imm(14, 13, 0));
        asm.push(str_imm(14, 10, 16));
    });
    let exit = run_program(&f, entry).expect("the guest must be able to use what mmap gave it");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    assert_eq!(f.guest.read_u64(probe), 0, "anonymous memory arrives zeroed");
    assert_eq!(f.guest.read_u64(probe + 8), 0x0BAD_F00D_DEAD_BEEF);
    assert_eq!(f.guest.read_u64(probe + 16), 0x0BAD_F00D_DEAD_BEEF, "the last page is mapped too");
    // Mapped, not free. **Not** a length assertion: `region_at` reports the *entry*, and a lazily
    // committed mapping is split at every granule it commits, so the entry at `at` is one 64 KiB
    // granule rather than the whole 128 KiB. That is the shape the straddling-access fix records.
    assert!(f.guest.space.region_at(at as usize).is_some_and(|r| !r.is_free()));
}

/// `munmap` really takes the memory away: the same guest instruction that worked before the call
/// faults after it.
#[test]
fn munmap_takes_the_mapping_away_and_a_later_guest_access_faults() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    let at = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(at, u64::MAX);

    let code = value_of(&f, "munmap", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
    });
    assert_eq!(code as i64 as i32, 0, "munmap succeeded");
    assert!(f.guest.space.region_at(at as usize).is_none_or(|r| r.is_free()));

    let entry = program(&f, |asm| {
        asm.mov(9, at);
        asm.push(ldr_imm(10, 9, 0));
    });
    let exit = run_program(&f, entry).expect("the run itself must not fail");
    match exit {
        ExitReason::MemoryFault { address, .. } => assert_eq!(address as u64, at),
        other => panic!("reading unmapped memory must fault: {other:?}"),
    }
}

/// `mprotect` really changes what the guest may do: a store that worked is refused after it.
#[test]
fn mprotect_drops_a_mapping_to_read_only_and_the_guest_store_faults() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    let at = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(at, u64::MAX);

    let write = program(&f, |asm| {
        asm.mov(9, at);
        asm.mov(10, 0x11);
        asm.push(str_imm(10, 9, 0));
    });
    assert!(matches!(
        run_program(&f, write).expect("writable"),
        ExitReason::Returned { .. }
    ));

    let code = value_of(&f, "mprotect", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, 1); // PROT_READ
    });
    assert_eq!(code as i64 as i32, 0);

    let again = program(&f, |asm| {
        asm.mov(9, at);
        asm.mov(10, 0x22);
        asm.push(str_imm(10, 9, 0));
    });
    match run_program(&f, again).expect("the run itself must not fail") {
        ExitReason::MemoryFault { address, .. } => assert_eq!(address as u64, at),
        other => panic!("a store to a read-only mapping must fault: {other:?}"),
    }
    // Reading still works, so the protection changed rather than the mapping disappearing.
    let read = program(&f, |asm| {
        asm.mov(9, at);
        asm.push(ldr_imm(10, 9, 0));
    });
    assert!(matches!(run_program(&f, read).expect("readable"), ExitReason::Returned { .. }));
}

/// **Every `mmap` shape this layer will not carry out is a refusal, and none of them is
/// `MAP_FAILED`.**
///
/// `MAP_FAILED` is the believable wrong answer: the guest's allocator handles it by trying
/// something else, and the real failure would surface later as an allocation pattern nobody could
/// explain.
#[test]
fn every_unimplementable_mmap_shape_is_refused_by_name() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let page = f.guest.space.page_size() as u64;

    // A real file descriptor.
    let file = guest_mmap_refusal(&f, 0, page, PROT_RW, MAP_PRIVATE_ONLY, 7);
    assert_eq!(file.symbol(), Some("mmap"));
    assert!(file.to_string().contains("file-backed"), "{file}");

    // No MAP_ANONYMOUS, even with fd = -1.
    let not_anon = guest_mmap_refusal(&f, 0, page, PROT_RW, MAP_PRIVATE_ONLY, -1);
    assert!(not_anon.to_string().contains("file-backed"), "{not_anon}");

    // MAP_FIXED, which Linux implements by destroying whatever is already there.
    let fixed = guest_mmap_refusal(&f, 0x4000_0000, page, PROT_RW, MAP_ANON_PRIVATE | 0x10, -1);
    assert!(fixed.to_string().contains("MAP_FIXED"), "{fixed}");
    assert!(fixed.to_string().contains("NOREPLACE"), "{fixed}");

    // A flag nobody implemented: MAP_GROWSDOWN.
    let unknown = guest_mmap_refusal(&f, 0, page, PROT_RW, MAP_ANON_PRIVATE | 0x0100, -1);
    assert!(unknown.to_string().contains("0x100"), "{unknown}");

    // Write without read, and write with execute.
    for prot in [2u64, 4, 6, 7] {
        let error = guest_mmap_refusal(&f, 0, page, prot, MAP_ANON_PRIVATE, -1);
        assert_eq!(error.symbol(), Some("mmap"), "prot {prot:#x}");
        assert!(error.to_string().contains(&format!("{prot:#x}")), "{error}");
    }
}

/// `MAP_PRIVATE` with no `MAP_ANONYMOUS`.
const MAP_PRIVATE_ONLY: u64 = 0x02;

/// A well-formed call that legitimately fails returns `MAP_FAILED` **and sets `errno`** — which is
/// the contract, not a stub. The guest reads `errno` through `__errno`, exactly as it would.
#[test]
fn a_legitimate_mmap_failure_is_map_failed_with_errno_and_not_a_refusal() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let out = f.guest.data + 0x400;

    // Length zero: EINVAL, which is what Linux answers.
    let entry = program(&f, |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, PROT_RW);
        asm.mov(3, MAP_ANON_PRIVATE);
        asm.mov(4, u64::MAX);
        asm.mov(5, 0);
        asm.bl(f.thunk("mmap"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out), u64::MAX, "MAP_FAILED is (void *) -1, not NULL");
    assert_eq!(f.guest.read_u64(out + 8), 22, "EINVAL is Linux's 22");

    // A length that would wrap when rounded up to a page must not become a small one. **The
    // errno is asserted, not only the return**, because both a wrap and an ordinary too-large
    // request answer MAP_FAILED: saturating the round-up rather than checking it would turn this
    // into an ENOMEM for a mapping of `usize::MAX & !4095` bytes, and the return value alone
    // cannot tell the two apart.
    let entry = program(&f, |asm| {
        asm.mov(0, 0);
        asm.mov(1, u64::MAX);
        asm.mov(2, PROT_RW);
        asm.mov(3, MAP_ANON_PRIVATE);
        asm.mov(4, u64::MAX);
        asm.mov(5, 0);
        asm.bl(f.thunk("mmap"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 16));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 24));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 16), u64::MAX, "MAP_FAILED, not a mapping");
    assert_eq!(
        f.guest.read_u64(out + 24),
        12,
        "ENOMEM is Linux's 12, and it is what PAGE_ALIGN(len) == 0 answers there"
    );

    // And a length larger than the guest address space is ENOMEM rather than a panic.
    let huge = guest_mmap(&f, 0, 1 << 40, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_eq!(huge, u64::MAX);
}

/// `MAP_FIXED_NOREPLACE` is honoured because `Placement::Fixed` means exactly that, and a second
/// request for the same address fails rather than destroying the first mapping.
#[test]
fn map_fixed_noreplace_is_honoured_and_refuses_an_occupied_address() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let page = f.guest.space.page_size() as u64;
    let length = 64 * 1024;

    let first = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(first, u64::MAX);
    let second =
        guest_mmap(&f, first, length, PROT_RW, MAP_ANON_PRIVATE | 0x10_0000, -1, 0);
    assert_eq!(second, u64::MAX, "the address is taken, and MAP_FIXED_NOREPLACE must not replace");

    // A misaligned fixed address is EINVAL rather than a rounded-down mapping.
    let misaligned =
        guest_mmap(&f, page + 1, length, PROT_RW, MAP_ANON_PRIVATE | 0x10_0000, -1, 0);
    assert_eq!(misaligned, u64::MAX);
}

/// **The guarantee, asserted by reading the bytes back through guest code.**
///
/// `MADV_FREE` promises "the old contents or zeroes", which `advise_idle` alone gives.
/// `MADV_DONTNEED` promises "zero, immediately" -- and the only way to check that is to write a
/// value, advise the range away, and have the *guest* load from it again. `MADV_REMOVE` stays a
/// refusal, because it is defined on an underlying object this layer's private anonymous mappings
/// do not have.
///
/// M4's gate is what made this necessary: `JNI_OnLoad` reaches the engine's own heap trim, and
/// while `MADV_DONTNEED` was refused, §8 step 6 stopped there.
#[test]
fn madvise_dontneed_really_does_return_zero_and_madv_remove_is_refused() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    let at = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(at, u64::MAX);

    // The value is written and read back **by guest code**, so that "before" and "after" are the
    // same measurement rather than two different ones -- and so that the read goes through the
    // demand pager, which is the thing that has to hand back a fresh zero page.
    let read_back = |f: &Fixture| -> u64 {
        let entry = program(f, |asm| {
            asm.mov(9, at);
            asm.push(ldr_imm(10, 9, 0));
            asm.mov(11, f.guest.data as u64);
            asm.push(str_imm(10, 11, 0));
        });
        run_program(f, entry).expect("the read must complete");
        f.guest.read_u64(f.guest.data)
    };
    let write = program(&f, |asm| {
        asm.mov(9, at);
        asm.mov(10, 0x5555_5555_5555_5555u64);
        asm.push(str_imm(10, 9, 0));
    });
    run_program(&f, write).expect("writable");
    assert_eq!(read_back(&f), 0x5555_5555_5555_5555, "the write landed");

    // `MADV_FREE` is the weak form: the pages may be dropped, so afterwards the contents are the
    // old ones **or** zeroes. Asserted as that disjunction rather than as one of them, because
    // pinning either would pin an implementation detail the contract does not give.
    let freed = value_of(&f, "madvise", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, 8); // MADV_FREE
    });
    assert_eq!(freed as i64 as i32, 0);
    let after_free = read_back(&f);
    assert!(
        after_free == 0x5555_5555_5555_5555 || after_free == 0,
        "MADV_FREE permits the old contents or zeroes, and this is {after_free:#x}"
    );

    // Put the value back, so `MADV_DONTNEED` is tested against a non-zero range whatever
    // `MADV_FREE` did -- and so that the *sequence* an allocator really makes, free then
    // dontneed over the same range, is the one under test.
    run_program(&f, write).expect("writable");
    assert_eq!(read_back(&f), 0x5555_5555_5555_5555);

    let dontneed = value_of(&f, "madvise", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, 4); // MADV_DONTNEED
    });
    assert_eq!(dontneed as i64 as i32, 0, "MADV_DONTNEED must be carried out, not refused");
    assert_eq!(
        read_back(&f),
        0,
        "MADV_DONTNEED guarantees a later read returns zero, and this is the assertion that \
         distinguishes carrying it out from reporting success"
    );
    // The range is still mapped and still writable: `MADV_DONTNEED` releases the contents, not
    // the mapping. An implementation that unmapped it would pass the zero check above and be
    // wrong in a way nothing else here would see.
    run_program(&f, write).expect("still mapped and writable");
    assert_eq!(read_back(&f), 0x5555_5555_5555_5555);

    let removed = refusal_of(&f, "madvise", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, 9); // MADV_REMOVE
    });
    assert_eq!(removed.symbol(), Some("madvise"));
    let text = removed.to_string();
    assert!(text.contains("MADV_REMOVE"), "{text}");
    assert!(text.contains("underlying"), "the refusal names the object it has none of: {text}");

    // The purely advisory ones succeed, because ignoring a hint that cannot change what a read
    // returns is the latitude the interface gives.
    for advice in [0u64, 1, 2, 3, 14, 15] {
        let code = value_of(&f, "madvise", |asm| {
            asm.mov(0, at);
            asm.mov(1, length);
            asm.mov(2, advice);
        });
        assert_eq!(code as i64 as i32, 0, "advice {advice}");
    }
    // And an advice nobody defined is EINVAL, which is what Linux answers.
    let unknown = value_of(&f, "madvise", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, 999);
    });
    assert_eq!(unknown as i64 as i32, -1);
}


/// `mlock` is refused, and the refusal says why `-1`/`ENOMEM` was rejected — it is the most
/// tempting wrong answer in the group, because a failing `mlock` is ordinary on a real device.
#[test]
fn mlock_is_refused_rather_than_answered_with_a_believable_failure() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let error = refusal_of(&f, "mlock", |asm| {
        asm.mov(0, f.guest.data as u64);
        asm.mov(1, 4096);
    });
    assert_eq!(error.symbol(), Some("mlock"));
    let text = error.to_string();
    assert!(text.contains("resident"), "{text}");
    assert!(text.contains("ENOMEM"), "the rejected alternative is named: {text}");
}

/// Hostile arguments to every one of the five: null, unaligned, wild, and lengths that would wrap.
/// None may panic, and each must be a typed error or a documented `-1`.
#[test]
fn hostile_arguments_to_the_guest_memory_group_are_typed_errors_and_not_panics() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let wild = f.guest.unmapped as u64;

    for (symbol, args) in [
        ("munmap", vec![0u64, 0]),
        ("munmap", vec![1, 4096]),
        ("munmap", vec![wild, u64::MAX]),
        ("munmap", vec![u64::MAX, u64::MAX]),
        ("mprotect", vec![1, 4096, 1]),
        ("mprotect", vec![wild, u64::MAX, 1]),
        ("mprotect", vec![0, 0, 0]),
        ("madvise", vec![1, 4096, 8]),
        ("madvise", vec![wild, u64::MAX, 8]),
        ("madvise", vec![0, 0, 3]),
    ] {
        let entry = call_one(&f, symbol, |asm| {
            for (index, value) in args.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
        });
        let mut cpu = f.guest.thread(&f.boundary);
        match f.run(&mut cpu, entry) {
            // Either it completed with a `-1`/`0`, or it refused by name. Both are fine; a panic
            // or a silent success that changed something is not.
            Ok(exit) => {
                assert!(matches!(exit, ExitReason::Returned { .. }), "`{symbol}` {args:?}: {exit:?}");
                let code = f.guest.read_u64(f.guest.data) as i64 as i32;
                assert!(code == 0 || code == -1, "`{symbol}` {args:?} returned {code}");
            }
            Err(error) => {
                assert_eq!(error.symbol(), Some(symbol), "{error:?}");
            }
        }
    }

    // `mmap` with every argument hostile at once.
    let hostile = guest_mmap(&f, u64::MAX, u64::MAX, PROT_RW, MAP_ANON_PRIVATE, -1, u64::MAX);
    assert_eq!(hostile, u64::MAX);
    // `mlock` refuses whatever it is handed, including a wrapping length.
    let locked = refusal_of(&f, "mlock", |asm| {
        asm.mov(0, wild);
        asm.mov(1, u64::MAX);
    });
    assert_eq!(locked.symbol(), Some("mlock"));
}

/// The adapter's static pool refuses rather than wrapping its bump pointer, which would put one
/// image's `dlpi_name` on top of an `AMEDIAFORMAT_KEY_*` string.
#[test]
fn a_full_static_pool_is_a_refusal_and_not_an_overwrite() {
    use omni_android::bionic::POOL_BYTES;
    let _guard = serialized();
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let mut interned = Vec::new();
    let chunk = vec![b'x'; 255];
    loop {
        match bionic.intern("dl_iterate_phdr", &chunk) {
            Ok(at) => interned.push(at),
            Err(error) => {
                assert!(error.to_string().contains("static pool"), "{error}");
                break;
            }
        }
        assert!(interned.len() < POOL_BYTES, "the pool must run out");
    }
    assert!(!interned.is_empty());
    let unique: std::collections::BTreeSet<_> = interned.iter().collect();
    assert_eq!(unique.len(), interned.len(), "no two allocations share an address");
    assert!(bionic.pool_used() <= POOL_BYTES);
}

/// **The demand pager is the heap seam (D10), and this is what says so.** A guest `mmap` is
/// `CommitPolicy::Lazy`, so 16 MiB of address space costs no commit charge until the guest touches
/// it — and then one granule, not sixteen megabytes.
///
/// `libroblox.so` carries its own allocator and reaches the host only through this call, so an
/// eager `mmap` would charge the whole of every arena the engine reserves.
#[test]
fn a_guest_mmap_costs_no_commit_charge_until_the_guest_touches_it() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let granule = f.guest.space.commit_granule();
    let length = 16 * 1024 * 1024;
    // **A warm-up call first, and it is not tidiness.** Every `value_of` creates a guest thread,
    // and the first one takes a TLS block out of `omni-cpu`'s lazily-committed arena — which is a
    // granule of commit charge that has nothing to do with `mmap`. Measuring from before it would
    // have attributed 64 KiB of somebody else's charge to this call, in the direction that makes
    // a lazy mapping look eager.
    let warmup = guest_mmap(&f, 0, 4096, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(warmup, u64::MAX);
    let before = f.guest.space.stats();

    let at = guest_mmap(&f, 0, length as u64, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(at, u64::MAX);
    let mapped = f.guest.space.stats();
    assert_eq!(mapped.mapped - before.mapped, length, "the address space really was claimed");
    assert_eq!(mapped.free, before.free - length, "and it came out of the free space");
    assert_eq!(
        mapped.committed, before.committed,
        "and none of it was committed: address space is free, commit charge is not (D10)"
    );

    let touch = program(&f, |asm| {
        asm.mov(9, at);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
    });
    run_program(&f, touch).expect("the guest can write to it");
    let touched = f.guest.space.stats();
    assert_eq!(
        touched.committed - mapped.committed,
        granule,
        "one granule, committed on demand — not the whole mapping"
    );
}

/// `MAP_SHARED` on anonymous memory is accepted, and is the same thing as `MAP_PRIVATE` here.
///
/// The two differ only across a `fork`, and there is no `fork`: it is not in the reachable set and
/// there is no process surface to build one on. Refusing `MAP_SHARED` would be an over-correction
/// that fails a correct program, which is the direction mutation row `guestmem-B1` exists for.
#[test]
fn an_anonymous_map_shared_is_accepted_because_there_is_no_fork_to_share_with() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    let shared = guest_mmap(&f, 0, length, PROT_RW, 0x21, -1, 0);
    assert_ne!(shared, u64::MAX, "MAP_SHARED | MAP_ANONYMOUS is a legitimate request");
    let entry = program(&f, |asm| {
        asm.mov(9, shared);
        asm.mov(10, 0x5A);
        asm.push(str_imm(10, 9, 0));
    });
    assert!(matches!(run_program(&f, entry).expect("usable"), ExitReason::Returned { .. }));

    // A sharing mode that is neither is EINVAL, which is what Linux answers — `MAP_TYPE` is a
    // three-bit field and `3` is `MAP_SHARED_VALIDATE`, which this layer does not implement.
    let neither = guest_mmap(&f, 0, length, PROT_RW, 0x20, -1, 0);
    assert_eq!(neither, u64::MAX, "no sharing mode at all is EINVAL");
}

/// **A guest that unmaps code and maps different code at the same address must run the new code.**
///
/// The backend caches translations by guest address, so `munmap` and `mprotect` have to discard
/// them or the second call runs the first program. This is the reason the guest-memory group is on
/// the exit path in the first place: `ImportCall` holds no CPU, so an inline handler could not
/// invalidate anything even if changing the address space were safe from one.
///
/// **One CPU context for the whole test, and that is what makes it a detector.** The translating
/// backend's code cache is per context (D5: unshared per-thread code caches), so a version that
/// took a fresh context for each step translated everything afresh every time and could not tell
/// an invalidated cache from an empty one. That version passed with the invalidation removed —
/// mutation row `guestmem-A1` came back NOT CAUGHT, which is what found it.
///
/// Both programs are two instructions and differ only in the constant they return, so a failure
/// here is unambiguous: `0xAA` where `0xBB` belongs is the old translation.
#[test]
fn code_at_a_reused_address_is_retranslated_rather_than_run_from_the_cache() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    let out = f.guest.data + 0x400;
    let mut cpu = f.guest.thread(&f.boundary);

    let at = {
        let entry = call_one(&f, "mmap", |asm| {
            asm.mov(0, 0);
            asm.mov(1, length);
            asm.mov(2, PROT_RW);
            asm.mov(3, MAP_ANON_PRIVATE);
            asm.mov(4, u64::MAX);
            asm.mov(5, 0);
        });
        f.guest.rearm(&mut cpu, &f.boundary);
        f.run(&mut cpu, entry).expect("mmap");
        f.guest.read_u64(f.guest.data)
    };
    assert_ne!(at, u64::MAX);

    // Write a two-instruction program at `at`, make it executable through the guest's own
    // `mprotect`, call it, and store what it answered.
    let install_and_call =
        |f: &Fixture, cpu: &mut omni_cpu::dynarmic::DynarmicCpu, answer: u16, slot: u32| {
        f.guest.space.ensure_committed(at as usize, 16).expect("the first page of the mapping");
        f.guest.write_bytes(
            at as usize,
            &[movz(0, answer, 0).to_le_bytes(), ret(30).to_le_bytes()].concat(),
        );
        let protect = call_one(f, "mprotect", |asm| {
            asm.mov(0, at);
            asm.mov(1, length);
            asm.mov(2, 5); // PROT_READ | PROT_EXEC
        });
        f.guest.rearm(cpu, &f.boundary);
        f.run(cpu, protect).expect("mprotect");
        assert_eq!(f.guest.read_u64(f.guest.data) as i64 as i32, 0);

        // A fresh caller each time, so what is under test is the cached translation of the callee
        // at `at` rather than of the caller.
        let caller = program(f, |asm| {
            asm.mov(9, at);
            asm.mov(10, out as u64);
            asm.push(blr(9));
            asm.push(str_imm(0, 10, slot));
        });
        f.guest.rearm(cpu, &f.boundary);
        let exit = f.run(cpu, caller).expect("the guest runs what it mapped");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    };

    install_and_call(&f, &mut cpu, 0xAA, 0);
    assert_eq!(f.guest.read_u64(out), 0xAA);

    // Give the range back and take it again at the same address, then put a different program
    // there. Without the invalidation in `munmap`/`mprotect` the cached translation of the first
    // one still answers.
    let unmap = call_one(&f, "munmap", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
    });
    f.guest.rearm(&mut cpu, &f.boundary);
    f.run(&mut cpu, unmap).expect("munmap");
    assert_eq!(f.guest.read_u64(f.guest.data) as i64 as i32, 0);

    let remap = call_one(&f, "mmap", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, PROT_RW);
        asm.mov(3, MAP_ANON_PRIVATE | 0x10_0000);
        asm.mov(4, u64::MAX);
        asm.mov(5, 0);
    });
    f.guest.rearm(&mut cpu, &f.boundary);
    f.run(&mut cpu, remap).expect("mmap");
    assert_eq!(
        f.guest.read_u64(f.guest.data),
        at,
        "MAP_FIXED_NOREPLACE must give the address back"
    );

    install_and_call(&f, &mut cpu, 0xBB, 8);
    assert_eq!(
        f.guest.read_u64(out + 8),
        0xBB,
        "0xAA here is the first program's translation, served out of the code cache after the          memory it was translated from was unmapped"
    );
}


// =================================================================== M3 task 3 phase 3a
//
// Clocks, process and environment, and the log sink — the first symbols in this adapter whose
// answers come from `omni-platform`.

use omni_android::bionic::{HwcapPolicy, LogPriority, HWCAP_ATOMICS, LOG_CAPTURE_MAX, PROP_VALUE_MAX};

/// Linux `clockid_t` values, as guest code passes them.
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
const CLOCK_THREAD_CPUTIME_ID: u64 = 3;
const CLOCK_BOOTTIME: u64 = 7;

/// Read a guest `int` at any 4-byte-aligned address.
///
/// The harness reads 8 bytes at a time from an 8-aligned address, and a `struct tm` is nine `int`s
/// in a row — so half of them start at offset 4 of their word.
fn read_i32(f: &Fixture, at: omni_cpu::GuestAddr) -> i32 {
    assert_eq!(at % 4, 0, "an int is 4-byte aligned");
    let word = f.guest.read_u64(at & !7);
    let shift = 32 * ((at & 7) / 4);
    (word >> shift) as u32 as i32
}

// ------------------------------------------------------------------ clocks

/// **The clocks this layer models are answered from the platform seam, and the ids it does not
/// model are refused by number.**
///
/// The refusal half is the part that matters. `clock_gettime(CLOCK_THREAD_CPUTIME_ID, &ts)`
/// answered with wall time is a plausible, monotonic number of seconds that is not what was asked
/// for, and a guest profiler built on it would report wall time as CPU time forever.
#[test]
fn clock_gettime_answers_the_clocks_it_models_and_refuses_the_ones_it_does_not() {
    let _guard = serialized();
    let f = fixture();
    let ts = f.guest.data + 0x200;

    for id in [CLOCK_MONOTONIC, 4 /* _RAW */, 6 /* _COARSE */] {
        f.guest.write_u64(ts, u64::MAX);
        f.guest.write_u64(ts + 8, u64::MAX);
        let code = value_of(&f, "clock_gettime", |asm| {
            asm.mov(0, id);
            asm.mov(1, ts as u64);
        });
        assert_eq!(code as i64 as i32, 0, "clock {id}");
        let nanos = f.guest.read_u64(ts + 8);
        assert!(nanos < 1_000_000_000, "clock {id}: tv_nsec is {nanos}, which is not a fraction");
        // The monotonic clock is measured from a process epoch, so a *plausible* reading is a
        // small number of seconds rather than a Unix time. Asserting the upper bound is what
        // catches "monotonic was served from the wall clock", which is otherwise invisible.
        let seconds = f.guest.read_u64(ts);
        assert!(seconds < 1_000_000, "clock {id}: {seconds} s looks like a wall clock");
    }

    // Monotonic never goes backwards across two real calls through the boundary.
    let first = {
        value_of(&f, "clock_gettime", |asm| {
            asm.mov(0, CLOCK_MONOTONIC);
            asm.mov(1, ts as u64);
        });
        (f.guest.read_u64(ts), f.guest.read_u64(ts + 8))
    };
    let second = {
        value_of(&f, "clock_gettime", |asm| {
            asm.mov(0, CLOCK_MONOTONIC);
            asm.mov(1, ts as u64);
        });
        (f.guest.read_u64(ts), f.guest.read_u64(ts + 8))
    };
    assert!(second >= first, "CLOCK_MONOTONIC went backwards: {first:?} -> {second:?}");

    for id in [CLOCK_REALTIME, 5 /* _COARSE */] {
        let code = value_of(&f, "clock_gettime", |asm| {
            asm.mov(0, id);
            asm.mov(1, ts as u64);
        });
        assert_eq!(code as i64 as i32, 0, "clock {id}");
        // 2020-01-01. A wall clock below it is a host whose clock is not set, which is worth
        // knowing about; the point of the bound is that it separates a wall clock from a
        // monotonic one, and nothing narrower would.
        assert!(f.guest.read_u64(ts) > 1_577_836_800, "clock {id} is not a wall clock");
    }

    // **`CLOCK_PROCESS_CPUTIME_ID` was refused here until phase 3e and is answered now**, which
    // is a correction to D22 rather than a change of mind: that phase added
    // `omni_platform::process::cpu_time` for the guest's `clock()`, and a layer that reported the
    // figure through one symbol while saying it could not be had through another would be giving
    // one question two answers. `the_process_cpu_clock_is_answered_and_the_thread_cpu_clock_is_
    // still_refused` asserts the two against each other.
    //
    // The two that are still refused are refused for reasons that did **not** go away: a
    // per-thread figure is `GetThreadTimes`, a primitive that does not exist, and `CLOCK_BOOTTIME`
    // counts time spent suspended, which nothing here can know.
    for (id, name) in [
        (CLOCK_THREAD_CPUTIME_ID, "CLOCK_THREAD_CPUTIME_ID"),
        (CLOCK_BOOTTIME, "CLOCK_BOOTTIME"),
    ] {
        let error = refusal_of(&f, "clock_gettime", |asm| {
            asm.mov(0, id);
            asm.mov(1, ts as u64);
        });
        assert_eq!(error.symbol(), Some("clock_gettime"));
        let text = error.to_string();
        assert!(text.contains(name), "the refusal must name the clock asked for: {text}");
    }
    // And one nobody has a name for is still refused, with its number in the message.
    let unknown = refusal_of(&f, "clock_gettime", |asm| {
        asm.mov(0, 4242);
        asm.mov(1, ts as u64);
    });
    assert!(unknown.to_string().contains("4242"), "{unknown}");
}

/// **`gettimeofday` writes MICROseconds**, and zeroes the obsolete `struct timezone`.
///
/// The thousand-fold error is the one this catches: `tv_usec` filled with nanoseconds is still a
/// number under a billion and still increases, so nothing but a range check sees it.
#[test]
fn gettimeofday_writes_microseconds_and_zeroes_the_obsolete_timezone() {
    let _guard = serialized();
    let f = fixture();
    let tv = f.guest.data + 0x200;
    let tz = f.guest.data + 0x240;
    f.guest.write_u64(tv, u64::MAX);
    f.guest.write_u64(tv + 8, u64::MAX);
    f.guest.write_u64(tz, 0xAAAA_AAAA_AAAA_AAAA);

    let code = value_of(&f, "gettimeofday", |asm| {
        asm.mov(0, tv as u64);
        asm.mov(1, tz as u64);
    });
    assert_eq!(code as i64 as i32, 0);
    assert!(f.guest.read_u64(tv) > 1_577_836_800, "tv_sec must be a wall clock");
    let micros = f.guest.read_u64(tv + 8);
    assert!(micros < 1_000_000, "tv_usec is {micros}: a struct timeval is MICROseconds");
    assert_eq!(f.guest.read_u64(tz), 0, "Linux fills the obsolete struct timezone with zeroes");

    // Both null is legal and writes nothing: `gettimeofday(NULL, NULL)` must not fault.
    let code = value_of(&f, "gettimeofday", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
    });
    assert_eq!(code as i64 as i32, 0, "a null tv is legal — the call is then only about tz");
}

/// **`gmtime_r` fills the guest's `struct tm` at the documented offsets.**
///
/// A leap day in a year divisible by 400 is the date chosen, because it exercises the century rule
/// in both directions at once. Asserted field by field against a date anyone can check, not
/// against the conversion's own inverse.
#[test]
fn gmtime_r_breaks_a_real_timestamp_down_into_the_guest_struct_tm() {
    let _guard = serialized();
    let f = fixture();
    let timer = f.guest.data + 0x200;
    let result = f.guest.data + 0x240;
    // 2000-02-29T12:00:00Z, a Tuesday, day 59 of the year.
    f.guest.write_u64(timer, 951_825_600);
    for offset in (0..56).step_by(8) {
        f.guest.write_u64(result + offset, 0xAAAA_AAAA_AAAA_AAAA);
    }

    let returned = value_of(&f, "gmtime_r", |asm| {
        asm.mov(0, timer as u64);
        asm.mov(1, result as u64);
    });
    assert_eq!(returned, result as u64, "gmtime_r returns the buffer it was given");

    assert_eq!(read_i32(&f, result), 0, "tm_sec");
    assert_eq!(read_i32(&f, result + 4), 0, "tm_min");
    assert_eq!(read_i32(&f, result + 8), 12, "tm_hour");
    assert_eq!(read_i32(&f, result + 12), 29, "tm_mday");
    assert_eq!(read_i32(&f, result + 16), 1, "tm_mon is 0-based, so February is 1");
    assert_eq!(read_i32(&f, result + 20), 100, "tm_year is years since 1900");
    assert_eq!(read_i32(&f, result + 24), 2, "tm_wday: 2000-02-29 was a Tuesday");
    assert_eq!(read_i32(&f, result + 28), 59, "tm_yday is 0-based");
    assert_eq!(read_i32(&f, result + 32), 0, "UTC has no daylight saving");
    assert_eq!(f.guest.read_u64(result + 40), 0, "tm_gmtoff: UTC's offset is zero");
    let zone = f.guest.read_u64(result + 48) as omni_cpu::GuestAddr;
    assert_ne!(zone, 0, "tm_zone is a `const char *` guest code prints");
    assert_eq!(f.read_cstring(zone), b"UTC", "and it says UTC, because gmtime is UTC");

    // A pre-1970 timestamp: the half of the calendar arithmetic that a truncating division gets
    // wrong by exactly one day, and only before the epoch.
    f.guest.write_u64(timer, (-1i64) as u64);
    value_of(&f, "gmtime_r", |asm| {
        asm.mov(0, timer as u64);
        asm.mov(1, result as u64);
    });
    assert_eq!(read_i32(&f, result + 20), 69, "1969");
    assert_eq!(read_i32(&f, result + 16), 11, "December");
    assert_eq!(read_i32(&f, result + 12), 31);
    assert_eq!(read_i32(&f, result + 8), 23);

    // A year that will not fit `int tm_year` is NULL with EOVERFLOW, which is C's answer and not
    // a stub. The struct is left as it was rather than half written.
    f.guest.write_u64(result, 0x1234_5678_9ABC_DEF0);
    f.guest.write_u64(timer, i64::MAX as u64);
    let refused = value_of(&f, "gmtime_r", |asm| {
        asm.mov(0, timer as u64);
        asm.mov(1, result as u64);
    });
    assert_eq!(refused, 0, "an out-of-range year is NULL, not a wrapped date");
    assert_eq!(
        f.guest.read_u64(result),
        0x1234_5678_9ABC_DEF0,
        "a failed conversion must not have written anything"
    );
}

/// **`localtime_r` is `gmtime_r` on a runtime with no zone, and the premise is asserted with it.**
///
/// The premise is what makes it true: no `TZ` in the environment. So the test reads `getenv("TZ")`
/// through the guest first -- if an embedding ever gives this instance a zone, the premise fails
/// here, by name, instead of `localtime_r` quietly disagreeing with the environment. Then both
/// conversions of one timestamp, into two buffers, compared byte for byte over the whole
/// `struct tm` including `tm_gmtoff` and the `tm_zone` pointer.
#[test]
fn localtime_r_is_gmtime_r_on_a_runtime_with_no_zone() {
    let _guard = serialized();
    let f = fixture();
    let name = f.cstring(f.guest.data + 0x100, b"TZ");
    assert_eq!(
        value_of(&f, "getenv", |asm| { asm.mov(0, name as u64); }),
        0,
        "the premise: this instance has no TZ"
    );

    let timer = f.guest.data + 0x200;
    let (local, utc) = (f.guest.data + 0x240, f.guest.data + 0x280);
    f.guest.write_u64(timer, 951_825_600); // 2000-02-29T12:00:00Z
    for offset in (0..56).step_by(8) {
        f.guest.write_u64(local + offset, 0xAAAA_AAAA_AAAA_AAAA);
        f.guest.write_u64(utc + offset, 0x5555_5555_5555_5555);
    }
    let returned = value_of(&f, "localtime_r", |asm| {
        asm.mov(0, timer as u64);
        asm.mov(1, local as u64);
    });
    assert_eq!(returned, local as u64, "localtime_r returns the buffer it was given");
    value_of(&f, "gmtime_r", |asm| {
        asm.mov(0, timer as u64);
        asm.mov(1, utc as u64);
    });
    assert_eq!(read_guest(&f, local, 56), read_guest(&f, utc, 56), "the same struct tm");
    assert_eq!(read_i32(&f, local + 8), 12, "tm_hour is the UTC hour");
    assert_eq!(f.guest.read_u64(local + 40), 0, "tm_gmtoff");
}

/// **`gmtime` answers into this thread's own `struct tm`, and it is not the scratch buffer.**
///
/// The non-reentrant spelling returns a pointer to storage the library owns. M4's gate found it
/// missing, and it has to agree with `gmtime_r` on the one case where the two could drift: a year
/// that does not fit `int tm_year` is NULL with `EOVERFLOW`, not a wrapped date.
///
/// The last part is what makes the per-thread block's split load-bearing: `strerror` and `gmtime`
/// both hand back a pointer the guest may hold, so a `gmtime` between a `strerror` and the
/// guest's read of it must not rewrite the message.
#[test]
fn gmtime_answers_into_per_thread_storage_that_strerror_does_not_share() {
    let _guard = serialized();
    let f = fixture();
    let timer = f.guest.data + 0x200;
    // 2000-02-29T12:00:00Z, as `gmtime_r`'s test uses, so the two are compared on one date.
    f.guest.write_u64(timer, 951_825_600);

    let returned = value_of(&f, "gmtime", |asm| {
        asm.mov(0, timer as u64);
    });
    assert_ne!(returned, 0, "gmtime returns a pointer to storage it owns");
    let at = returned as omni_cpu::GuestAddr;
    assert_eq!(read_i32(&f, at + 8), 12, "tm_hour");
    assert_eq!(read_i32(&f, at + 12), 29, "tm_mday");
    assert_eq!(read_i32(&f, at + 16), 1, "tm_mon is 0-based, so February is 1");
    assert_eq!(read_i32(&f, at + 20), 100, "tm_year is years since 1900");
    assert_eq!(read_i32(&f, at + 24), 2, "2000-02-29 was a Tuesday");
    let zone = f.guest.read_u64(at + 48) as omni_cpu::GuestAddr;
    assert_eq!(f.read_cstring(zone), b"UTC");

    // The same answer `gmtime_r` gives for a year that will not fit, which is the one case the
    // two spellings could drift apart on.
    f.guest.write_u64(timer, i64::MAX as u64);
    let refused = value_of(&f, "gmtime", |asm| {
        asm.mov(0, timer as u64);
    });
    assert_eq!(refused, 0, "an out-of-range year is NULL, not a wrapped date");

    // **The two returned pointers must not overlap.** `strerror` hands back a `char *` from the
    // same thread block, and one buffer for both would let this `gmtime` rewrite that message.
    f.guest.write_u64(timer, 951_825_600);
    let message = value_of(&f, "strerror", |asm| {
        asm.mov(0, 2); // ENOENT
    }) as omni_cpu::GuestAddr;
    let before = f.read_cstring(message);
    assert!(!before.is_empty());
    let tm = value_of(&f, "gmtime", |asm| {
        asm.mov(0, timer as u64);
    }) as omni_cpu::GuestAddr;
    assert!(
        tm > message + before.len() || message >= tm + 56,
        "strerror's buffer at {message:#x} and gmtime's struct tm at {tm:#x} overlap"
    );
    assert_eq!(f.read_cstring(message), before, "and the message survived the gmtime");
}

/// **A sleep really sleeps, a malformed request is `EINVAL`, and a request past the cap refuses.**
///
/// The cap is the hostile-input half: a sleeping thread executes no guest instructions, so D16's
/// step-budget watchdog cannot end one, and `nanosleep({INT64_MAX, 0})` would be a permanent hang
/// of the host thread that serviced it.
///
/// The duration assertion is **one-sided**, which is the only side `nanosleep` and Windows' ~15.6 ms
/// timer tick between them guarantee. n = 1: a lower bound on a sleep is not a rare event and does
/// not need a sample — every run either slept or did not.
#[test]
fn nanosleep_sleeps_reports_einval_and_refuses_a_request_past_the_cap() {
    let _guard = serialized();
    let f = fixture();
    let req = f.guest.data + 0x200;
    let rem = f.guest.data + 0x240;

    // 10 ms.
    f.guest.write_u64(req, 0);
    f.guest.write_u64(req + 8, 10_000_000);
    f.guest.write_u64(rem, 0xAAAA_AAAA_AAAA_AAAA);
    f.guest.write_u64(rem + 8, 0xAAAA_AAAA_AAAA_AAAA);
    let before = std::time::Instant::now();
    let code = value_of(&f, "nanosleep", |asm| {
        asm.mov(0, req as u64);
        asm.mov(1, rem as u64);
    });
    let elapsed = before.elapsed();
    assert_eq!(code as i64 as i32, 0);
    assert!(
        elapsed >= std::time::Duration::from_millis(10),
        "a 10 ms nanosleep returned after {elapsed:?}, so it did not sleep"
    );
    assert_eq!(f.guest.read_u64(rem), 0, "no signals are delivered, so nothing remains");
    assert_eq!(f.guest.read_u64(rem + 8), 0);

    // POSIX's validity rule, at both edges. `-1` with `errno` is the C library's own answer to a
    // malformed request and is a contract rather than a stub.
    for (seconds, nanos) in [(0u64, 1_000_000_000u64), (0, (-1i64) as u64), ((-1i64) as u64, 0)] {
        f.guest.write_u64(req, seconds);
        f.guest.write_u64(req + 8, nanos);
        let code = value_of(&f, "nanosleep", |asm| {
            asm.mov(0, req as u64);
            asm.mov(1, 0);
        });
        assert_eq!(code as i64 as i32, -1, "{seconds}s + {nanos}ns must be EINVAL");
    }

    // Past the cap: refused by name, with both numbers in the message.
    f.guest.write_u64(req, i64::MAX as u64);
    f.guest.write_u64(req + 8, 0);
    let error = refusal_of(&f, "nanosleep", |asm| {
        asm.mov(0, req as u64);
        asm.mov(1, 0);
    });
    assert_eq!(error.symbol(), Some("nanosleep"));
    let text = error.to_string();
    assert!(text.contains("60"), "the refusal must name the cap: {text}");
    assert!(text.contains(&i64::MAX.to_string()), "and what was asked for: {text}");
}

/// **`usleep` takes only the low 32 bits of `X0`**, because `useconds_t` is `unsigned int`.
///
/// The structural assertion, and the reason it is structural rather than timed: AAPCS64 does not
/// require a caller to clear the high half of a register holding a 32-bit argument, so a handler
/// that read all 64 bits would turn a perfectly ordinary 100 µs sleep into a request for 584,000
/// years — which the cap would then *refuse*. So the test is "a correct call is not refused", and
/// it fails loudly against the wrong read rather than hanging.
#[test]
fn usleep_reads_only_the_low_thirty_two_bits_of_its_argument() {
    let _guard = serialized();
    let f = fixture();
    let code = value_of(&f, "usleep", |asm| {
        asm.mov(0, 0xFFFF_FFFF_0000_0064);
    });
    assert_eq!(code as i64 as i32, 0, "100 us with a dirty high half must still be 100 us");

    // And the cap still applies to a value that really is large: 0xFFFF_FFFF us is 4,294 s.
    let error = refusal_of(&f, "usleep", |asm| {
        asm.mov(0, 0xFFFF_FFFF);
    });
    assert_eq!(error.symbol(), Some("usleep"));
    assert!(error.to_string().contains("60"), "{error}");
}

// ------------------------------------------------------------------ process and environment

/// `getpid` is the host's, and `sched_getcpu` is answered or refused by name — never `-1`.
#[test]
fn getpid_and_sched_getcpu_come_from_the_platform_seam() {
    let _guard = serialized();
    let f = fixture();
    let pid = value_of(&f, "getpid", |_| {}) as i64 as i32;
    assert_eq!(
        pid,
        std::process::id() as i32,
        "several guest instances share one host process, exactly as several threads of an \
         Android process share one pid"
    );

    let entry = call_one(&f, "sched_getcpu", |_| {});
    let mut cpu = f.guest.thread(&f.boundary);
    match f.run(&mut cpu, entry) {
        Ok(_) => {
            let id = f.guest.read_u64(f.guest.data) as i64 as i32;
            assert!(id >= 0, "a processor number is never negative: {id}");
        }
        Err(error) => {
            // The other four targets have no cpu-id backend and must say so by name. `-1` is a
            // documented `sched_getcpu` failure that callers route around, so it is the one
            // answer that would hide the gap.
            assert_eq!(error.symbol(), Some("sched_getcpu"));
            assert!(error.to_string().contains("sched_getcpu"), "{error}");
        }
    }
}

/// **`arc4random_buf` fills exactly what it was given, with bytes that differ between draws.**
///
/// Structural, not statistical: 64 bytes left at zero is what a backend that reports success
/// without writing looks like, two identical 64-byte draws is what a constant source looks like,
/// and an untouched sentinel past the end is what a length bug looks like. None of the three is a
/// randomness-quality claim and none is a flake risk at 2^-512.
#[test]
fn arc4random_buf_fills_exactly_the_buffer_it_was_given() {
    let _guard = serialized();
    let f = fixture();
    let first = f.guest.data + 0x400;
    let second = f.guest.data + 0x500;
    // 64 bytes of buffer followed by 32 bytes of sentinel.
    for offset in (0..96).step_by(8) {
        f.guest.write_u64(first + offset, 0xAAAA_AAAA_AAAA_AAAA);
        f.guest.write_u64(second + offset, 0xAAAA_AAAA_AAAA_AAAA);
    }

    for at in [first, second] {
        value_of(&f, "arc4random_buf", |asm| {
            asm.mov(0, at as u64);
            asm.mov(1, 64);
        });
    }
    let read = |at: omni_cpu::GuestAddr| -> Vec<u64> {
        (0..64).step_by(8).map(|o| f.guest.read_u64(at + o)).collect()
    };
    let a = read(first);
    let b = read(second);
    assert!(a.iter().any(|&w| w != 0), "the buffer was reported filled and is all zero");
    assert!(
        a.iter().any(|&w| w != 0xAAAA_AAAA_AAAA_AAAA),
        "the buffer was reported filled and is untouched"
    );
    assert_ne!(a, b, "two draws from a real entropy source cannot be equal");
    for at in [first, second] {
        for offset in (64..96).step_by(8) {
            assert_eq!(
                f.guest.read_u64(at + offset),
                0xAAAA_AAAA_AAAA_AAAA,
                "arc4random_buf wrote past the {offset}th byte of a 64-byte request"
            );
        }
    }

    // **A request bigger than the host-side chunk, so the multi-chunk path is real code.** The
    // failure this catches is a chunk loop that writes every chunk at the *base* rather than at
    // the offset: with one chunk that is invisible, and with two it silently leaves the second
    // half untouched.
    let wide = f.guest.data + 0x1000;
    let wide_len = 8192u64;
    for offset in (0..wide_len).step_by(8) {
        f.guest.write_u64(wide + offset as omni_cpu::GuestAddr, 0xAAAA_AAAA_AAAA_AAAA);
    }
    value_of(&f, "arc4random_buf", |asm| {
        asm.mov(0, wide as u64);
        asm.mov(1, wide_len);
    });
    for (half, range) in [("first", 0u64..4096), ("second", 4096..8192)] {
        assert!(
            range
                .step_by(8)
                .any(|o| f.guest.read_u64(wide + o as omni_cpu::GuestAddr) != 0xAAAA_AAAA_AAAA_AAAA),
            "the {half} 4 KiB chunk of an 8 KiB request was left untouched"
        );
    }

    // **A destination that is writable for its first chunk and not its second is refused with
    // NOTHING written.** Without validating the whole range up front, the first 4 KiB would
    // receive real entropy and the call would then report failure — and the caller would have no
    // way to know which half it got.
    let straddle = f.guest.data + harness::DATA_BYTES as omni_cpu::GuestAddr - 5000;
    for offset in (0..4992u64).step_by(8) {
        f.guest.write_u64(straddle + offset as omni_cpu::GuestAddr, 0xAAAA_AAAA_AAAA_AAAA);
    }
    let error = refusal_of(&f, "arc4random_buf", |asm| {
        asm.mov(0, straddle as u64);
        asm.mov(1, 8192);
    });
    assert_eq!(error.symbol(), Some("arc4random_buf"));
    for offset in (0..4992u64).step_by(8) {
        assert_eq!(
            f.guest.read_u64(straddle + offset as omni_cpu::GuestAddr),
            0xAAAA_AAAA_AAAA_AAAA,
            "a refused arc4random_buf wrote entropy into the part of the range that WAS writable"
        );
    }

    // A zero length writes nothing and is legal C at any address, null included.
    let sentinel = f.guest.read_u64(first);
    value_of(&f, "arc4random_buf", |asm| {
        asm.mov(0, first as u64);
        asm.mov(1, 0);
    });
    assert_eq!(f.guest.read_u64(first), sentinel, "a zero-length request must touch nothing");
    value_of(&f, "arc4random_buf", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
    });
}

/// **`getenv` answers `NULL` until the host gives the guest a variable**, and the pointer it then
/// returns is stable.
///
/// Empty is a *fact* about a process that was started with no environment — the same fact the
/// `environ` data object states by pointing at a vector of one null — not a stub. The host's own
/// environment is deliberately unreachable: it would be both a wrong answer and a leak.
#[test]
fn getenv_answers_null_until_the_host_gives_the_guest_a_variable() {
    let _guard = serialized();
    let f = fixture();
    let path = f.cstring(f.guest.data + 0x100, b"PATH");
    let name = f.cstring(f.guest.data + 0x140, b"OMNI_TEST");
    let empty = f.cstring(f.guest.data + 0x180, b"");
    let malformed = f.cstring(f.guest.data + 0x1C0, b"A=B");

    // `PATH` is certainly set in the host's environment, which is exactly why it is the one asked
    // for: a `getenv` that reached the host would answer it.
    assert_eq!(
        value_of(&f, "getenv", |asm| { asm.mov(0, path as u64); }),
        0,
        "the guest must not see the host's environment"
    );
    assert_eq!(value_of(&f, "getenv", |asm| { asm.mov(0, 0); }), 0, "getenv(NULL)");
    assert_eq!(value_of(&f, "getenv", |asm| { asm.mov(0, empty as u64); }), 0, "an empty name");
    assert_eq!(value_of(&f, "getenv", |asm| { asm.mov(0, malformed as u64); }), 0, "a name with `=`");

    f.bionic.set_env("OMNI_TEST", "a value").expect("the pool has room");
    let found = value_of(&f, "getenv", |asm| { asm.mov(0, name as u64); });
    assert_ne!(found, 0, "a variable the host set must be found");
    assert_eq!(f.read_cstring(found as omni_cpu::GuestAddr), b"a value");
    // `getenv`'s contract is that the pointer stays valid, so two calls give the same address.
    assert_eq!(value_of(&f, "getenv", |asm| { asm.mov(0, name as u64); }), found);

    // A name that could never be found again is refused at the setting end rather than stored.
    assert!(f.bionic.set_env("", "x").is_err());
    assert!(f.bionic.set_env("A=B", "x").is_err());
}

/// **The property table is empty until the host fills it**, and an oversized value is refused when
/// it is set rather than truncated when it is read.
#[test]
fn the_system_property_table_is_empty_until_the_host_fills_it() {
    let _guard = serialized();
    let f = fixture();
    let name = f.cstring(f.guest.data + 0x100, b"ro.build.version.sdk");
    let value = f.guest.data + 0x200;
    f.guest.write_u64(value, 0xAAAA_AAAA_AAAA_AAAA);

    let length = value_of(&f, "__system_property_get", |asm| {
        asm.mov(0, name as u64);
        asm.mov(1, value as u64);
    }) as i64 as i32;
    assert_eq!(length, 0, "an unset property is length 0");
    assert_eq!(f.read_cstring(value), b"", "and an empty string, not an untouched buffer");

    f.bionic.set_system_property("ro.build.version.sdk", "33").expect("a short value");
    let length = value_of(&f, "__system_property_get", |asm| {
        asm.mov(0, name as u64);
        asm.mov(1, value as u64);
    }) as i64 as i32;
    assert_eq!(length, 2, "the length excludes the NUL, as bionic's does");
    assert_eq!(f.read_cstring(value), b"33");

    // A null name is answered as "not set" rather than dereferenced.
    let length = value_of(&f, "__system_property_get", |asm| {
        asm.mov(0, 0);
        asm.mov(1, value as u64);
    }) as i64 as i32;
    assert_eq!(length, 0);

    // The guest declares `char value[PROP_VALUE_MAX]`, so a longer value is refused at the setting
    // end: writing it would overflow a buffer in guest code, and truncating it would be a
    // believable wrong answer.
    let too_long = "x".repeat(PROP_VALUE_MAX);
    let error = f.bionic.set_system_property("ro.too.long", &too_long).expect_err("refused");
    assert!(error.to_string().contains(&PROP_VALUE_MAX.to_string()), "{error}");
    // One byte less, with its NUL, is exactly the limit and is accepted.
    f.bionic
        .set_system_property("ro.exactly.max", &"x".repeat(PROP_VALUE_MAX - 1))
        .expect("PROP_VALUE_MAX includes the NUL");
}

/// **`getauxval(AT_HWCAP)` REFUSES until a host makes the decision, and the refusal carries both
/// measured arms.**
///
/// This is the open `AT_HWCAP` question, and the test exists so that it cannot be closed by
/// accident. Advertising `HWCAP_ATOMICS` gives 53 hard interpreter halts; declining gives 106
/// fallback arms into a global spinlock that anti-scales 21x. Neither is a default, and a policy
/// type in which "no decision" and "decided to decline" were the same value could not refuse.
#[test]
fn getauxval_refuses_at_hwcap_until_a_host_makes_the_decision() {
    let _guard = serialized();
    let f = fixture();

    // The default is the refusal, and it names both arms.
    assert_eq!(f.bionic.hwcap_policy(), HwcapPolicy::Undecided);
    for kind in [16u64 /* AT_HWCAP */, 26 /* AT_HWCAP2 */] {
        let error = refusal_of(&f, "getauxval", |asm| { asm.mov(0, kind); });
        assert_eq!(error.symbol(), Some("getauxval"));
        let text = error.to_string();
        assert!(text.contains("OPEN DECISION"), "{text}");
        assert!(text.contains("53"), "the refusal must carry the advertise arm: {text}");
        assert!(text.contains("106"), "and the decline arm: {text}");
        assert!(text.contains("21x"), "and what declining costs: {text}");
    }

    // A host that has decided says so, and then gets what it asked for — including zero, which is
    // a decision and is not the same value as having made none.
    f.bionic.set_hwcap_policy(HwcapPolicy::Decline);
    assert_eq!(value_of(&f, "getauxval", |asm| { asm.mov(0, 16); }), 0);
    assert_eq!(value_of(&f, "getauxval", |asm| { asm.mov(0, 26); }), 0);

    f.bionic.set_hwcap_policy(HwcapPolicy::Advertise { hwcap: HWCAP_ATOMICS, hwcap2: 7 });
    assert_eq!(value_of(&f, "getauxval", |asm| { asm.mov(0, 16); }), HWCAP_ATOMICS);
    assert_eq!(value_of(&f, "getauxval", |asm| { asm.mov(0, 26); }), 7);
    assert_eq!(HWCAP_ATOMICS, 1 << 8, "HWCAP_ATOMICS is bit 8 of AT_HWCAP on AArch64");

    // `AT_PAGESZ` is a fact and is answered whatever the policy is.
    let page = value_of(&f, "getauxval", |asm| { asm.mov(0, 6); });
    assert_eq!(page, f.guest.space.page_size() as u64);

    // Everything else refuses by number, rather than returning `getauxval`'s documented 0/ENOENT —
    // which a guest cannot tell apart from a key whose value really is zero.
    for kind in [23u64 /* AT_SECURE */, 17 /* AT_CLKTCK */, 25 /* AT_RANDOM */, 9999] {
        let error = refusal_of(&f, "getauxval", |asm| { asm.mov(0, kind); });
        assert!(error.to_string().contains(&kind.to_string()), "{error}");
    }
}

/// **`abort`, `__stack_chk_fail` and `_exit` become typed, catchable outcomes.**
///
/// The failure this replaces is `std::process::abort()`, which no caller can contain and which
/// would take every other guest instance in the process — and this test runner — with it. A test
/// cannot assert "the host did not abort", because there would be nothing left to assert it; what
/// it can assert is the shape that makes aborting impossible, which is a value carrying the reason.
#[test]
fn abort_and_exit_become_typed_outcomes_rather_than_ending_the_host() {
    let _guard = serialized();
    let f = fixture();

    let silent = refusal_of(&f, "abort", |_| {});
    assert_eq!(silent.symbol(), Some("abort"));
    assert!(matches!(silent, AbiError::GuestAborted { .. }), "{silent:?}");
    assert!(silent.to_string().contains("set no abort message"), "{silent}");

    // The message bionic's crash reporter would have printed travels with the abort.
    let message = f.cstring(f.guest.data + 0x100, b"terminating with uncaught exception");
    value_of(&f, "android_set_abort_message", |asm| { asm.mov(0, message as u64); });
    assert_eq!(
        f.bionic.abort_message().as_deref(),
        Some("terminating with uncaught exception")
    );
    let spoken = refusal_of(&f, "abort", |_| {});
    assert!(spoken.to_string().contains("uncaught exception"), "{spoken}");

    // The stack protector is an abort with its own reason, so a reader is not left thinking the
    // guest chose to exit.
    let smashed = refusal_of(&f, "__stack_chk_fail", |_| {});
    assert_eq!(smashed.symbol(), Some("__stack_chk_fail"));
    assert!(matches!(smashed, AbiError::GuestAborted { .. }), "{smashed:?}");
    assert!(smashed.to_string().contains("canary"), "{smashed}");

    // A null message clears it.
    value_of(&f, "android_set_abort_message", |asm| { asm.mov(0, 0); });
    assert_eq!(f.bionic.abort_message(), None);

    // `_exit` carries its status, and a zero status is still an exit rather than a success.
    for status in [42i64, 0, -1] {
        let exited = refusal_of(&f, "_exit", |asm| { asm.mov(0, status as u64); });
        assert_eq!(exited.symbol(), Some("_exit"));
        match exited {
            AbiError::GuestExited { status: reported, .. } => {
                assert_eq!(i64::from(reported), status);
            }
            other => panic!("{other:?}"),
        }
    }
}

/// **What the four process calls answer now, and what they still refuse.**
///
/// Phase 3a refused all four outright. M3's gate is what changed three of them, and it changed
/// them with evidence out of the guest rather than out of a header — `tools/call_sites.py` decodes
/// the arguments every direct call site passes. What each still refuses is the half with a
/// believable wrong answer sitting next to it, and the refusal names the value it was given.
#[test]
fn the_process_symbols_answer_what_is_known_and_refuse_the_rest() {
    let _guard = serialized();
    let f = fixture();

    // **`sysconf` answers the page size**, from the same source `getauxval(AT_PAGESZ)` answers
    // from, so the two cannot disagree. Both of bionic's two spellings, which is the corroboration
    // that licensed the numbering: bionic is the one libc where they are different values.
    let page = f.bionic.space_page_size() as u64;
    assert_eq!(value_of(&f, "sysconf", |asm| { asm.mov(0, 0x27); }), page, "_SC_PAGESIZE");
    assert_eq!(value_of(&f, "sysconf", |asm| { asm.mov(0, 0x28); }), page, "_SC_PAGE_SIZE");
    let cpus = value_of(&f, "sysconf", |asm| { asm.mov(0, 0x61); });
    assert!(cpus >= 1, "_SC_NPROCESSORS_ONLN must be at least one: {cpus}");
    assert_eq!(value_of(&f, "sysconf", |asm| { asm.mov(0, 0x60); }), cpus, "_SC_NPROCESSORS_CONF");
    // And still refuses the ones it does not know, naming the number and what it is believed to
    // be. `_SC_PHYS_PAGES` is refused **although a number is available**: it is the host's
    // physical memory, which is not the guest's budget.
    let phys = refusal_of(&f, "sysconf", |asm| { asm.mov(0, 0x62); });
    assert_eq!(phys.symbol(), Some("sysconf"));
    assert!(phys.to_string().contains("_SC_PHYS_PAGES"), "{phys}");
    let unknown = refusal_of(&f, "sysconf", |asm| { asm.mov(0, 4242); });
    assert!(unknown.to_string().contains("4242"), "{unknown}");

    // **`sysinfo` refuses until the embedding says how much memory the guest has**, naming the
    // method that supplies it. That is the same shape as the filesystem root and the thread host.
    let info = refusal_of(&f, "sysinfo", |asm| {
        asm.mov(0, (f.guest.data + 0x200) as u64);
    });
    assert_eq!(info.symbol(), Some("sysinfo"));
    assert!(info.to_string().contains("set_memory_budget"), "{info}");

    // **`prctl` answers `PR_SET_VMA` by keeping the label**, which is that call's entire
    // observable effect on a device, and answers the two transparent-huge-page options with
    // `EINVAL`, which is what a kernel without `CONFIG_TRANSPARENT_HUGEPAGE` answers.
    let label = f.cstring(f.guest.data + 0x300, b"roblox-heap");
    let result = value_of(&f, "prctl", |asm| {
        asm.mov(0, 0x5356_4d41); // PR_SET_VMA
        asm.mov(1, 0); // PR_SET_VMA_ANON_NAME
        asm.mov(2, 0x1000);
        asm.mov(3, 0x2000);
        asm.mov(4, label as u64);
    });
    assert_eq!(result, 0, "PR_SET_VMA succeeds");
    assert_eq!(
        f.bionic.vma_names(),
        vec![((0x1000u64, 0x2000u64), "roblox-heap".to_string())],
        "the label is kept, which is the whole of what the call does on a device"
    );
    for option in [41i64, 42] {
        let value = value_of(&f, "prctl", |asm| {
            asm.mov(0, option as u64);
            asm.mov(1, 0);
        });
        assert_eq!(value as i64 as i32, -1, "PR_*_THP_DISABLE is EINVAL without huge pages");
    }
    // And still refuses everything else, naming the option.
    let named = refusal_of(&f, "prctl", |asm| {
        asm.mov(0, 15); // PR_SET_NAME
        asm.mov(1, (f.guest.data + 0x100) as u64);
    });
    assert_eq!(named.symbol(), Some("prctl"));
    assert!(named.to_string().contains("PR_SET_NAME"), "{named}");

    // **`syscall` answers `gettid` with this thread's identity** — the whole contract of that
    // call is a value no other live thread has, and a `GuestThreadId` is exactly that.
    // The activation is held across the call so the test can ask what the handler answered
    // *with*: `attach_current` gives one host thread one slot, so the id this sees is the id the
    // handler saw. Without it the guard inside `Fixture::run` would have been dropped by now and
    // the comparison would be against `None`.
    let tid = {
        let _active = f.bionic.activate().expect("a thread block");
        let tid = value_of(&f, "syscall", |asm| { asm.mov(0, 178); });
        assert_eq!(
            tid,
            f.bionic.current_thread().expect("attached").0,
            "gettid is this thread's identity, not an invented number"
        );
        tid
    };
    assert!(tid > 0, "a thread identity of zero is indistinguishable from an unset one");
    // And still refuses a number it does not model, naming it.
    let nameless = refusal_of(&f, "syscall", |asm| { asm.mov(0, 100_000); });
    assert_eq!(nameless.symbol(), Some("syscall"));
    let text = nameless.to_string();
    assert!(text.contains("100000"), "{text}");
    assert!(text.contains("ENOSYS"), "the refusal must say why -1/ENOSYS was rejected: {text}");
}

/// **`syscall(SYS_rt_sigprocmask)` is the engine's pointer-readability probe, and it answers it.**
///
/// The engine passes `how = -1` deliberately: the kernel validates `sigsetsize`, then the `set`
/// pointer — `EFAULT` if it cannot be read — and only then rejects `how` with `EINVAL`. So the
/// errno the call fails with is a precise answer to "can this process read eight bytes there",
/// and the caller saves and restores `errno` around it. Both arms are asserted, because the
/// *difference* between them is the whole answer.
#[test]
fn the_rt_sigprocmask_pointer_probe_tells_a_readable_address_from_an_unreadable_one() {
    let _guard = serialized();
    let f = fixture();
    let set = f.guest.data + 0x400;
    f.guest.write_u64(set, 0);

    let probe = |at: u64| -> i32 {
        let entry = {
            let thunk = f.thunk("syscall");
            let entry = f.guest.next_entry();
            let mut asm = Asm::at(entry);
            asm.push(mov_reg(21, 30));
            asm.mov(0, 135); // SYS_rt_sigprocmask
            asm.mov(1, u64::from(u32::MAX)); // how = -1, invalid on purpose
            asm.mov(2, at);
            asm.mov(3, 0); // oldset = NULL
            asm.mov(4, 8); // sigsetsize
            asm.bl(thunk);
            asm.mov(22, f.guest.data as u64);
            asm.push(str_imm(0, 22, 0));
            asm.push(ret(21));
            f.guest.load(asm.words());
            entry
        };
        let mut cpu = f.guest.thread(&f.boundary);
        let exit = f.run(&mut cpu, entry).expect("the probe must complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        assert_eq!(f.guest.read_u64(f.guest.data) as i64 as i32, -1, "the probe always fails");
        // `errno`, read the way the guest reads it.
        let _active = f.bionic.activate().expect("a thread block");
        f.guest.read_u64(f.bionic.arena()) as u32 as i32
    };

    assert_eq!(probe(set as u64), 22, "a readable `set` reaches the `how` check: EINVAL");
    assert_eq!(probe(f.guest.unmapped as u64), 14, "an unreadable `set` is EFAULT");
}

// ------------------------------------------------------------------ the log sink

/// **`__android_log_print` runs the real `printf` engine**, and the record keeps the priority and
/// the tag the guest gave.
///
/// A line reading `%s at %p` with the arguments dropped would be worse than no line, so the
/// formatting is the same engine `snprintf` uses rather than a passthrough of the format string.
#[test]
fn android_log_print_formats_through_the_real_printf_engine() {
    let _guard = serialized();
    let f = fixture();
    let tag = f.cstring(f.guest.data + 0x100, b"Roblox");
    let fmt = f.cstring(f.guest.data + 0x140, b"n=%d s=%s");
    let text = f.cstring(f.guest.data + 0x180, b"hi");

    let written = value_of(&f, "__android_log_print", |asm| {
        asm.mov(0, 4); // ANDROID_LOG_INFO
        asm.mov(1, tag as u64);
        asm.mov(2, fmt as u64);
        asm.mov(3, 7);
        asm.mov(4, text as u64);
    }) as i64 as i32;

    let records = f.bionic.log_records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].priority, LogPriority::Info);
    assert_eq!(records[0].tag, "Roblox");
    assert_eq!(records[0].message, "n=7 s=hi");
    assert_eq!(written, records[0].message.len() as i32, "the return is the message's length");

    // A null tag is an empty tag, not a fault.
    value_of(&f, "__android_log_print", |asm| {
        asm.mov(0, 6); // ANDROID_LOG_ERROR
        asm.mov(1, 0);
        asm.mov(2, text as u64);
    });
    let records = f.bionic.log_records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].tag, "");
    assert_eq!(records[1].priority, LogPriority::Error);

    // A priority outside the scale is a refusal, not a mapping to its nearest neighbour.
    let error = refusal_of(&f, "__android_log_print", |asm| {
        asm.mov(0, 42);
        asm.mov(1, tag as u64);
        asm.mov(2, text as u64);
    });
    assert_eq!(error.symbol(), Some("__android_log_print"));
    assert!(error.to_string().contains("42"), "{error}");
    assert_eq!(f.bionic.log_records().len(), 2, "a refused line must not be recorded");
}

/// `syslog` takes its tag from `openlog`, keeps the facility, and `closelog` clears it.
#[test]
fn syslog_takes_its_tag_from_openlog_and_closelog_clears_it() {
    let _guard = serialized();
    let f = fixture();
    let ident = f.cstring(f.guest.data + 0x100, b"omnidroid");
    let fmt = f.cstring(f.guest.data + 0x140, b"x=%d");

    // LOG_USER (1 << 3) | LOG_WARNING (4).
    value_of(&f, "openlog", |asm| {
        asm.mov(0, ident as u64);
        asm.mov(1, 0x01); // LOG_PID, read and not acted on
        asm.mov(2, 8); // LOG_USER
    });
    assert_eq!(f.bionic.syslog_ident().as_deref(), Some("omnidroid"));
    value_of(&f, "syslog", |asm| {
        asm.mov(0, 8 | 4);
        asm.mov(1, fmt as u64);
        asm.mov(2, 5);
    });
    let records = f.bionic.log_records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].priority, LogPriority::Warn, "LOG_WARNING maps to WARN");
    assert_eq!(records[0].tag, "omnidroid[facility 8]", "the facility is carried, not dropped");
    assert_eq!(records[0].message, "x=5");

    value_of(&f, "closelog", |_| {});
    assert_eq!(f.bionic.syslog_ident(), None);
    value_of(&f, "syslog", |asm| {
        asm.mov(0, 3); // LOG_ERR, facility 0
        asm.mov(1, fmt as u64);
        asm.mov(2, 9);
    });
    let records = f.bionic.log_records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].priority, LogPriority::Error);
    assert_eq!(records[1].tag, "syslog", "with no ident and no facility, the tag names the call");
    assert_eq!(records[1].message, "x=9");
}

/// **The capture ring is bounded, and it says how much it dropped.**
///
/// How much the engine logs during initialisation has not been measured, so an unbounded ring is a
/// host allocation a guest can drive in a loop. The assertion is on *membership* rather than only
/// on the count: each line carries its own number, so the surviving window is checked at both ends
/// — a ring that dropped the newest records instead of the oldest would keep exactly the same
/// number of them.
///
/// Driven from a guest loop rather than 266 separate runs, so it is one translation and 266 real
/// thunk crossings.
#[test]
fn the_log_capture_ring_is_bounded_and_reports_what_it_dropped() {
    let _guard = serialized();
    let f = fixture();
    let rounds: u64 = LOG_CAPTURE_MAX as u64 + 10;
    let tag = f.cstring(f.guest.data + 0x100, b"loop");
    let fmt = f.cstring(f.guest.data + 0x140, b"n=%d");
    let thunk = f.thunk("__android_log_print");

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    // X22 is callee-saved, so the handler cannot disturb it.
    asm.mov(22, rounds);
    let loop_start = asm.pc();
    asm.mov(0, 4); // ANDROID_LOG_INFO
    asm.mov(1, tag as u64);
    asm.mov(2, fmt as u64);
    asm.push(mov_reg(3, 22)); // the variadic `%d`
    asm.bl(thunk);
    asm.push(subs_imm(22, 22, 1));
    let here = asm.pc();
    let back = ((loop_start as i64 - here as i64) / 4) as i32;
    asm.push(b_cond(1 /* NE */, back));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    let exit = f.run(&mut cpu, entry).expect("the loop must complete");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");

    let records = f.bionic.log_records();
    // Pinned as a number as well as as a symbol: a test written only against the constant would
    // pass for any cap at all, including one too small to hold a run's worth of lines.
    assert_eq!(LOG_CAPTURE_MAX, 256);
    assert_eq!(records.len(), LOG_CAPTURE_MAX, "the ring is bounded");
    assert_eq!(f.bionic.log_dropped(), rounds - LOG_CAPTURE_MAX as u64, "and says what it dropped");
    // The loop counts down, so the *last* record is n=1 and the oldest survivor is n=256.
    assert_eq!(records[0].message, format!("n={LOG_CAPTURE_MAX}"), "the oldest survivor");
    assert_eq!(records[LOG_CAPTURE_MAX - 1].message, "n=1", "the newest");
}

// ------------------------------------------------------------------ hostile arguments

/// **Every pointer and length in the phase 3a group, hostile**, and none of them panics.
///
/// A panic or an abort reachable from guest-supplied arguments is Critical. Each case here either
/// completes with a defined `-1`/`0`/`NULL` or refuses by name; nothing else is acceptable, and in
/// particular nothing may report success having written somewhere it should not.
#[test]
fn hostile_arguments_to_the_clock_and_process_group_are_typed_errors_and_not_panics() {
    let _guard = serialized();
    let f = fixture();
    let wild = f.guest.unmapped as u64;
    // A `struct timespec` asking for zero time, so `nanosleep`'s hostile cases test the *pointer*
    // rather than spending a minute asleep.
    let zero_req = f.guest.data + 0x300;
    f.guest.write_u64(zero_req, 0);
    f.guest.write_u64(zero_req + 8, 0);

    let cases: &[(&str, &[u64])] = &[
        ("clock_gettime", &[1, 0]),
        ("clock_gettime", &[1, wild]),
        ("clock_gettime", &[1, u64::MAX]),
        ("clock_gettime", &[1, u64::MAX - 4]),
        ("gettimeofday", &[wild, 0]),
        ("gettimeofday", &[0, wild]),
        ("gettimeofday", &[u64::MAX, u64::MAX]),
        ("gmtime_r", &[0, 0]),
        ("gmtime_r", &[wild, wild]),
        ("gmtime_r", &[u64::MAX, u64::MAX]),
        ("nanosleep", &[0, 0]),
        ("nanosleep", &[wild, 0]),
        ("nanosleep", &[u64::MAX, u64::MAX]),
        ("nanosleep", &[zero_req as u64, wild]),
        ("arc4random_buf", &[0, 64]),
        ("arc4random_buf", &[wild, 64]),
        ("arc4random_buf", &[wild, u64::MAX]),
        ("arc4random_buf", &[u64::MAX, u64::MAX]),
        ("getenv", &[wild]),
        ("getenv", &[u64::MAX]),
        ("__system_property_get", &[wild, wild]),
        ("__system_property_get", &[0, 0]),
        ("__system_property_get", &[0, u64::MAX]),
        ("android_set_abort_message", &[wild]),
        ("android_set_abort_message", &[u64::MAX]),
        ("getauxval", &[u64::MAX]),
        ("sysconf", &[u64::MAX]),
        ("sysinfo", &[wild]),
        ("prctl", &[u64::MAX, u64::MAX, u64::MAX]),
        ("syscall", &[u64::MAX, u64::MAX]),
        ("__android_log_print", &[4, wild, wild]),
        ("__android_log_print", &[4, 0, 0]),
        ("__android_log_print", &[u64::MAX, 0, 0]),
        ("syslog", &[0, 0]),
        ("syslog", &[0, wild]),
        ("openlog", &[wild, 0, 0]),
        ("openlog", &[u64::MAX, 0, 0]),
    ];

    for (symbol, args) in cases {
        let entry = call_one(&f, symbol, |asm| {
            for (index, value) in args.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
        });
        let mut cpu = f.guest.thread(&f.boundary);
        match f.run(&mut cpu, entry) {
            Ok(exit) => {
                assert!(
                    matches!(exit, ExitReason::Returned { .. }),
                    "`{symbol}` {args:x?}: {exit:?}"
                );
                let code = f.guest.read_u64(f.guest.data) as i64 as i32;
                assert!(
                    code == 0 || code == -1,
                    "`{symbol}` {args:x?} completed with {code}, which is neither a success nor \
                     a defined failure"
                );
            }
            Err(error) => {
                assert_eq!(error.symbol(), Some(*symbol), "{error:?}");
                assert!(error.guest_address().is_some(), "{error}");
            }
        }
    }
}

/// **Hostile arguments to every symbol phase 3c binds**, including the ones that create threads.
///
/// A guest that creates threads is a new hostile surface, and the shapes are its own: a null or
/// wild `pthread_t *`, an attribute object that is not readable, a `pthread_t` nobody handed out,
/// and a `sigset_t` that is not writable. Every one of them has to be a typed error naming the
/// symbol or a defined return, never a host access violation and never a panic.
///
/// **No thread host here**, deliberately: this fixture cannot create a thread at all, so every
/// `pthread_create` case exercises the argument handling and the refusal rather than spawning 30
/// host threads with wild arguments. The cases that need a live thread are the tests above.
#[test]
fn hostile_arguments_to_the_thread_and_signal_group_are_typed_errors_and_not_panics() {
    let _guard = serialized();
    let f = fixture();
    let wild = f.guest.unmapped as u64;
    let readonly = f.guest.readonly as u64;

    let cases: &[(&str, &[u64])] = &[
        // `pthread_create(thread, attr, start, arg)`
        ("pthread_create", &[0, 0, 1, 0]),
        ("pthread_create", &[wild, 0, 1, 0]),
        ("pthread_create", &[u64::MAX, 0, 1, 0]),
        ("pthread_create", &[readonly, 0, 1, 0]),
        ("pthread_create", &[0, 0, 0, 0]),
        ("pthread_create", &[wild, wild, wild, wild]),
        ("pthread_create", &[u64::MAX, u64::MAX, u64::MAX, u64::MAX]),
        // `pthread_join(thread, retval)`
        ("pthread_join", &[0, 0]),
        ("pthread_join", &[u64::MAX, 0]),
        ("pthread_join", &[0, wild]),
        ("pthread_join", &[u64::MAX, u64::MAX]),
        ("pthread_join", &[0xDEAD_BEEF, readonly]),
        // `pthread_detach(thread)`
        ("pthread_detach", &[0]),
        ("pthread_detach", &[u64::MAX]),
        // `pthread_getschedparam(thread, policy, param)`
        ("pthread_getschedparam", &[0, 0, 0]),
        ("pthread_getschedparam", &[u64::MAX, wild, wild]),
        ("pthread_getschedparam", &[0xDEAD_BEEF, u64::MAX, u64::MAX]),
        // the signal family
        ("sigfillset", &[0]),
        ("sigfillset", &[wild]),
        ("sigfillset", &[u64::MAX]),
        ("sigfillset", &[u64::MAX - 4]),
        ("sigfillset", &[readonly]),
        ("sigaction", &[0, 0, 0]),
        ("sigaction", &[u64::MAX, wild, wild]),
        ("raise", &[0]),
        ("raise", &[u64::MAX]),
        ("pthread_sigmask", &[u64::MAX, wild, wild]),
        ("pthread_sigmask", &[0, 0, 0]),
    ];

    for (symbol, args) in cases {
        let entry = call_one(&f, symbol, |asm| {
            for (index, value) in args.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
        });
        let mut cpu = f.guest.thread(&f.boundary);
        match f.run(&mut cpu, entry) {
            Ok(exit) => {
                assert!(
                    matches!(exit, ExitReason::Returned { .. }),
                    "`{symbol}` {args:x?}: {exit:?}"
                );
                let code = f.guest.read_u64(f.guest.data) as i64 as i32;
                // The pthread functions return their error as the value, so the defined set here
                // is wider than the errno group's: 0, -1, or one of the four POSIX numbers this
                // group answers with. Anything else is a number nobody decided on.
                assert!(
                    code == 0 || code == -1 || [3, 11, 22, 35].contains(&code),
                    "`{symbol}` {args:x?} completed with {code}, which is neither a success nor \
                     a defined failure"
                );
            }
            Err(error) => {
                assert_eq!(error.symbol(), Some(*symbol), "{error:?}");
                assert!(error.guest_address().is_some(), "{error}");
            }
        }
    }
    // Nothing above may have created a thread: the fixture has no thread host at all.
    assert_eq!(f.bionic.guest_thread_records(), 0);
    assert!(f.bionic.guest_thread_failures().is_empty());
}

// =================================================================== phase 3b: files

/// A host directory that removes itself, for the guest's filesystem root.
///
/// Built without a dependency, and with the process id and thread id in its name so that two
/// tests -- and two `cargo test` processes -- cannot collide on it.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!(
            "omni-bionic-fs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        Scratch(at)
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fixture whose instance has a filesystem rooted at a fresh scratch directory.
///
/// Built on `fixture_with`, which also places the eighteen data objects — because `__sF` has to
/// exist before `stdin`, `stdout` and `stderr` can be streams over descriptors 0, 1 and 2.
fn rooted(tag: &str) -> (Fixture, Scratch) {
    let scratch = Scratch::new(tag);
    let f = fixture_with(&[]);
    f.bionic.set_filesystem_root(&scratch.0).expect("a filesystem root");
    (f, scratch)
}

/// Read `len` bytes out of guest memory.
fn read_guest(f: &Fixture, at: omni_cpu::GuestAddr, len: usize) -> Vec<u8> {
    (0..len)
        .map(|offset| {
            let address = at + offset;
            (f.guest.read_u64(address & !7) >> (8 * (address & 7))) as u8
        })
        .collect()
}

/// A little-endian `u32` read out of guest memory.
fn read_u32_guest(f: &Fixture, at: omni_cpu::GuestAddr) -> u32 {
    u32::from_le_bytes(read_guest(f, at, 4).try_into().expect("four bytes"))
}

/// A little-endian `u64` read out of guest memory at an arbitrary alignment.
fn read_u64_guest(f: &Fixture, at: omni_cpu::GuestAddr) -> u64 {
    u64::from_le_bytes(read_guest(f, at, 8).try_into().expect("eight bytes"))
}

// The guest's own `O_*`, spelled again in the test so that the constants in `files.rs` are
// compared against a second copy rather than against themselves.
const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_DIRECTORY: u64 = 0o200000;

/// **An instance with no filesystem root refuses every path call, by name.**
///
/// The default is the confinement property: there is no root until the embedding names one, so
/// there is no way for a guest to reach a host file by accident. A refusal rather than `ENOENT`,
/// because "this runtime was not configured" and "that file is not there" are different problems
/// and only one of them is the guest's.
#[test]
fn a_guest_with_no_filesystem_root_refuses_every_path_call_by_name() {
    let _guard = serialized();
    let f = fixture();
    let path = f.cstring(f.guest.data + 0x100, b"/data/anything");
    for symbol in ["open", "stat", "lstat", "access", "unlink", "rmdir", "opendir", "statvfs"] {
        let error = refusal_of(&f, symbol, |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, 0);
        });
        assert_eq!(error.symbol(), Some(symbol), "{error:?}");
        let text = error.to_string();
        assert!(
            text.contains("set_filesystem_root"),
            "`{symbol}` must name the method that would supply a root: {text}"
        );
    }
}

/// A file created, written and read back **through real translated guest code**.
///
/// Five calls, each its own guest program: `open`, `__write_chk`, `close`, `open`, `read`. The
/// assertion is on the *bytes*, read out of guest memory afterwards, so a handler that returned a
/// plausible count without moving anything fails it.
#[test]
fn a_file_is_created_written_and_read_back_through_real_guest_code() {
    let _guard = serialized();
    let (f, scratch) = rooted("roundtrip");
    let path = f.cstring(f.guest.data + 0x100, b"/hello.txt");
    let payload = b"the quick brown fox jumps over the lazy dog";
    let source = f.guest.data + 0x200;
    f.guest.write_bytes(source, payload);

    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_WRONLY | O_CREAT | O_TRUNC);
        asm.mov(2, 0o644);
    }) as i64;
    assert!(fd >= 3, "open returned {fd}");

    let written = value_of(&f, "__write_chk", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, source as u64);
        asm.mov(2, payload.len() as u64);
        asm.mov(3, payload.len() as u64);
    }) as i64;
    assert_eq!(written, payload.len() as i64);

    assert_eq!(value_of(&f, "close", |asm| { asm.mov(0, fd as u64); }) as i64, 0);
    // The host really has the file, with the bytes the guest wrote.
    assert_eq!(std::fs::read(scratch.path("hello.txt")).expect("the host file"), payload);

    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY);
        asm.mov(2, 0);
    }) as i64;
    assert_eq!(fd, 3, "the lowest free descriptor comes back after the close");
    let destination = f.guest.data + 0x400;
    let read = value_of(&f, "read", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, destination as u64);
        asm.mov(2, 256);
    }) as i64;
    assert_eq!(read, payload.len() as i64);
    assert_eq!(read_guest(&f, destination, payload.len()), payload);
    // A second read is end of file, which is zero and not an error.
    assert_eq!(
        value_of(&f, "read", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, destination as u64);
            asm.mov(2, 256);
        }) as i64,
        0
    );
    assert_eq!(value_of(&f, "close", |asm| { asm.mov(0, fd as u64); }) as i64, 0);
}

/// **The confinement property, through the boundary, against a bait file.**
///
/// A file is created *outside* the root and the guest is given every shape of path that would
/// reach it. The assertion afterwards is that the bait is still there and was never read: a
/// traversal that worked would show up as content rather than as a missing refusal.
#[test]
fn no_guest_path_can_reach_a_file_outside_the_instance_root() {
    let _guard = serialized();
    let outer = Scratch::new("outer");
    std::fs::write(outer.path("secret.txt"), b"HOST SECRET").expect("the bait");
    let root = outer.path("root");
    std::fs::create_dir_all(&root).expect("the guest root");
    let f = fixture();
    f.bionic.set_filesystem_root(&root).expect("a filesystem root");

    let attempts: &[&[u8]] = &[
        b"/../secret.txt",
        b"/../../secret.txt",
        b"../secret.txt",
        b"/a/../../secret.txt",
        b"/./../secret.txt",
        b"//../secret.txt",
        br"/..\secret.txt",
        b"/a/b/c/../../../../secret.txt",
        b"/NUL",
        b"/C:secret.txt",
        b"/secret.txt.",
    ];
    for (index, attempt) in attempts.iter().enumerate() {
        let path = f.cstring(f.guest.data + 0x100 + index * 0x40, attempt);
        let entry = call_one(&f, "open", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, O_RDONLY);
            asm.mov(2, 0);
        });
        let mut cpu = f.guest.thread(&f.boundary);
        match f.run(&mut cpu, entry) {
            // Refused outright: a traversal, a device name, an aliasing name.
            Err(error) => assert_eq!(error.symbol(), Some("open"), "{error:?}"),
            // Or absorbed: `/.. == /`, so the path named something inside the root that is not
            // there. `-1` is the answer, never a descriptor.
            Ok(_) => {
                let fd = f.guest.read_u64(f.guest.data) as i64 as i32;
                assert_eq!(
                    fd,
                    -1,
                    "`{}` opened descriptor {fd}",
                    String::from_utf8_lossy(attempt)
                );
            }
        }
    }
    assert_eq!(
        std::fs::read(outer.path("secret.txt")).expect("the bait survives"),
        b"HOST SECRET",
        "the bait was modified"
    );
}

/// **`struct stat`'s fields land where the guest reads them.**
///
/// The layout is ASSUMED — derived from Linux UAPI, with no NDK here to check it against — so the
/// test asserts against a file whose length this test chose. A layout that moved `st_size` would
/// report a number that is not 1,234.
#[test]
fn stat_lands_its_fields_where_the_guest_reads_them() {
    let _guard = serialized();
    let (f, scratch) = rooted("stat");
    std::fs::write(scratch.path("f"), vec![7u8; 1234]).expect("a file of known length");
    std::fs::create_dir(scratch.path("d")).expect("a directory");
    let path = f.cstring(f.guest.data + 0x100, b"/f");
    let buf = f.guest.data + 0x400;

    assert_eq!(
        value_of(&f, "stat", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, buf as u64);
        }) as i64,
        0
    );
    let mode = read_u32_guest(&f, buf + 16);
    assert_eq!(mode & omni_android::bionic::S_IFMT, omni_android::bionic::S_IFREG, "st_mode");
    assert_ne!(mode & 0o200, 0, "a writable file must report S_IWUSR");
    assert_eq!(read_u32_guest(&f, buf + 20), 1, "st_nlink");
    assert_eq!(read_u64_guest(&f, buf + 48), 1234, "st_size");
    assert_eq!(read_u32_guest(&f, buf + 56), 4096, "st_blksize is the size this layer transfers in");
    assert_eq!(read_u64_guest(&f, buf + 64), 1234u64.div_ceil(512), "st_blocks");
    assert_ne!(read_u64_guest(&f, buf + 8), 0, "st_ino must never be zero");
    assert_ne!(read_u64_guest(&f, buf), 0, "st_dev must never be zero");
    let file_ino = read_u64_guest(&f, buf + 8);

    // A directory, through `lstat`, which is the same encoder on a different call.
    let dir = f.cstring(f.guest.data + 0x140, b"/d");
    assert_eq!(
        value_of(&f, "lstat", |asm| {
            asm.mov(0, dir as u64);
            asm.mov(1, buf as u64);
        }) as i64,
        0
    );
    let mode = read_u32_guest(&f, buf + 16);
    assert_eq!(mode & omni_android::bionic::S_IFMT, omni_android::bionic::S_IFDIR);
    assert_ne!(read_u64_guest(&f, buf + 8), file_ino, "two paths must not share an inode");

    // And `fstat` on an open descriptor agrees with `stat` on its path.
    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY);
        asm.mov(2, 0);
    }) as i64;
    assert_eq!(
        value_of(&f, "fstat", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, buf as u64);
        }) as i64,
        0
    );
    assert_eq!(read_u64_guest(&f, buf + 48), 1234, "fstat's st_size");
    assert_eq!(read_u64_guest(&f, buf + 8), file_ino, "one file, one inode");
    assert_eq!(value_of(&f, "close", |asm| { asm.mov(0, fd as u64); }) as i64, 0);

    // A path that is not there is `-1`, not a zeroed structure reported as a success.
    let missing = f.cstring(f.guest.data + 0x180, b"/nope");
    assert_eq!(
        value_of(&f, "stat", |asm| {
            asm.mov(0, missing as u64);
            asm.mov(1, buf as u64);
        }) as i64,
        -1
    );
}

/// `statvfs` fills the guest's structure with the host volume's own numbers.
///
/// **Structural rather than numeric**: the relations that must hold for any volume, because a
/// test that pinned a free-space figure would be asserting about this machine's disk. A fabricated
/// filesystem has no reason to satisfy them.
#[test]
fn statvfs_fills_the_guests_structure_with_the_hosts_numbers() {
    let _guard = serialized();
    let (f, _scratch) = rooted("statvfs");
    let path = f.cstring(f.guest.data + 0x100, b"/");
    let buf = f.guest.data + 0x400;
    let entry = call_one(&f, "statvfs", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, buf as u64);
    });
    let mut cpu = f.guest.thread(&f.boundary);
    match f.run(&mut cpu, entry) {
        Err(error) => {
            // The only acceptable failure is the structural refusal on a target with no backend.
            assert!(error.to_string().contains("statvfs(3)"), "{error}");
            return;
        }
        Ok(_) => assert_eq!(f.guest.read_u64(f.guest.data) as i64, 0),
    }
    let field = |index: usize| read_u64_guest(&f, buf + index * 8);
    assert!(field(0) > 0 && field(0).is_power_of_two(), "f_bsize is {}", field(0));
    assert_eq!(field(1), field(0), "f_frsize is the allocation unit too on this host");
    assert!(field(2) > 0, "f_blocks");
    assert!(field(3) <= field(2), "f_bfree > f_blocks");
    assert!(field(4) <= field(3), "f_bavail > f_bfree");
    assert_eq!((field(5), field(6), field(7)), (0, 0, 0), "the three inode counts");
    assert!(field(10) > 0, "f_namemax");
}

/// A directory walk: `opendir`, `readdir` to the end, `closedir`.
///
/// `.` and `..` come first and every entry appears exactly once. The `DIR *` and the returned
/// `struct dirent *` are the same address, which is what makes the "valid until the next call"
/// contract true by construction.
#[test]
fn a_directory_walk_returns_dot_dotdot_and_every_entry_once() {
    let _guard = serialized();
    let (f, scratch) = rooted("dir");
    std::fs::create_dir(scratch.path("d")).expect("a directory");
    for name in ["alpha", "beta"] {
        std::fs::write(scratch.path(&format!("d/{name}")), b"x").expect("a file");
    }
    std::fs::create_dir(scratch.path("d/sub")).expect("a subdirectory");
    let path = f.cstring(f.guest.data + 0x100, b"/d");

    let dirp = value_of(&f, "opendir", |asm| { asm.mov(0, path as u64); });
    assert_ne!(dirp, 0, "opendir returned NULL");
    assert_eq!(f.bionic.open_dirs(), 1);

    let mut names = Vec::new();
    for _ in 0..8 {
        let entry = value_of(&f, "readdir", |asm| { asm.mov(0, dirp); });
        if entry == 0 {
            break;
        }
        assert_eq!(entry, dirp, "readdir must return the DIR's own dirent slot");
        let ino = read_u64_guest(&f, entry as omni_cpu::GuestAddr);
        assert_ne!(ino, 0, "d_ino must never be zero");
        let reclen = u16::from_le_bytes(
            read_guest(&f, entry as omni_cpu::GuestAddr + 16, 2).try_into().unwrap(),
        );
        assert_eq!(reclen, 280, "d_reclen");
        let kind = read_guest(&f, entry as omni_cpu::GuestAddr + 18, 1)[0];
        let name = f.read_cstring(entry as omni_cpu::GuestAddr + 19);
        names.push((String::from_utf8(name).expect("a UTF-8 name"), kind));
    }
    assert_eq!(names[0], (".".to_string(), 4), "DT_DIR");
    assert_eq!(names[1], ("..".to_string(), 4));
    let mut rest: Vec<_> = names[2..].to_vec();
    rest.sort();
    assert_eq!(
        rest,
        vec![
            ("alpha".to_string(), 8u8),
            ("beta".to_string(), 8),
            ("sub".to_string(), 4),
        ],
        "DT_REG is 8 and DT_DIR is 4"
    );
    assert_eq!(value_of(&f, "closedir", |asm| { asm.mov(0, dirp); }) as i64, 0);
    assert_eq!(f.bionic.open_dirs(), 0);
    // A `DIR *` that has been closed is `EBADF`, not a second walk of the same directory.
    assert_eq!(value_of(&f, "closedir", |asm| { asm.mov(0, dirp); }) as i64, -1);
}

/// **The three standard streams are streams**, with the descriptors POSIX reserves.
///
/// `stdin`, `stdout` and `stderr` are data objects pointing into `__sF`, and phase 3b registers
/// each of the three as a stream over descriptor 0, 1 and 2. `fileno` answering 1 for `stdout` is
/// what proves the registration, the `FILE_BYTES` spacing and the declaration order agree.
#[test]
fn the_three_standard_streams_are_streams_over_the_descriptors_posix_reserves() {
    let _guard = serialized();
    let (f, _scratch) = rooted("std");
    let sf = f
        .boundary
        .slot_named("__sF")
        .expect("__sF is declared")
        .address;
    for (index, expected) in [(0usize, 0i64), (1, 1), (2, 2)] {
        let stream = sf + index * omni_android::bionic::FILE_BYTES;
        assert_eq!(
            value_of(&f, "fileno", |asm| { asm.mov(0, stream as u64); }) as i64,
            expected,
            "`__sF[{index}]` must be descriptor {expected}"
        );
        assert_eq!(value_of(&f, "feof", |asm| { asm.mov(0, stream as u64); }) as i64, 0);
        // `fflush` on a standard stream reaches the host's own buffered writer.
        assert_eq!(value_of(&f, "fflush", |asm| { asm.mov(0, stream as u64); }) as i64, 0);
    }
    // `fflush(NULL)` walks every stream and succeeds.
    assert_eq!(value_of(&f, "fflush", |asm| { asm.mov(0, 0); }) as i64, 0);
    // And the `stdout` data object really points at `__sF[1]`.
    let stdout_cell = f.boundary.slot_named("stdout").expect("stdout is declared").address;
    assert_eq!(
        f.guest.read_u64(stdout_cell),
        (sf + omni_android::bionic::FILE_BYTES) as u64,
        "stdout must point at __sF[1]"
    );
}

/// **`fseeko` moves the stream, clears its end-of-file indicator, and `ftello` reports where it
/// is** -- through real guest code, with the bytes that come back as the evidence.
///
/// The end-of-file half is the one a plausible implementation misses: a seek that moved the
/// descriptor and left `feof` sticky would read correctly and report end of file for ever after.
#[test]
fn fseeko_moves_the_stream_and_clears_end_of_file_and_ftello_reports_it() {
    let _guard = serialized();
    let (f, scratch) = rooted("fseeko");
    std::fs::write(scratch.path("seek.txt"), b"0123456789").expect("a host file");
    let path = f.cstring(f.guest.data + 0x100, b"/seek.txt");
    let mode = f.cstring(f.guest.data + 0x140, b"r");
    let stream = value_of(&f, "fopen", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, mode as u64);
    });
    assert_ne!(stream, 0, "fopen returned NULL");
    let buf = f.guest.data + 0x400;
    let fgets = |len: u64| {
        value_of(&f, "fgets", |asm| {
            asm.mov(0, buf as u64);
            asm.mov(1, len);
            asm.mov(2, stream);
        })
    };
    // Read to the end, so the indicator is set.
    assert_eq!(fgets(64), buf as u64);
    assert_eq!(fgets(64), 0, "end of file");
    assert_eq!(value_of(&f, "feof", |asm| { asm.mov(0, stream); }) as i64, 1);
    assert_eq!(value_of(&f, "ftello", |asm| { asm.mov(0, stream); }) as i64, 10, "at the end");

    let seek = |offset: i64, whence: u64| {
        value_of(&f, "fseeko", |asm| {
            asm.mov(0, stream);
            asm.mov(1, offset as u64);
            asm.mov(2, whence);
        }) as i64
    };
    assert_eq!(seek(-4, 2), 0, "SEEK_END - 4");
    assert_eq!(value_of(&f, "feof", |asm| { asm.mov(0, stream); }) as i64, 0, "cleared");
    assert_eq!(value_of(&f, "ftello", |asm| { asm.mov(0, stream); }) as i64, 6);
    assert_eq!(fgets(3), buf as u64);
    assert_eq!(f.read_cstring(buf), b"67", "the read starts where the seek put it");
    assert_eq!(seek(1, 1), 0, "SEEK_CUR + 1");
    assert_eq!(value_of(&f, "ftello", |asm| { asm.mov(0, stream); }) as i64, 9);
    assert_eq!(seek(-1, 0), -1, "a negative position is refused, as EINVAL");
    assert_eq!(value_of(&f, "ftello", |asm| { asm.mov(0, stream); }) as i64, 9, "and moved nothing");
    assert_eq!(value_of(&f, "fclose", |asm| { asm.mov(0, stream); }) as i64, 0);
}

/// A `FILE *` round trip: `fopen`, `fputs`, `fclose`, `fopen`, `fgets`, `feof`, `fclose`.
#[test]
fn a_file_stream_round_trips_through_fopen_fputs_fgets_and_fclose() {
    let _guard = serialized();
    let (f, scratch) = rooted("stream");
    let path = f.cstring(f.guest.data + 0x100, b"/lines.txt");
    let write_mode = f.cstring(f.guest.data + 0x140, b"w");
    let read_mode = f.cstring(f.guest.data + 0x160, b"r");
    let line = f.cstring(f.guest.data + 0x200, b"first line\nsecond line\n");

    let stream = value_of(&f, "fopen", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, write_mode as u64);
    });
    assert_ne!(stream, 0, "fopen returned NULL");
    assert_eq!(f.bionic.open_streams(), 4, "the three standard streams plus this one");
    assert_eq!(
        value_of(&f, "fputs", |asm| {
            asm.mov(0, line as u64);
            asm.mov(1, stream);
        }) as i64,
        23,
        "fputs returns the byte count bionic returns, and the two lines are 11 + 12 bytes"
    );
    assert_eq!(value_of(&f, "fclose", |asm| { asm.mov(0, stream); }) as i64, 0);
    assert_eq!(f.bionic.open_streams(), 3, "the slot is released");
    assert_eq!(
        std::fs::read(scratch.path("lines.txt")).expect("the host file"),
        b"first line\nsecond line\n"
    );

    let stream = value_of(&f, "fopen", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, read_mode as u64);
    });
    let buf = f.guest.data + 0x400;
    let got = value_of(&f, "fgets", |asm| {
        asm.mov(0, buf as u64);
        asm.mov(1, 64);
        asm.mov(2, stream);
    });
    assert_eq!(got, buf as u64, "fgets returns its buffer");
    assert_eq!(f.read_cstring(buf), b"first line\n", "the newline is kept");
    let got = value_of(&f, "fgets", |asm| {
        asm.mov(0, buf as u64);
        asm.mov(1, 64);
        asm.mov(2, stream);
    });
    assert_eq!(got, buf as u64);
    assert_eq!(f.read_cstring(buf), b"second line\n", "and the second line is intact");
    assert_eq!(value_of(&f, "feof", |asm| { asm.mov(0, stream); }) as i64, 0, "not at the end yet");
    assert_eq!(
        value_of(&f, "fgets", |asm| {
            asm.mov(0, buf as u64);
            asm.mov(1, 64);
            asm.mov(2, stream);
        }),
        0,
        "the third call is end of file: NULL"
    );
    assert_eq!(value_of(&f, "feof", |asm| { asm.mov(0, stream); }) as i64, 1, "and feof sticks");
    assert_eq!(value_of(&f, "fclose", |asm| { asm.mov(0, stream); }) as i64, 0);
}

/// `fread` and `fwrite` report whole items, and `fdopen` puts a stream over a descriptor.
#[test]
fn fread_and_fwrite_report_whole_items_over_a_descriptor_fdopen_adopted() {
    let _guard = serialized();
    let (f, scratch) = rooted("items");
    std::fs::write(scratch.path("b"), b"0123456789").expect("ten bytes");
    let path = f.cstring(f.guest.data + 0x100, b"/b");
    let mode = f.cstring(f.guest.data + 0x140, b"rb");
    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY);
        asm.mov(2, 0);
    }) as i64;
    let stream = value_of(&f, "fdopen", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, mode as u64);
    });
    assert_ne!(stream, 0, "fdopen returned NULL");
    assert_eq!(value_of(&f, "fileno", |asm| { asm.mov(0, stream); }) as i64, fd);

    let buf = f.guest.data + 0x400;
    // Three items of three bytes: nine bytes, three items.
    assert_eq!(
        value_of(&f, "fread", |asm| {
            asm.mov(0, buf as u64);
            asm.mov(1, 3);
            asm.mov(2, 3);
            asm.mov(3, stream);
        }),
        3
    );
    assert_eq!(read_guest(&f, buf, 9), b"012345678");
    // One byte left: no complete item, and end of file.
    assert_eq!(
        value_of(&f, "fread", |asm| {
            asm.mov(0, buf as u64);
            asm.mov(1, 3);
            asm.mov(2, 3);
            asm.mov(3, stream);
        }),
        0
    );
    assert_eq!(value_of(&f, "feof", |asm| { asm.mov(0, stream); }) as i64, 1);
    assert_eq!(value_of(&f, "fclose", |asm| { asm.mov(0, stream); }) as i64, 0);

    // And `fwrite` on the way out, with `fputc` after it.
    let out = f.cstring(f.guest.data + 0x180, b"/out");
    let wmode = f.cstring(f.guest.data + 0x1c0, b"wb");
    let source = f.guest.data + 0x600;
    f.guest.write_bytes(source, &[1u8, 2, 3, 4, 5, 6]);
    let stream = value_of(&f, "fopen", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, wmode as u64);
    });
    assert_eq!(
        value_of(&f, "fwrite", |asm| {
            asm.mov(0, source as u64);
            asm.mov(1, 2);
            asm.mov(2, 3);
            asm.mov(3, stream);
        }),
        3,
        "three items of two bytes"
    );
    // `fputc` returns the byte as an unsigned char: 0xff is 255, never EOF.
    assert_eq!(
        value_of(&f, "fputc", |asm| {
            asm.mov(0, 0xffff_ffff_ffff_ffff);
            asm.mov(1, stream);
        }) as i64,
        255
    );
    assert_eq!(value_of(&f, "fclose", |asm| { asm.mov(0, stream); }) as i64, 0);
    assert_eq!(
        std::fs::read(scratch.path("out")).expect("the host file"),
        &[1u8, 2, 3, 4, 5, 6, 0xff]
    );
}

/// `pread` reads at an offset and leaves the descriptor's own offset alone.
///
/// **The regression this pins was found by the seam's own test and is worth having from the guest
/// side too**: `FileExt::seek_read` on Windows moves the file pointer, so a `pread` built on it
/// alone makes the next sequential `read` return end of file.
#[test]
fn pread_leaves_the_descriptors_own_offset_alone_for_the_guest_too() {
    let _guard = serialized();
    let (f, scratch) = rooted("pread");
    std::fs::write(scratch.path("p"), b"0123456789").expect("ten bytes");
    let path = f.cstring(f.guest.data + 0x100, b"/p");
    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY);
        asm.mov(2, 0);
    }) as i64;
    let buf = f.guest.data + 0x400;
    assert_eq!(
        value_of(&f, "read", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, buf as u64);
            asm.mov(2, 4);
        }) as i64,
        4
    );
    assert_eq!(read_guest(&f, buf, 4), b"0123");
    let entry = call_one(&f, "pread", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, (buf + 0x100) as u64);
        asm.mov(2, 3);
        asm.mov(3, 7);
    });
    let mut cpu = f.guest.thread(&f.boundary);
    match f.run(&mut cpu, entry) {
        Err(error) => {
            assert!(error.to_string().contains("pread(2)"), "{error}");
            return;
        }
        Ok(_) => assert_eq!(f.guest.read_u64(f.guest.data) as i64, 3),
    }
    assert_eq!(read_guest(&f, buf + 0x100, 3), b"789");
    // The sequential offset is untouched: the next read continues from 4.
    assert_eq!(
        value_of(&f, "read", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, (buf + 0x200) as u64);
            asm.mov(2, 3);
        }) as i64,
        3
    );
    assert_eq!(read_guest(&f, buf + 0x200, 3), b"456", "pread moved the descriptor's offset");
    // A negative offset is EINVAL rather than a wrap into a huge one.
    assert_eq!(
        value_of(&f, "pread", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, buf as u64);
            asm.mov(2, 3);
            asm.mov(3, 0xffff_ffff_ffff_ffff);
        }) as i64,
        -1
    );
    assert_eq!(value_of(&f, "close", |asm| { asm.mov(0, fd as u64); }) as i64, 0);
}

/// The namespace calls keep POSIX's own distinctions, from real guest code.
#[test]
fn the_namespace_calls_keep_posixs_distinctions_from_guest_code() {
    let _guard = serialized();
    let (f, scratch) = rooted("namespace");
    let dir = f.cstring(f.guest.data + 0x100, b"/d");
    let file = f.cstring(f.guest.data + 0x140, b"/d/f");
    let moved = f.cstring(f.guest.data + 0x180, b"/moved");

    assert_eq!(
        value_of(&f, "mkdir", |asm| {
            asm.mov(0, dir as u64);
            asm.mov(1, 0o755);
        }) as i64,
        0
    );
    assert!(scratch.path("d").is_dir(), "the host directory exists");
    assert_eq!(
        value_of(&f, "mkdir", |asm| {
            asm.mov(0, dir as u64);
            asm.mov(1, 0o755);
        }) as i64,
        -1,
        "a second mkdir is EEXIST"
    );
    std::fs::write(scratch.path("d/f"), b"body").expect("a file in it");
    // `unlink` refuses a directory and `rmdir` refuses a file, which `std::fs` does not do for us.
    assert_eq!(value_of(&f, "unlink", |asm| { asm.mov(0, dir as u64); }) as i64, -1);
    assert_eq!(value_of(&f, "rmdir", |asm| { asm.mov(0, file as u64); }) as i64, -1);
    assert_eq!(value_of(&f, "rmdir", |asm| { asm.mov(0, dir as u64); }) as i64, -1, "not empty");
    assert_eq!(
        value_of(&f, "rename", |asm| {
            asm.mov(0, file as u64);
            asm.mov(1, moved as u64);
        }) as i64,
        0
    );
    assert!(scratch.path("moved").is_file());
    assert_eq!(value_of(&f, "rmdir", |asm| { asm.mov(0, dir as u64); }) as i64, 0, "now empty");
    assert_eq!(value_of(&f, "unlink", |asm| { asm.mov(0, moved as u64); }) as i64, 0);
    assert_eq!(value_of(&f, "unlink", |asm| { asm.mov(0, moved as u64); }) as i64, -1);

    // `access`: F_OK, R_OK and W_OK are answered; X_OK is refused by name.
    std::fs::write(scratch.path("a"), b"x").expect("a file");
    let probe = f.cstring(f.guest.data + 0x1c0, b"/a");
    for mode in [0u64, 4, 2, 6] {
        assert_eq!(
            value_of(&f, "access", |asm| {
                asm.mov(0, probe as u64);
                asm.mov(1, mode);
            }) as i64,
            0,
            "access(path, {mode})"
        );
    }
    let error = refusal_of(&f, "access", |asm| {
        asm.mov(0, probe as u64);
        asm.mov(1, 1);
    });
    assert_eq!(error.symbol(), Some("access"), "{error:?}");
    assert!(error.to_string().contains("X_OK"), "{error}");
}

/// **`__open_2` refuses `O_CREAT` by name, and the refused `open` flags refuse by name.**
///
/// Each of these is a promise this layer cannot keep, and each has a believable wrong answer
/// available — accept the flag and do nothing — which is the shape `mlock` was refused for.
#[test]
fn the_open_flags_that_cannot_be_honoured_refuse_by_name() {
    let _guard = serialized();
    let (f, _scratch) = rooted("flags");
    let path = f.cstring(f.guest.data + 0x100, b"/x");
    // `__open_2` with O_CREAT: the FORTIFY check bionic aborts on.
    let error = refusal_of(&f, "__open_2", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_WRONLY | O_CREAT);
    });
    assert_eq!(error.symbol(), Some("__open_2"), "{error:?}");
    assert!(error.to_string().contains("O_CREAT"), "{error}");
    // And the flags whose guarantees this layer cannot meet.
    for (flag, name) in [
        (0o4010000u64, "O_SYNC"),
        (0o10000u64, "O_DSYNC"),
        (0o40000u64, "O_DIRECT"),
        (0o10000000u64, "O_PATH"),
        (0o20200000u64, "O_TMPFILE"),
    ] {
        let error = refusal_of(&f, "open", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, O_RDWR | flag);
            asm.mov(2, 0);
        });
        assert!(error.to_string().contains(name), "{name}: {error}");
    }
    // A bit nobody defined is refused with the bits named, never masked away.
    let error = refusal_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY | (1 << 30));
        asm.mov(2, 0);
    });
    assert!(error.to_string().contains("no name for"), "{error}");
    // `O_DIRECTORY` on a file is ENOTDIR, and on a directory it gives a descriptor that stats.
    assert_eq!(
        value_of(&f, "__open_2", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, O_RDONLY | O_DIRECTORY);
        }) as i64,
        -1
    );
}

/// **A wild `FILE *` or `DIR *` is refused by name, never answered.**
///
/// `NULL` is `readdir`'s own end-of-directory answer and `EBADF` is `fileno`'s own invalid-stream
/// answer, so either would let guest code route around a wild pointer. Every `FILE *` and `DIR *`
/// in this guest's world came out of this layer, so one that did not is a real defect.
#[test]
fn a_wild_file_or_directory_pointer_is_refused_by_name() {
    let _guard = serialized();
    let (f, _scratch) = rooted("wild");
    for symbol in ["feof", "fileno", "fclose", "fflush"] {
        let error = refusal_of(&f, symbol, |asm| { asm.mov(0, 0xdead_beef_0000_1000); });
        assert_eq!(error.symbol(), Some(symbol), "{error:?}");
        assert!(error.to_string().contains("dead"), "{error}");
    }
    let error = refusal_of(&f, "readdir", |asm| { asm.mov(0, 0xdead_beef_0000_1000); });
    assert_eq!(error.symbol(), Some("readdir"), "{error:?}");
    // `closedir` on an unknown pointer is EBADF rather than a refusal: it is the one call whose
    // whole job is to release a handle, and `EBADF` is what a double `closedir` gets on a device.
    assert_eq!(value_of(&f, "closedir", |asm| { asm.mov(0, 0xdead_beef_0000_1000); }) as i64, -1);
}

/// **Hostile arguments to the file group: every one is a defined answer or a typed refusal.**
///
/// Null pointers, unmapped pointers, read-only destinations, descriptors nobody opened, counts
/// past `SSIZE_MAX`, and a FORTIFY size smaller than the count. A panic or an abort reachable
/// from any of these is Critical (Global Constraint 11), so the test asserts the *shape* of every
/// outcome rather than a value.
#[test]
fn hostile_arguments_to_the_file_group_are_typed_errors_and_not_panics() {
    let _guard = serialized();
    let (f, scratch) = rooted("hostile");
    std::fs::write(scratch.path("h"), b"0123456789").expect("a file");
    let path = f.cstring(f.guest.data + 0x100, b"/h");
    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY);
        asm.mov(2, 0);
    });
    let unmapped = f.guest.unmapped as u64;
    let readonly = f.guest.readonly as u64;

    let cases: &[(&str, &[u64])] = &[
        // Null and wild paths.
        ("open", &[0, O_RDONLY, 0]),
        ("open", &[unmapped, O_RDONLY, 0]),
        ("stat", &[0, f.guest.data as u64]),
        ("lstat", &[unmapped, f.guest.data as u64]),
        ("statvfs", &[0, f.guest.data as u64]),
        ("access", &[0, 0]),
        ("unlink", &[0]),
        ("mkdir", &[unmapped, 0o755]),
        ("rmdir", &[0]),
        ("opendir", &[0]),
        ("rename", &[0, 0]),
        // Destinations that cannot be written.
        ("stat", &[path as u64, unmapped]),
        ("stat", &[path as u64, readonly]),
        ("fstat", &[fd, readonly]),
        ("statvfs", &[path as u64, unmapped]),
        ("read", &[fd, unmapped, 16]),
        ("read", &[fd, readonly, 16]),
        // Descriptors nobody opened, including the negative ones a failed call returns.
        ("read", &[0xffff_ffff_ffff_ffff, f.guest.data as u64, 16]),
        ("close", &[0xffff_ffff_ffff_ffff]),
        ("fstat", &[999, f.guest.data as u64]),
        ("pread", &[999, f.guest.data as u64, 16, 0]),
        ("__write_chk", &[999, f.guest.data as u64, 4, 4]),
        // Counts and offsets the guest chose.
        ("read", &[fd, f.guest.data as u64, 0xffff_ffff_ffff_ffff]),
        ("pread", &[fd, f.guest.data as u64, 0xffff_ffff_ffff_ffff, 0]),
        ("pread", &[fd, f.guest.data as u64, 16, 0x8000_0000_0000_0000]),
        ("__write_chk", &[fd, f.guest.data as u64, 0xffff_ffff_ffff_ffff, 0xffff_ffff_ffff_ffff]),
        // A FORTIFY size smaller than the count: a detected overrun in guest code.
        ("__write_chk", &[fd, f.guest.data as u64, 64, 8]),
        // Zero-length transfers at a null pointer, which are legal C and must not fault.
        ("read", &[fd, 0, 0]),
        ("__write_chk", &[fd, 0, 0, 0]),
        // Streams.
        ("fopen", &[0, 0]),
        ("fopen", &[path as u64, 0]),
        ("fopen", &[path as u64, unmapped]),
        ("fdopen", &[999, 0]),
        ("fgets", &[0, 64, 0]),
        ("fread", &[unmapped, 1, 16, 0]),
    ];

    for (symbol, args) in cases {
        let entry = call_one(&f, symbol, |asm| {
            for (index, value) in args.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
        });
        let mut cpu = f.guest.thread(&f.boundary);
        match f.run(&mut cpu, entry) {
            Ok(exit) => {
                assert!(
                    matches!(exit, ExitReason::Returned { .. }),
                    "`{symbol}` {args:x?}: {exit:?}"
                );
                let value = f.guest.read_u64(f.guest.data) as i64;
                assert!(
                    value == 0 || value == -1 || value >= 0,
                    "`{symbol}` {args:x?} completed with {value}"
                );
            }
            Err(error) => {
                assert_eq!(error.symbol(), Some(*symbol), "{error:?}");
                assert!(error.guest_address().is_some(), "{error}");
            }
        }
    }
}

/// The descriptor and stream ceilings are `EMFILE`, and they are real bounds.
///
/// A guest that leaks descriptors in a loop would otherwise hold as many host handles as it
/// liked, and several instances share one host process.
#[test]
fn the_descriptor_and_stream_ceilings_report_emfile_rather_than_growing() {
    let _guard = serialized();
    let (f, scratch) = rooted("ceilings");
    std::fs::write(scratch.path("c"), b"x").expect("a file");
    let path = f.cstring(f.guest.data + 0x100, b"/c");
    let mode = f.cstring(f.guest.data + 0x140, b"r");

    let mut opened = 0usize;
    loop {
        let fd = value_of(&f, "open", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, O_RDONLY);
            asm.mov(2, 0);
        }) as i64;
        if fd < 0 {
            break;
        }
        opened += 1;
        assert!(opened <= omni_platform::fs::MAX_OPEN_FILES, "the descriptor ceiling never fired");
    }
    assert_eq!(
        opened,
        omni_platform::fs::MAX_OPEN_FILES - 3,
        "three of the descriptors are the standard streams"
    );
    // And a `fopen` now fails too, because it needs a descriptor first.
    assert_eq!(
        value_of(&f, "fopen", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, mode as u64);
        }),
        0
    );
}

// =================================================================== thread lifecycle (phase 3c)

/// A fixture whose instance can create guest threads, over the guest's own backend.
fn fixture_with_threads(limit: usize) -> Fixture {
    let f = fixture_with(&[]);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&f.guest.backend) as _;
    f.bionic
        .set_thread_host(ThreadHost::new(backend).with_limit(limit))
        .expect("a thread host");
    f
}

/// Write a `u64` into guest memory from a host thread that does not hold the `Guest`.
///
/// `Guest` is deliberately not `Sync` — it owns a `Cell` for the next code offset — so a helper
/// thread cannot borrow it. Identity mapping (D4) makes a guest address a host address, and
/// `GuestSpace` is `Send + Sync`, so this is the whole of what such a thread needs.
fn poke(space: &Arc<omni_mem::GuestSpace>, at: omni_cpu::GuestAddr, value: u64) {
    let ptr = space.ptr(at, 8).expect("a host pointer for a guest word");
    // SAFETY: `GuestSpace::ptr` has checked that the eight bytes at `at` are mapped and
    // committed in this space, and D4 makes the guest address a host address. Guest threads may
    // be reading this word concurrently, which is the point of it — it is a gate they poll —
    // and an aligned 8-byte store is what they poll for.
    unsafe { ptr.cast::<u64>().write_unaligned(value) }
}

/// Opens a set of guest gate words when dropped, **including while a panic unwinds**.
///
/// Every test here that starts a guest thread which spins on a gate needs one. A failing
/// assertion otherwise leaves that thread spinning for the rest of the test binary: it does not
/// hang — libtest exits the process when the run finishes — but it burns a core and makes every
/// later test slower and noisier, and under a mutation run that is dozens of rows. Found when
/// mutation row `threads-B1` made `pthread_join` block in a test whose program opened its gate
/// *after* the join, which deadlocked the harness rather than failing it.
struct OpenOnDrop {
    space: Arc<omni_mem::GuestSpace>,
    gates: Vec<omni_cpu::GuestAddr>,
}

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        for gate in &self.gates {
            poke(&self.space, *gate, 1);
        }
    }
}

/// Assemble a start routine: `X0` is the guest's `arg`, and whatever it leaves in `X0` is the
/// thread's `void *`. It finishes with `RET`, which lands on the boundary's sentinel.
fn start_routine(f: &Fixture, build: impl FnOnce(&mut Asm)) -> omni_cpu::GuestAddr {
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    build(&mut asm);
    asm.push(ret(30));
    f.guest.load(asm.words());
    entry
}

/// `pthread_create(&tid, attr, start, arg)` with `tid` at `out` and the result at `out + 8`.
fn create_call(
    f: &Fixture,
    asm: &mut Asm,
    out: omni_cpu::GuestAddr,
    attr: u64,
    start: omni_cpu::GuestAddr,
    arg: u64,
) {
    asm.mov(0, out as u64);
    asm.mov(1, attr);
    asm.mov(2, start as u64);
    asm.mov(3, arg);
    asm.bl(f.thunk("pthread_create"));
    asm.mov(22, out as u64);
    asm.push(str_imm(0, 22, 8));
}

/// **An instance with no thread host refuses `pthread_create` by name.**
///
/// No default is possible: only a CPU backend can give a new guest thread the bionic TLS block
/// D13 requires before it runs an instruction. `EAGAIN` would say the runtime ran out of
/// resources when it was never configured, and a guest would retry for ever.
#[test]
fn pthread_create_without_a_thread_host_refuses_by_name() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let error = refusal_of(&f, "pthread_create", |asm| {
        asm.mov(0, f.guest.data as u64 + 0x800);
        asm.mov(1, 0);
        asm.mov(2, f.guest.code as u64);
        asm.mov(3, 0);
    });
    assert_eq!(error.symbol(), Some("pthread_create"));
    assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("set_thread_host"), "the refusal must name the method: {text}");
    assert!(text.contains("D13"), "and say what it cannot satisfy without one: {text}");
}

/// **The phase, end to end: a real guest thread runs real translated ARM64 code, and the join
/// brings its `void *` back.**
///
/// Asserted three ways, because each one alone would pass against a different broken version:
/// the thread's own *side effect* in guest memory (a handler that returned 0 without starting
/// anything would leave it untouched), the value `pthread_join` writes (a join that succeeded
/// without waiting would read it before the thread wrote it), and the return codes of both calls.
#[test]
fn a_guest_thread_runs_guest_code_and_join_brings_back_its_value() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let marker = f.guest.data + 0x880;
    f.guest.write_u64(marker, 0);

    // The start routine: store `arg` at `marker`, and return `arg + 1`.
    let start = start_routine(&f, |asm| {
        asm.mov(9, marker as u64);
        asm.push(str_imm(0, 9, 0));
        asm.push(add_imm(0, 0, 1));
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0x4321);
        // pthread_join(tid, &retval)
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, out as u64 + 16);
        asm.bl(f.thunk("pthread_join"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 24));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));

    assert_eq!(f.guest.read_u64(out + 8), 0, "pthread_create returns 0 on success");
    assert_eq!(f.guest.read_u64(out + 24), 0, "pthread_join returns 0 on success");
    assert_eq!(f.guest.read_u64(marker), 0x4321, "the start routine really ran, with its arg");
    assert_eq!(f.guest.read_u64(out + 16), 0x4322, "and its `void *` came back through the join");
    assert_ne!(f.guest.read_u64(out), 0, "the pthread_t is never the reserved zero");
    assert_eq!(f.bionic.live_guest_threads(), 0, "the thread was reaped by the join");
}

/// **D13, on a thread the guest created: `TPIDR_EL0` is programmed before the thread's first
/// instruction, and its stack guard is the same value every other thread has.**
///
/// This is the constraint that is easiest to satisfy incorrectly and hardest to notice: a guest
/// thread with a zero thread pointer faults on the first stack-protected call, with a symptom
/// that reads as a loader bug, and one with a *different* guard than its parent fails
/// `__stack_chk_fail` only when a canary crosses threads.
///
/// The start routine reads the thread pointer itself, exactly as 1,276 of `libroblox.so`'s own
/// instructions do, and stores both the pointer and `[Xt, #0x28]`.
#[test]
fn a_created_guest_thread_has_its_thread_pointer_and_the_process_stack_guard() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let seen = f.guest.data + 0x900;
    f.guest.write_u64(seen, 0);
    f.guest.write_u64(seen + 8, 0);

    let start = start_routine(&f, |asm| {
        asm.mov(9, seen as u64);
        asm.push(mrs_tpidr_el0(10));
        asm.push(str_imm(10, 9, 0));
        asm.push(ldr_imm(11, 10, 0x28));
        asm.push(str_imm(11, 9, 8));
        asm.mov(0, 0);
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 24));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 24), 0, "the join succeeded");

    let pointer = f.guest.read_u64(seen);
    let guard = f.guest.read_u64(seen + 8);
    assert_ne!(pointer, 0, "a guest thread with a null TPIDR_EL0 is D13's crash");
    assert!(
        f.guest.space.contains(pointer as usize, 0x30),
        "the thread pointer must be guest memory with room for the slots up to +0x28"
    );
    assert_ne!(guard, 0, "a zero canary compares equal to a zeroed stack slot");
    assert_eq!(
        guard,
        f.guest.backend.tls().stack_guard(),
        "bionic copies ONE per-process guard into every thread; a created thread with its own \
         value fails __stack_chk_fail only when a canary crosses threads"
    );
}

/// The `pthread_t` the parent is given is the one the child's `pthread_self()` returns, and the
/// two threads have different `errno` cells.
///
/// Allocating the identity in the child would make these differ until the child got there, which
/// is a race guest code loses rarely and silently.
#[test]
fn a_created_thread_agrees_with_its_parent_about_its_own_identity() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let seen = f.guest.data + 0x900;
    f.guest.write_u64(seen, 0);
    f.guest.write_u64(seen + 8, 0);

    let start = start_routine(&f, |asm| {
        asm.push(mov_reg(19, 30));
        asm.mov(20, seen as u64);
        asm.bl(f.thunk("pthread_self"));
        asm.push(str_imm(0, 20, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(str_imm(0, 20, 8));
        asm.mov(0, 0);
        asm.push(mov_reg(30, 19));
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 24));
        // And this thread's own errno cell, for the comparison.
        asm.bl(f.thunk("__errno"));
        asm.push(str_imm(0, 22, 32));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));

    assert_eq!(f.guest.read_u64(out + 24), 0, "the join succeeded");
    assert_eq!(
        f.guest.read_u64(seen),
        f.guest.read_u64(out),
        "the child's pthread_self() must be the pthread_t the parent was handed"
    );
    assert_ne!(
        f.guest.read_u64(seen + 8),
        f.guest.read_u64(out + 32),
        "two guest threads sharing one errno cell is the failure the arena exists to prevent"
    );
}

/// **A start routine at an address that is not guest code is a recorded failure, and the join
/// refuses rather than reporting a `void *` the thread never produced.**
///
/// The hostile case the brief names. A `0` from the join with a null `retval` would be
/// indistinguishable from a thread that returned `NULL`.
#[test]
fn a_start_routine_at_a_bad_address_is_a_recorded_failure_and_a_join_refusal() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, f.guest.unmapped, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, out as u64 + 16);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 24));
    });
    let error = match run_program(&f, entry) {
        Err(error) => error,
        Ok(exit) => panic!("the join must refuse, got {exit:?}"),
    };
    assert_eq!(error.symbol(), Some("pthread_join"));
    assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("stopped without returning"), "{text}");

    // `pthread_create` itself succeeded: the thread was created, and it is the thread that
    // failed. That distinction is the whole reason the failure is recorded separately.
    assert_eq!(f.guest.read_u64(out + 8), 0, "pthread_create reported success");
    let failures = f.bionic.guest_thread_failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].start_routine, f.guest.unmapped);
    assert_eq!(failures[0].thread, f.guest.read_u64(out));
    // **Where it died, not only where it started.** `PC` first -- the fetch from the unmapped
    // start routine is the fault -- then `X30`, which the runner set to the sentinel and nothing
    // has overwritten. A record that dropped the stack, or took it after the thread's stack was
    // unmapped, fails here rather than in a three-minute run that needed it.
    assert_eq!(
        failures[0].stack.first().copied(),
        Some(f.guest.unmapped as u64),
        "the stack at death starts at the PC that faulted: {:?}",
        failures[0].stack
    );
    assert_eq!(
        failures[0].stack.get(1).copied(),
        Some(f.boundary.sentinel() as u64),
        "then X30, which the runner pointed at the sentinel: {:?}",
        failures[0].stack
    );
}

/// A detached thread that fails has nobody to report to, so the instance's failure list is the
/// only place it can surface — and it is, with the thread and its start routine named.
#[test]
fn a_detached_guest_thread_that_fails_is_still_recorded() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let attr = f.guest.data + 0xA00;
    let entry = program(&f, |asm| {
        // pthread_attr_init, then write PTHREAD_CREATE_DETACHED into it directly -- the guest's
        // own attr bytes, which is all a detach state is.
        asm.mov(0, attr as u64);
        asm.bl(f.thunk("pthread_attr_init"));
        asm.mov(9, attr as u64);
        asm.mov(10, 1);
        asm.push(str_w(10, 9, 0));
        create_call(&f, asm, out, attr as u64, f.guest.unmapped, 0);
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0, "pthread_create reported success");

    // The thread is detached, so nothing joins it: wait for the failure to be recorded.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while f.bionic.guest_thread_failures().is_empty() {
        assert!(std::time::Instant::now() < deadline, "a detached thread's failure was never recorded");
        std::thread::yield_now();
    }
    let failures = f.bionic.guest_thread_failures();
    assert_eq!(failures[0].start_routine, f.guest.unmapped);
    assert!(
        failures[0].to_string().contains("start routine"),
        "the record must name the thread and its start routine: {}",
        failures[0]
    );
    // And the record is gone: nobody will ever join a detached thread, so keeping one would be a
    // leak a guest could drive in a loop.
    while f.bionic.guest_thread_records() > 0 {
        assert!(std::time::Instant::now() < deadline, "a detached thread's record was never removed");
        std::thread::yield_now();
    }
}

/// The POSIX error cases, each of which is a branch guest code has.
///
/// Asserted **by number**: `EDEADLK` is 35, `ESRCH` is 3 and `EINVAL` is 22 in Linux's numbering,
/// which is not the host's, and a handler that returned the host's would be wrong in a way that
/// only a guest can see.
#[test]
fn the_join_and_detach_error_cases_are_the_posix_ones() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;

    // Joining yourself.
    let entry = program(&f, |asm| {
        asm.bl(f.thunk("pthread_self"));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        // An id nothing ever handed out.
        asm.mov(0, 0xDEAD_BEEF);
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 8));
        // Detaching one.
        asm.mov(0, 0xDEAD_BEEF);
        asm.bl(f.thunk("pthread_detach"));
        asm.push(str_imm(0, 22, 16));
        // And `pthread_getschedparam` on it.
        asm.mov(0, 0xDEAD_BEEF);
        asm.mov(1, out as u64 + 64);
        asm.mov(2, out as u64 + 72);
        asm.bl(f.thunk("pthread_getschedparam"));
        asm.push(str_imm(0, 22, 24));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out), 35, "joining yourself is EDEADLK");
    assert_eq!(f.guest.read_u64(out + 8), 3, "joining an unknown thread is ESRCH");
    assert_eq!(f.guest.read_u64(out + 16), 3, "detaching an unknown thread is ESRCH");
    assert_eq!(f.guest.read_u64(out + 24), 3, "getschedparam on an unknown thread is ESRCH");
}

/// A second `pthread_detach` is `EINVAL`, and a `pthread_join` on a detached thread is too.
///
/// **A second detach answering `0` would be indistinguishable from the first**, and a double
/// detach is a guest defect worth hearing about: in a real implementation it is a use-after-free
/// of the thread's own descriptor. The thread here spins on a guest word so that it is still
/// running while both calls are made — a thread that had already finished would exercise the
/// reap path instead, which is a different branch.
#[test]
fn detaching_twice_is_einval_and_joining_a_detached_thread_is_einval() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let gate = f.guest.data + 0x980;
    f.guest.write_u64(gate, 0);

    // Spin until the parent stores a non-zero word, then return.
    let start = start_routine(&f, |asm| {
        asm.mov(9, gate as u64);
        let loop_at = asm.pc();
        asm.push(ldr_imm(10, 9, 0));
        asm.push(subs_imm(10, 10, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.mov(0, 0);
    });

    let ready = f.guest.data + 0x988;
    f.guest.write_u64(ready, 0);
    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(19, 22, 0));
        asm.push(mov_reg(0, 19));
        asm.bl(f.thunk("pthread_detach"));
        asm.push(str_imm(0, 22, 16));
        asm.push(mov_reg(0, 19));
        asm.bl(f.thunk("pthread_detach"));
        asm.push(str_imm(0, 22, 24));
        // Both detaches are done: tell the host, which releases the thread. See below.
        asm.mov(9, ready as u64);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
        asm.push(mov_reg(0, 19));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 32));
    });

    // **The gate is opened from the host, once both detaches have happened, and not by the guest
    // program after its own `pthread_join`.** The join here is *expected* to return EINVAL
    // immediately, because the thread is detached — but a defect that made it block instead
    // would deadlock against a gate the same program had not reached yet. That is not a
    // hypothetical: mutation row `threads-B1` refuses the *first* detach, which leaves the
    // thread joinable, and the first version of this test hung the whole mutation harness on it
    // rather than failing. A test that deadlocks under the defect it exists to detect reports
    // nothing at all.
    let _gates = OpenOnDrop { space: Arc::clone(&f.guest.space), gates: vec![gate] };
    let opener = {
        let space = Arc::clone(&f.guest.space);
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while space.ptr(ready, 8).map_or(0, |p| unsafe { p.cast::<u64>().read_unaligned() })
                == 0
            {
                if std::time::Instant::now() > deadline {
                    break;
                }
                std::thread::yield_now();
            }
            poke(&space, gate, 1);
        })
    };
    let outcome = run_program(&f, entry);
    opener.join().expect("the gate opener");
    assert!(matches!(outcome.expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 16), 0, "the first detach succeeds");
    assert_eq!(f.guest.read_u64(out + 24), 22, "the second is EINVAL, not a second success");
    assert_eq!(f.guest.read_u64(out + 32), 22, "a detached thread is not joinable");

    // And the detached thread really does finish and clean itself up.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while f.bionic.guest_thread_records() > 0 {
        assert!(std::time::Instant::now() < deadline, "the detached thread never finished");
        std::thread::yield_now();
    }
    assert!(f.bionic.guest_thread_failures().is_empty(), "it returned, so it did not fail");
}

/// **Two guest threads joining each other is `EDEADLK`, not two host threads blocked for ever.**
///
/// A hang from untrusted guest input is a denial of service on the host, and POSIX permits
/// `EDEADLK` for exactly this ("a deadlock was detected"). The check walks the wait chain, so it
/// catches a cycle of any length rather than only the self-join POSIX names.
#[test]
fn two_guest_threads_joining_each_other_is_edeadlk() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let ids = f.guest.data + 0xB00;
    let results = f.guest.data + 0xB40;
    for offset in 0..8u32 {
        f.guest.write_u64(ids + offset as usize * 8, 0);
        f.guest.write_u64(results + offset as usize * 8, 0);
    }

    // The ids double as the gates these two spin on, so a failing assertion releases them.
    let _gates = OpenOnDrop {
        space: Arc::clone(&f.guest.space),
        gates: vec![ids, ids + 8],
    };
    // Each thread joins whichever id it is handed, spinning until it is non-zero first so that
    // both ids are published before either join is attempted.
    let start = start_routine(&f, |asm| {
        asm.push(mov_reg(19, 30));
        asm.push(mov_reg(20, 0)); // the address of the id to join
        let loop_at = asm.pc();
        asm.push(ldr_imm(9, 20, 0));
        asm.push(subs_imm(9, 9, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.push(mov_reg(0, 9));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 20, 16));
        asm.mov(0, 0);
        asm.push(mov_reg(30, 19));
    });

    let entry = program(&f, |asm| {
        // Thread A is told to join what lands at `ids + 8`; thread B, `ids + 0`.
        create_call(&f, asm, out, 0, start, ids as u64 + 8);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(19, 22, 0));
        create_call(&f, asm, out + 32, 0, start, ids as u64);
        asm.mov(22, out as u64 + 32);
        asm.push(ldr_imm(20, 22, 0));
        // Publish both ids at once, so neither thread can join before the other exists.
        asm.mov(9, ids as u64);
        asm.push(str_imm(19, 9, 0));
        asm.push(str_imm(20, 9, 8));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));

    // **The parent does not join them**, and that is not a convenience: a `pthread_join` from
    // this thread would itself be a waiter on one of the two, and the other's join would then be
    // refused as "already being joined" before it ever reached the cycle check. The first draft
    // did exactly that and measured EINVAL instead of EDEADLK.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while f.bionic.live_guest_threads() > 0 {
        assert!(std::time::Instant::now() < deadline, "the mutual join deadlocked");
        std::thread::yield_now();
    }

    // One of the two saw the cycle. Which one is a race and is not asserted; that exactly one
    // did is not.
    let a = f.guest.read_u64(ids + 16);
    let b = f.guest.read_u64(ids + 24);
    assert!(
        a == 35 || b == 35,
        "one of the two mutual joins must be EDEADLK, got {a} and {b}"
    );
    assert!(f.bionic.guest_thread_failures().is_empty(), "neither thread failed: {:?}", f.bionic.guest_thread_failures());
}

/// A hostile `pthread_attr_t`: a detach state the guest wrote directly, a stack under the floor,
/// and a stack size no mapping of which can exist.
///
/// **`SIZE_MAX` is the one that matters.** Rounding it up to a page with `(n + page - 1) &
/// !(page - 1)` gives **zero**, silently in a release build, so a guest asking for the largest
/// stack in the world would be handed a zero-byte one. That is `gmtime(i64::MIN)` again.
#[test]
fn a_hostile_pthread_attr_is_einval_or_eagain_and_never_a_zero_stack() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let attr = f.guest.data + 0xA00;
    let start = start_routine(&f, |asm| {
        asm.mov(0, 7);
    });

    let entry = program(&f, |asm| {
        // A detach state that is neither JOINABLE nor DETACHED.
        asm.mov(0, attr as u64);
        asm.bl(f.thunk("pthread_attr_init"));
        asm.mov(9, attr as u64);
        asm.mov(10, 99);
        asm.push(str_w(10, 9, 0));
        create_call(&f, asm, out, attr as u64, start, 0);

        // A stack under the floor.
        asm.mov(0, attr as u64);
        asm.bl(f.thunk("pthread_attr_init"));
        asm.mov(0, attr as u64);
        asm.mov(1, 64);
        asm.bl(f.thunk("pthread_attr_setstacksize"));
        create_call(&f, asm, out + 32, attr as u64, start, 0);

        // A stack of SIZE_MAX.
        asm.mov(0, attr as u64);
        asm.bl(f.thunk("pthread_attr_init"));
        asm.mov(0, attr as u64);
        asm.mov(1, u64::MAX);
        asm.bl(f.thunk("pthread_attr_setstacksize"));
        create_call(&f, asm, out + 64, attr as u64, start, 0);
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));

    assert_eq!(f.guest.read_u64(out + 8), 22, "a detach state of 99 is EINVAL");
    assert_eq!(f.guest.read_u64(out + 40), 22, "a 64-byte stack is EINVAL");
    assert_eq!(f.guest.read_u64(out + 72), 11, "a SIZE_MAX stack is EAGAIN, never a zero stack");
    assert_eq!(f.bionic.guest_thread_records(), 0, "none of the three created a thread");
    assert_eq!(f.bionic.live_guest_threads(), 0);
}

/// **Thread-count exhaustion is `EAGAIN`, which is POSIX's own answer and a branch guest code
/// has** — not a refusal, and not a thread that shares another one's `errno` block.
#[test]
fn creating_more_guest_threads_than_the_limit_is_eagain() {
    let _guard = serialized();
    let f = fixture_with_threads(2);
    let out = f.guest.data + 0x800;
    let gate = f.guest.data + 0x980;
    f.guest.write_u64(gate, 0);
    let _gates = OpenOnDrop { space: Arc::clone(&f.guest.space), gates: vec![gate] };
    let start = start_routine(&f, |asm| {
        asm.mov(9, gate as u64);
        let loop_at = asm.pc();
        asm.push(ldr_imm(10, 9, 0));
        asm.push(subs_imm(10, 10, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.mov(0, 0);
    });

    let entry = program(&f, |asm| {
        for slot in 0..3u32 {
            create_call(&f, asm, out + slot as usize * 32, 0, start, 0);
        }
        // Let them all go, then join the two that started.
        asm.mov(9, gate as u64);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
        for slot in 0..2u32 {
            asm.mov(22, out as u64 + u64::from(slot) * 32);
            asm.push(ldr_imm(0, 22, 0));
            asm.mov(1, 0);
            asm.bl(f.thunk("pthread_join"));
        }
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0, "the first is created");
    assert_eq!(f.guest.read_u64(out + 40), 0, "and the second");
    assert_eq!(f.guest.read_u64(out + 72), 11, "the third is EAGAIN, the limit being 2");
    assert_eq!(f.bionic.live_guest_threads(), 0, "both were reaped");
    assert!(f.bionic.guest_thread_failures().is_empty());
}

/// **A guest thread's arena block goes back when it exits**, so a guest that creates and joins in
/// a loop is not limited to 64 threads for the life of the instance.
///
/// The block index comes from a free list rather than from the table's length, and the length
/// version is what makes two live threads share one block — which is silent. This test creates
/// and joins more threads than the live limit, which only works if blocks are reused.
#[test]
fn an_exited_guest_thread_gives_its_arena_block_back() {
    let _guard = serialized();
    let f = fixture_with_threads(2);
    let out = f.guest.data + 0x800;
    let counter = f.guest.data + 0x9C0;
    f.guest.write_u64(counter, 0);
    let start = start_routine(&f, |asm| {
        asm.mov(9, counter as u64);
        asm.push(ldr_imm(10, 9, 0));
        asm.push(add_imm(10, 10, 1));
        asm.push(str_imm(10, 9, 0));
        asm.mov(0, 0);
    });

    for _ in 0..6 {
        let entry = program(&f, |asm| {
            create_call(&f, asm, out, 0, start, 0);
            asm.mov(22, out as u64);
            asm.push(ldr_imm(0, 22, 0));
            asm.mov(1, 0);
            asm.bl(f.thunk("pthread_join"));
            asm.push(str_imm(0, 22, 24));
        });
        assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
        assert_eq!(f.guest.read_u64(out + 8), 0, "every create succeeds");
        assert_eq!(f.guest.read_u64(out + 24), 0, "every join succeeds");
    }
    assert_eq!(f.guest.read_u64(counter), 6, "six threads really ran");
    // **One**: this test's own host thread, which attached when the first program ran. Six
    // guest threads came and went; if a block were never given back the count would be seven,
    // and the seventh `pthread_create` would eventually be refused rather than reusing one.
    assert_eq!(
        f.bionic.attached(),
        1,
        "every exited guest thread must have given its arena block back"
    );
}

/// `pthread_getschedparam` answers `SCHED_OTHER` with a priority of 0 for a thread this instance
/// created, and it writes both out-parameters.
///
/// The argument that this is an answer rather than a stub is in the handler's documentation and
/// rests on there being no way to *set* a policy anywhere in the reachable 188 — which
/// `the_bound_count_is_exactly_what_this_phase_claims` and the reachable list together pin.
#[test]
fn getschedparam_answers_sched_other_with_priority_zero() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    f.guest.write_u64(out + 64, 0x5555_5555_5555_5555);
    f.guest.write_u64(out + 72, 0x6666_6666_6666_6666);
    f.guest.write_u64(out + 80, 0x7777_7777_7777_7777);
    f.guest.write_u64(out + 88, 0x8888_8888_8888_8888);
    // **No gate, and the thread returns immediately.** Whether it is still running when the
    // question is asked does not matter: a joinable thread's record outlives it until the join,
    // so `pthread_getschedparam` answers either way — and a thread that spins would have to be
    // released, which is one more way for a failing assertion to leave one behind.
    let start = start_routine(&f, |asm| {
        asm.mov(0, 0);
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(19, 22, 0));
        asm.push(mov_reg(0, 19));
        asm.mov(1, out as u64 + 64);
        asm.mov(2, out as u64 + 72);
        asm.bl(f.thunk("pthread_getschedparam"));
        asm.push(str_imm(0, 22, 24));
        // And about **this** thread, which `pthread_create` did not make. The first version
        // answered ESRCH for it, because the registry only held created threads — the wrong
        // answer, since the main thread is a thread of this process with the same default
        // policy, and a guest asking about itself during initialisation would have taken an
        // error branch for no reason.
        asm.bl(f.thunk("pthread_self"));
        asm.mov(1, out as u64 + 80);
        asm.mov(2, out as u64 + 88);
        asm.bl(f.thunk("pthread_getschedparam"));
        asm.push(str_imm(0, 22, 40));
        asm.push(mov_reg(0, 19));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 24), 0, "it answers for a thread it created");
    assert_eq!(f.guest.read_u64(out + 40), 0, "and for the thread asking, which it did not create");
    assert_eq!(f.guest.read_u64(out + 80) & 0xFFFF_FFFF, 0, "SCHED_OTHER for this thread too");
    assert_eq!(
        f.guest.read_u64(out + 64) & 0xFFFF_FFFF,
        0,
        "SCHED_OTHER is SCHED_NORMAL and is 0"
    );
    assert_eq!(
        f.guest.read_u64(out + 72) & 0xFFFF_FFFF,
        0,
        "sched_priority has exactly one legal value under SCHED_OTHER"
    );
}

/// The stop switch really stops a guest thread that would otherwise run for ever, at a run-window
/// boundary — and a `pthread_join` on it is a refusal rather than a `void *` it never produced.
///
/// **D16's shape**: the bound on a runaway guest is a short budget window plus a decision point,
/// not a cross-thread halt. The window is lowered here so the test is a second rather than a
/// minute; that the mechanism is the window and not the halt is what the low value demonstrates.
#[test]
fn a_runaway_guest_thread_stops_at_a_window_boundary() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&f.guest.backend) as _;
    f.bionic
        .set_thread_host(ThreadHost::new(backend).with_limit(2).with_step_window(1_000))
        .expect("a thread host");
    let out = f.guest.data + 0x800;
    let running = f.guest.data + 0x9E0;
    f.guest.write_u64(running, 0);

    // An unconditional loop that first says it is running.
    let start = start_routine(&f, |asm| {
        asm.mov(9, running as u64);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
        asm.push(b(0));
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0, "the thread was created");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while f.guest.read_u64(running) == 0 {
        assert!(std::time::Instant::now() < deadline, "the runaway thread never started");
        std::thread::yield_now();
    }
    f.bionic.stop_guest_threads();
    while f.bionic.live_guest_threads() > 0 {
        assert!(std::time::Instant::now() < deadline, "the stop switch did not stop it");
        std::thread::yield_now();
    }
    assert_eq!(
        f.bionic.guest_thread_state(f.guest.read_u64(out)),
        Some(omni_android::bionic::GuestThreadState::Stopped)
    );

    // And the join refuses, naming what happened, rather than reporting a value.
    let join = program(&f, |asm| {
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
    });
    let error = match run_program(&f, join) {
        Err(error) => error,
        Ok(exit) => panic!("the join must refuse, got {exit:?}"),
    };
    assert!(error.to_string().contains("asked to stop"), "{error}");
}

/// A second thread host is refused: a second CPU backend would allocate TLS blocks from a second
/// arena, and bionic copies **one** stack-guard value into every thread of a process.
#[test]
fn a_thread_host_may_be_set_only_once() {
    let _guard = serialized();
    let f = fixture_with_threads(2);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&f.guest.backend) as _;
    match f.bionic.set_thread_host(ThreadHost::new(backend)) {
        Err(AbiError::Refused { why, .. }) => {
            assert!(why.contains("stack-guard"), "the refusal must say what breaks: {why}");
        }
        other => panic!("a second thread host must be refused, got {other:?}"),
    }
}

/// **A second guest thread's translations are invalidated when another thread unmaps a range.**
///
/// Phase 2 recorded `ReentrantCall::invalidate_code` as reaching **one** context — "a narrowing
/// of the window, not a closing of it" — and said the registry of live contexts that would close
/// the rest belonged with thread lifecycle. This is that registry, and this is the test that can
/// tell it apart from nothing happening.
///
/// **A detector rather than a watch** (Global Constraint 13): both counters stay at zero under
/// exactly this workload if the broadcast is removed, because there is no other path that raises
/// them. What is asserted is that a range one thread unmapped reached a *different* live context
/// and was applied to its CPU.
#[test]
fn a_range_one_guest_thread_unmaps_reaches_another_threads_context() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&f.guest.backend) as _;
    f.bionic
        .set_thread_host(ThreadHost::new(backend).with_limit(2).with_step_window(1_000))
        .expect("a thread host");
    let out = f.guest.data + 0x800;
    let gate = f.guest.data + 0xC80;
    let running = f.guest.data + 0xC88;
    f.guest.write_u64(gate, 0);
    f.guest.write_u64(running, 0);
    let _gates = OpenOnDrop { space: Arc::clone(&f.guest.space), gates: vec![gate] };

    // A guest thread that keeps running -- so it keeps reaching run-window boundaries, which is
    // where a context applies what another one queued for it.
    let _gates = OpenOnDrop { space: Arc::clone(&f.guest.space), gates: vec![gate] };
    let start = start_routine(&f, |asm| {
        asm.mov(9, running as u64);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
        asm.mov(9, gate as u64);
        let loop_at = asm.pc();
        asm.push(ldr_imm(10, 9, 0));
        asm.push(subs_imm(10, 10, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.mov(0, 0);
    });

    // **Every program this test runs is assembled before the second thread starts.**
    // `Guest::load` flips the whole shared code region to `ReadWrite` and back to `ReadExecute`,
    // and doing that while another guest thread is fetching from it is a fault in that thread
    // rather than anything to do with what is being tested. Found by this test failing about one
    // run in three.
    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
    });
    let mapped = program(&f, |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0x1_0000);
        asm.mov(2, PROT_RW);
        asm.mov(3, MAP_ANON_PRIVATE);
        asm.mov(4, u64::MAX);
        asm.mov(5, 0);
        asm.bl(f.thunk("mmap"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 64));
        asm.mov(1, 0x1_0000);
        asm.bl(f.thunk("munmap"));
        asm.push(str_imm(0, 22, 72));
    });
    let join = program(&f, |asm| {
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 24));
    });

    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0, "the thread was created");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while f.guest.read_u64(running) == 0 {
        assert!(std::time::Instant::now() < deadline, "the second guest thread never started");
        std::thread::yield_now();
    }
    let before = f.boundary.code_invalidations();
    assert_eq!(before.applied, 0, "nothing has been unmapped yet");

    // Now this thread maps and unmaps a range, through the guest's own `mmap`/`munmap`.
    assert!(matches!(run_program(&f, mapped).expect("completes"), ExitReason::Returned { .. }));
    assert_ne!(f.guest.read_u64(out + 64), u64::MAX, "the mapping succeeded");
    assert_eq!(f.guest.read_u64(out + 72), 0, "and the unmapping did");

    // The other context picks it up at its next run-window boundary.
    while f.boundary.code_invalidations().applied == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the other guest thread's context never applied the invalidation: {:?}",
            f.boundary.code_invalidations()
        );
        std::thread::yield_now();
    }
    let after = f.boundary.code_invalidations();
    assert!(after.queued >= 1, "{after:?}");
    assert!(after.applied >= 1, "{after:?}");
    assert_eq!(after.overflows, 0, "two ranges do not fill a queue of 64");

    // Let it go and reap it.
    f.guest.write_u64(gate, 1);
    assert!(matches!(run_program(&f, join).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 24), 0, "the join succeeded");
}

/// **A thread that exits must not hand its arena block to a thread that is still using one.**
///
/// The block index used to come from the table's length, which is exact only for a table nothing
/// is ever removed from — and until this phase nothing was. Remove the entry holding index 1 from
/// a table of three and the length is 2, so the next thread is handed index 2, which is live.
/// Two guest threads would then share one `errno` cell and one `strerror` buffer, and the only
/// symptom would be an occasional wrong error number in a thread that did nothing wrong.
///
/// The sequence is the one that produces the collision, not the one that is easiest to write:
/// start two threads, let the **first** exit, then start a third while the second is still
/// running, and compare the third's `errno` cell with the second's.
#[test]
fn a_thread_that_exits_does_not_give_its_block_to_a_live_thread() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0xD80;
    // Three records of { gate, errno }, one per thread.
    let rec = |n: usize| f.guest.data + 0xD00 + n * 16;
    for n in 0..3 {
        f.guest.write_u64(rec(n), 0);
        f.guest.write_u64(rec(n) + 8, 0);
    }

    // `X0` is this thread's record: publish `__errno()` into it, then spin until its gate opens.
    let start = start_routine(&f, |asm| {
        asm.push(mov_reg(19, 30));
        asm.push(mov_reg(20, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(str_imm(0, 20, 8));
        let loop_at = asm.pc();
        asm.push(ldr_imm(10, 20, 0));
        asm.push(subs_imm(10, 10, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.mov(0, 0);
        asm.push(mov_reg(30, 19));
    });

    // Every program up front: `Guest::load` reprotects the shared code region, and doing that
    // while a guest thread is fetching from it faults that thread.
    let create_two = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, rec(0) as u64);
        create_call(&f, asm, out + 32, 0, start, rec(1) as u64);
    });
    let join_first = program(&f, |asm| {
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 16));
    });
    let create_third = program(&f, |asm| {
        create_call(&f, asm, out + 64, 0, start, rec(2) as u64);
    });
    let join_rest = program(&f, |asm| {
        for slot in [1u64, 2] {
            asm.mov(22, out as u64 + slot * 32);
            asm.push(ldr_imm(0, 22, 0));
            asm.mov(1, 0);
            asm.bl(f.thunk("pthread_join"));
            asm.push(str_imm(0, 22, 16));
        }
    });

    let _gates = OpenOnDrop {
        space: Arc::clone(&f.guest.space),
        gates: (0..3).map(rec).collect(),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let wait_for = |at: omni_cpu::GuestAddr| {
        while f.guest.read_u64(at) == 0 {
            assert!(std::time::Instant::now() < deadline, "a guest thread never published {at:#x}");
            std::thread::yield_now();
        }
        f.guest.read_u64(at)
    };

    assert!(matches!(run_program(&f, create_two).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0);
    assert_eq!(f.guest.read_u64(out + 40), 0);
    let first = wait_for(rec(0) + 8);
    let second = wait_for(rec(1) + 8);
    assert_ne!(first, second, "two live threads already share an errno cell");

    // Let the FIRST one go and reap it, leaving a hole in the middle of the table.
    f.guest.write_u64(rec(0), 1);
    assert!(matches!(run_program(&f, join_first).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 16), 0, "the first join succeeded");

    // Now a third, which must be given the hole and not the block the second is using.
    assert!(matches!(
        run_program(&f, create_third).expect("completes"),
        ExitReason::Returned { .. }
    ));
    assert_eq!(f.guest.read_u64(out + 72), 0, "the third was created");
    let third = wait_for(rec(2) + 8);
    assert_ne!(
        third, second,
        "the third guest thread was handed the block the second is still using: two threads \
         sharing one errno cell is the failure the arena exists to prevent"
    );
    assert_eq!(third, first, "and it should be the block the first one gave back");

    f.guest.write_u64(rec(1), 1);
    f.guest.write_u64(rec(2), 1);
    assert!(matches!(run_program(&f, join_rest).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.bionic.live_guest_threads(), 0);
    assert!(f.bionic.guest_thread_failures().is_empty());
}

// =================================================================== phase 3d: the network group
//
// `socket`, `poll`, `select`, `eventfd`, `getaddrinfo`, `freeaddrinfo`, `gai_strerror`,
// `inet_ntop`. Two answered out of `omni-bionic`, two implemented over the descriptor table that
// already existed, four refused by name.

/// The guest's `AF_INET`/`AF_INET6`, spelled again here so that `omni-bionic`'s constants are
/// compared against a second copy rather than against themselves.
const AF_INET: u64 = 2;
const AF_INET6: u64 = 10;
/// Linux's `EAFNOSUPPORT`, `ENOSPC`, `EINVAL`, `EBADF`, spelled again for the same reason.
const EAFNOSUPPORT: u64 = 97;
const ENOSPC: u64 = 28;
const EINVAL_NET: u64 = 22;
const EBADF_NET: u64 = 9;

/// Build one `struct pollfd` as the guest lays it out: `int fd; short events; short revents;`.
fn pollfd(fd: i32, events: i16) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&fd.to_le_bytes());
    bytes[4..6].copy_from_slice(&events.to_le_bytes());
    bytes
}

/// `revents` of the `index`-th entry of an array at `at`.
fn revents_of(f: &Fixture, at: omni_cpu::GuestAddr, index: usize) -> i16 {
    let bytes = read_guest(f, at + index * 8 + 6, 2);
    i16::from_le_bytes([bytes[0], bytes[1]])
}

/// Make a pipe through real guest code and return `(read end, write end)`.
///
/// Through the guest's own `pipe`, not through `Filesystem::pipe`, so what is tested is the
/// handler and the `int[2]` it writes as well as the seam underneath.
fn pipe_through_guest(f: &Fixture) -> (i32, i32) {
    let at = f.guest.data + 0x40;
    // A sentinel in both slots, so "wrote nothing" and "wrote zero" are distinguishable.
    f.guest.write_u64(at, 0x5A5A_5A5A_5A5A_5A5A);
    let returned = value_of(f, "pipe", |asm| {
        asm.mov(0, at as u64);
    });
    assert_eq!(returned as i64, 0, "pipe() failed");
    let bytes = read_guest(f, at, 8);
    (
        i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    )
}

/// Open a file through real guest code and return the descriptor.
fn open_through_guest(f: &Fixture, path: &str, flags: u64) -> i32 {
    let at = f.cstring(f.guest.data + 0x80, path.as_bytes());
    value_of(f, "open", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, flags);
        asm.mov(2, 0o644);
    }) as i32
}

/// **`inet_ntop` formats both families through a real thunk**, and the string is read back out of
/// guest memory rather than inferred from the return value.
///
/// A handler that returned `dst` without writing anything would pass an assertion on the return
/// value alone, which is why the bytes are what is checked.
#[test]
fn inet_ntop_formats_both_families_through_a_real_thunk() {
    let _guard = serialized();
    let f = fixture();
    let src = f.guest.data + 0x100;
    let dst = f.guest.data + 0x200;

    f.guest.write_bytes(src, &[10, 0, 0, 1]);
    let returned = value_of(&f, "inet_ntop", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, src as u64);
        asm.mov(2, dst as u64);
        asm.mov(3, 64);
    });
    assert_eq!(returned, dst as u64, "inet_ntop returns its destination");
    assert_eq!(f.read_cstring(dst), b"10.0.0.1");

    // `2001:db8::1`, in network byte order.
    let mut v6 = [0u8; 16];
    v6[..2].copy_from_slice(&0x2001u16.to_be_bytes());
    v6[2..4].copy_from_slice(&0x0db8u16.to_be_bytes());
    v6[15] = 1;
    f.guest.write_bytes(src, &v6);
    let returned = value_of(&f, "inet_ntop", |asm| {
        asm.mov(0, AF_INET6);
        asm.mov(1, src as u64);
        asm.mov(2, dst as u64);
        asm.mov(3, 64);
    });
    assert_eq!(returned, dst as u64);
    assert_eq!(f.read_cstring(dst), b"2001:db8::1");
}

/// **A `size` that cannot hold the result is `NULL` with `ENOSPC`, and nothing is written.**
///
/// The `socklen_t` is read from `W3`, not `X3`: the test deliberately leaves rubbish in the high
/// half of `X3`, which is what AAPCS64 permits a caller to do for a 32-bit parameter. A handler
/// that read the whole register would see an enormous `size` and write the address into a buffer
/// the guest said was eight bytes.
#[test]
fn inet_ntop_reads_a_32_bit_socklen_and_refuses_a_short_buffer_without_writing() {
    let _guard = serialized();
    let f = fixture();
    let src = f.guest.data + 0x100;
    let dst = f.guest.data + 0x200;
    f.guest.write_bytes(src, &[10, 0, 0, 1]);
    // A sentinel the call must not disturb.
    f.guest.write_bytes(dst, &[0xAB; 16]);

    let out = f.guest.data + 0x300;
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, AF_INET);
    asm.mov(1, src as u64);
    asm.mov(2, dst as u64);
    // "10.0.0.1" is eight bytes and needs nine. The high half of X3 is 0xFFFF_FFFF, which a
    // handler reading `X3` rather than `W3` would take as part of the size.
    asm.mov(3, 0xFFFF_FFFF_0000_0008);
    asm.bl(f.thunk("inet_ntop"));
    asm.mov(22, out as u64);
    asm.push(str_imm(0, 22, 0));
    asm.bl(f.thunk("__errno"));
    asm.push(ldr_w(1, 0, 0));
    asm.push(str_imm(1, 22, 8));
    asm.push(ret(21));
    f.guest.load(asm.words());
    let mut cpu = f.guest.thread(&f.boundary);
    assert!(matches!(f.run(&mut cpu, entry).expect("completes"), ExitReason::Returned { .. }));

    assert_eq!(f.guest.read_u64(out), 0, "a short buffer is NULL");
    assert_eq!(f.guest.read_u64(out + 8), ENOSPC, "with ENOSPC");
    assert_eq!(
        read_guest(&f, dst, 16),
        vec![0xAB; 16],
        "a refused conversion must not leave a truncated address behind"
    );
}

/// An address family this layer does not format is `EAFNOSUPPORT`, not a refusal and not a fault.
#[test]
fn inet_ntop_with_an_unknown_family_is_eafnosupport() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    // AF_UNIX, with a null source: the family must be checked first, so this is not a fault.
    asm.mov(0, 1);
    asm.mov(1, 0);
    asm.mov(2, (f.guest.data + 0x200) as u64);
    asm.mov(3, 64);
    asm.bl(f.thunk("inet_ntop"));
    asm.mov(22, out as u64);
    asm.push(str_imm(0, 22, 0));
    asm.bl(f.thunk("__errno"));
    asm.push(ldr_w(1, 0, 0));
    asm.push(str_imm(1, 22, 8));
    asm.push(ret(21));
    f.guest.load(asm.words());
    let mut cpu = f.guest.thread(&f.boundary);
    assert!(matches!(f.run(&mut cpu, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out), 0);
    assert_eq!(f.guest.read_u64(out + 8), EAFNOSUPPORT);
}

/// **`inet_pton` parses both families through a real thunk**, and the bytes are read back out of
/// guest memory rather than inferred from the return value.
///
/// The IPv6 case is asserted on all sixteen bytes of an address with a `::` in the middle of it,
/// because the zero run is reinflated by a hand-written overlapping slide: an off-by-one there
/// produces a valid-looking address with the groups in the wrong half.
#[test]
fn inet_pton_parses_both_families_through_a_real_thunk() {
    let _guard = serialized();
    let f = fixture();
    let dst = f.guest.data + 0x200;

    let src = f.cstring(f.guest.data + 0x100, b"192.0.2.1");
    let returned = value_of(&f, "inet_pton", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, src as u64);
        asm.mov(2, dst as u64);
    });
    assert_eq!(returned, 1, "a converted address is 1");
    assert_eq!(read_guest(&f, dst, 4), vec![192, 0, 2, 1]);

    let src = f.cstring(f.guest.data + 0x100, b"2001:db8::8a2e:370:7334");
    let returned = value_of(&f, "inet_pton", |asm| {
        asm.mov(0, AF_INET6);
        asm.mov(1, src as u64);
        asm.mov(2, dst as u64);
    });
    assert_eq!(returned, 1);
    assert_eq!(
        read_guest(&f, dst, 16),
        vec![0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0x8a, 0x2e, 0x03, 0x70, 0x73, 0x34]
    );

    // The embedded quad, which is the one form where the two families' parsers meet.
    let src = f.cstring(f.guest.data + 0x100, b"::ffff:192.0.2.1");
    let returned = value_of(&f, "inet_pton", |asm| {
        asm.mov(0, AF_INET6);
        asm.mov(1, src as u64);
        asm.mov(2, dst as u64);
    });
    assert_eq!(returned, 1);
    assert_eq!(
        read_guest(&f, dst, 16),
        vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 192, 0, 2, 1]
    );
}

/// **A text that is not an address is 0, writes nothing, and does not touch errno.**
///
/// All three in one program, because the third can only be asserted against an errno that is
/// already set: the call before it asks for `AF_UNIX` and leaves `EAFNOSUPPORT` behind, and the
/// malformed parse that follows must leave it exactly there. A handler that reported 0 by
/// setting an errno — `EINVAL` is the believable choice — would pass an assertion on the return
/// value alone, and would break a caller that tries `AF_INET` first and `AF_INET6` second, which
/// is how every "parse an address the user typed" routine is written.
///
/// The address is `01.2.3.4`: **valid dotted-decimal to `inet_aton`, and not an address here.**
/// It is the leading-zero rule end to end rather than in a unit test, because that rule is the
/// one a future widening would take out first.
#[test]
fn inet_pton_answers_zero_without_writing_and_without_setting_errno() {
    let _guard = serialized();
    let f = fixture();
    let src = f.cstring(f.guest.data + 0x100, b"01.2.3.4");
    let dst = f.guest.data + 0x200;
    // A sentinel the refused call must not disturb.
    f.guest.write_bytes(dst, &[0xA5; 16]);
    let out = f.guest.data + 0x300;
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, out as u64);
    // First, a family this layer does not parse: -1, and errno becomes EAFNOSUPPORT.
    asm.mov(0, 1);
    asm.mov(1, src as u64);
    asm.mov(2, dst as u64);
    asm.bl(f.thunk("inet_pton"));
    asm.push(str_imm(0, 22, 0));
    // Then the malformed address, which is 0 and is **not** an error.
    asm.mov(0, AF_INET);
    asm.mov(1, src as u64);
    asm.mov(2, dst as u64);
    asm.bl(f.thunk("inet_pton"));
    asm.push(str_imm(0, 22, 8));
    asm.bl(f.thunk("__errno"));
    asm.push(ldr_w(1, 0, 0));
    asm.push(str_imm(1, 22, 16));
    asm.push(ret(21));
    f.guest.load(asm.words());
    let mut cpu = f.guest.thread(&f.boundary);
    assert!(matches!(f.run(&mut cpu, entry).expect("completes"), ExitReason::Returned { .. }));

    assert_eq!(
        f.guest.read_u64(out),
        u64::MAX,
        "an unsupported family is -1, sign-extended into X0 so `cmp w0, #-1` sees it"
    );
    assert_eq!(f.guest.read_u64(out + 8), 0, "`01.2.3.4` is not an address inet_pton accepts");
    assert_eq!(
        f.guest.read_u64(out + 16),
        EAFNOSUPPORT,
        "a 0 from inet_pton is a parse answer, not an error, and must leave errno alone"
    );
    assert_eq!(
        read_guest(&f, dst, 16),
        vec![0xA5; 16],
        "a refused conversion must not leave part of an address behind: a caller that ignores \
         the return value would read a different host"
    );
}

/// **`gai_strerror` returns a pointer that stays valid**, which is the whole of its contract.
///
/// Two calls with the same code return the **same** address, and a `strerror` in between does not
/// change what it points at. That is the assertion the per-thread scratch would fail: `strerror`
/// writes there, so a `gai_strerror` built on it would hand back a pointer whose bytes the next
/// `strerror` overwrites, and nothing about either call would say so.
#[test]
fn gai_strerror_returns_a_stable_pooled_string() {
    let _guard = serialized();
    let f = fixture();
    let first = value_of(&f, "gai_strerror", |asm| {
        asm.mov(0, 8);
    });
    assert_eq!(f.read_cstring(first as omni_cpu::GuestAddr), b"Name or service not known");

    // A `strerror` on this thread, which writes the per-thread scratch.
    let scratch = value_of(&f, "strerror", |asm| {
        asm.mov(0, 2);
    });
    assert_ne!(scratch, first, "gai_strerror must not share strerror's scratch");

    let again = value_of(&f, "gai_strerror", |asm| {
        asm.mov(0, 8);
    });
    assert_eq!(again, first, "the same code must return the same pointer");
    assert_eq!(
        f.read_cstring(first as omni_cpu::GuestAddr),
        b"Name or service not known",
        "and the bytes must survive an intervening strerror"
    );

    // Every code outside the table shares one row, so a hostile code allocates nothing.
    let unknown = value_of(&f, "gai_strerror", |asm| {
        asm.mov(0, 0xFFFF_FFFF_8000_0000);
    });
    assert_eq!(f.read_cstring(unknown as omni_cpu::GuestAddr), b"Unknown error");
    let unknown_again = value_of(&f, "gai_strerror", |asm| {
        asm.mov(0, 99);
    });
    assert_eq!(unknown_again, unknown, "the fallback row is interned once");
}

/// **`poll` answers every entry and counts only the ready ones**, over a real descriptor.
///
/// Four entries in one array, each a different rule: an open descriptor gets what it asked for, a
/// descriptor that is not open gets `POLLNVAL` whether or not it asked for anything, a negative
/// descriptor is ignored with a zeroed `revents`, and an open descriptor that asked only for
/// `POLLPRI` gets nothing — because out-of-band data is a socket concept and there are no
/// sockets here.
#[test]
fn poll_answers_every_entry_and_counts_only_the_ready_ones() {
    let _guard = serialized();
    let (f, scratch) = rooted("poll");
    std::fs::write(scratch.path("a.bin"), b"hello").expect("a file to poll");
    let fd = open_through_guest(&f, "/a.bin", O_RDONLY);
    assert!(fd >= 3, "a real descriptor: {fd}");

    const POLLIN: i16 = 0x001;
    const POLLPRI: i16 = 0x002;
    const POLLOUT: i16 = 0x004;
    const POLLNVAL: i16 = 0x020;
    const POLLRDNORM: i16 = 0x040;

    let at = f.guest.data + 0x400;
    let mut array = Vec::new();
    array.extend_from_slice(&pollfd(fd, POLLIN | POLLOUT));
    array.extend_from_slice(&pollfd(fd, POLLPRI));
    array.extend_from_slice(&pollfd(61, POLLIN));
    array.extend_from_slice(&pollfd(-1, POLLIN));
    // A sentinel in every `revents`, so "wrote nothing" and "wrote zero" are distinguishable.
    for index in 0..4 {
        array[index * 8 + 6] = 0x5A;
        array[index * 8 + 7] = 0x5A;
    }
    f.guest.write_bytes(at, &array);

    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, 4);
        asm.mov(2, 0);
    });
    assert_eq!(returned as i64, 2, "the open descriptor and the invalid one are ready");
    assert_eq!(revents_of(&f, at, 0), POLLIN | POLLOUT, "a regular file is ready for both");
    assert_eq!(revents_of(&f, at, 1), 0, "nothing here can ever report POLLPRI");
    assert_eq!(revents_of(&f, at, 2), POLLNVAL, "a descriptor that is not open");
    assert_eq!(revents_of(&f, at, 3), 0, "a negative descriptor is ignored, not POLLNVAL");

    // `POLLRDNORM` is the same condition as `POLLIN` here, and asking for it alone gets it alone —
    // the mask is applied to what was asked for rather than returned wholesale.
    f.guest.write_bytes(at, &pollfd(fd, POLLRDNORM));
    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, 1);
        asm.mov(2, 0);
    });
    assert_eq!(returned as i64, 1);
    assert_eq!(revents_of(&f, at, 0), POLLRDNORM, "only what was asked for");
}

/// The three standard streams are pollable, because they are descriptors this runtime has.
///
/// stdin answers `POLLIN` — reading it here is an immediate end of file, which *is* readable —
/// and stdout answers `POLLOUT`. The consistency criterion the module documents is that `poll`
/// predicts what `read` and `write` on that descriptor do **in this runtime**.
#[test]
fn the_standard_streams_are_pollable_because_they_are_descriptors_here() {
    let _guard = serialized();
    let (f, _scratch) = rooted("poll-std");
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    let at = f.guest.data + 0x400;
    let mut array = Vec::new();
    array.extend_from_slice(&pollfd(0, POLLIN));
    array.extend_from_slice(&pollfd(1, POLLOUT));
    array.extend_from_slice(&pollfd(2, POLLOUT));
    f.guest.write_bytes(at, &array);
    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, 3);
        asm.mov(2, 0);
    });
    assert_eq!(returned as i64, 3);
    assert_eq!(revents_of(&f, at, 0), POLLIN);
    assert_eq!(revents_of(&f, at, 1), POLLOUT);
    assert_eq!(revents_of(&f, at, 2), POLLOUT);
}

/// **`poll` with nothing to wait for sleeps for its timeout and returns zero**, and with no
/// descriptors at all it does not need a filesystem to do it.
///
/// The sleep is measured with a one-sided assertion — Windows' ~15.6 ms timer tick means the
/// upper bound is the fidelity gap `omni_platform::clock` documents, and a test that pinned one
/// would be flaky by design.
#[test]
fn poll_with_nothing_ready_sleeps_for_its_timeout_and_returns_zero() {
    let _guard = serialized();
    // No filesystem root at all: `poll(NULL, 0, ms)` is the portable sleep idiom and must not
    // need a descriptor table to answer.
    let f = fixture();
    let started = std::time::Instant::now();
    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 30);
    });
    let elapsed = started.elapsed();
    assert_eq!(returned as i64, 0, "a timeout with nothing ready is zero, not -1");
    assert!(elapsed >= std::time::Duration::from_millis(30), "it slept {elapsed:?}");

    // And a zero timeout returns at once rather than sleeping.
    let started = std::time::Instant::now();
    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
    });
    assert_eq!(returned as i64, 0);
    assert!(started.elapsed() < std::time::Duration::from_millis(500), "a zero timeout blocked");

    // **An array of disabled slots, still with no filesystem.** A program that has stopped using
    // a `pollfd` sets its `fd` negative rather than shortening the array, and POSIX says those
    // entries are ignored — so this call names no descriptor and must not need a descriptor
    // table to answer it.
    let at = f.guest.data + 0x400;
    let mut array = Vec::new();
    array.extend_from_slice(&pollfd(-1, 0x001));
    array.extend_from_slice(&pollfd(-7, 0x004));
    for index in 0..2 {
        array[index * 8 + 6] = 0x5A;
        array[index * 8 + 7] = 0x5A;
    }
    f.guest.write_bytes(at, &array);
    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, 2);
        asm.mov(2, 0);
    });
    assert_eq!(returned as i64, 0, "every entry is ignored, so nothing is ready");
    assert_eq!(revents_of(&f, at, 0), 0, "and every revents is zeroed rather than left alone");
    assert_eq!(revents_of(&f, at, 1), 0);
}

/// **An unbounded wait and an over-long one are refused by name, and a bounded one is not.**
///
/// All three arms, because a refusal that was widened to cover the third would stop a correct
/// guest and a refusal that was narrowed to cover neither would hang the run. The refusal text
/// has to name why nothing can become ready, since that is the fact a reader three thousand
/// initializers deep needs.
#[test]
fn poll_refuses_a_wait_that_nothing_can_end() {
    let _guard = serialized();
    let f = fixture();

    let error = refusal_of(&f, "poll", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0xFFFF_FFFF_FFFF_FFFF); // -1: wait indefinitely
    });
    assert_eq!(error.symbol(), Some("poll"), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("indefinitely"), "{text}");
    assert!(text.contains("socket"), "the refusal must say why nothing can become ready: {text}");

    // 61 seconds, one past the cap.
    let error = refusal_of(&f, "poll", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 61_000);
    });
    assert_eq!(error.symbol(), Some("poll"));
    assert!(error.to_string().contains("60 seconds"), "{error}");

    // And the arm that must not be refused: a wait inside the cap.
    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 1);
    });
    assert_eq!(returned as i64, 0, "a wait inside the cap is carried out, not refused");
}

/// A hostile `nfds` is `EINVAL` rather than a request to read 147 exabytes of guest memory.
#[test]
fn poll_with_a_hostile_nfds_is_einval() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    // 1,025 first, deliberately: it is one past the cap and is an array this layer *could* read,
    // so a missing cap fails the assertion below rather than overflowing the length arithmetic.
    // `SIZE_MAX` is the one that would, and a debug build panics on it — which is a failure too,
    // and a noisier one, so it is not the first thing a reader of a failing run sees.
    for nfds in [1025, u64::MAX, 0x8000_0000_0000_0000] {
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, (f.guest.data + 0x400) as u64);
        asm.mov(1, nfds);
        asm.mov(2, 0);
        asm.bl(f.thunk("poll"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
        asm.push(ret(21));
        f.guest.load(asm.words());
        let mut cpu = f.guest.thread(&f.boundary);
        assert!(matches!(f.run(&mut cpu, entry).expect("completes"), ExitReason::Returned { .. }));
        assert_eq!(f.guest.read_u64(out) as i64, -1, "nfds {nfds}");
        assert_eq!(f.guest.read_u64(out + 8), EINVAL_NET, "nfds {nfds}");
    }
}

/// **A `pollfd` array that is not wholly readable leaves the guest's `revents` untouched, and
/// this is a detector rather than a watch.**
///
/// Review finding **M1**'s direction, applied to this group: decide first, then write once. The
/// array is placed so that its **first** entry is inside the data mapping and its **second** runs
/// off the end of it. A per-entry implementation would answer entry 0 and write its `revents`
/// before it reached the entry that fails; this one reads the whole array first, so entry 0's
/// sentinel has to survive.
///
/// The sentinel is what makes it a detector: the failing entry is unreachable memory in both
/// implementations, so nothing about the *error* distinguishes them — only the byte that the
/// wrong one would have written.
#[test]
fn a_poll_array_that_runs_off_its_mapping_leaves_the_first_entry_untouched() {
    let _guard = serialized();
    let (f, scratch) = rooted("poll-straddle");
    std::fs::write(scratch.path("a.bin"), b"hello").expect("a file");
    let fd = open_through_guest(&f, "/a.bin", O_RDONLY);
    const POLLIN: i16 = 0x001;

    // The last eight bytes of the 64 KiB data mapping: entry 0 fits exactly and entry 1 is past
    // the end.
    let at = f.guest.data + harness::DATA_BYTES - 8;
    let mut entry = pollfd(fd, POLLIN);
    entry[6] = 0x5A;
    entry[7] = 0x5A;
    f.guest.write_bytes(at, &entry);

    let error = refusal_of(&f, "poll", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, 2);
        asm.mov(2, 0);
    });
    assert_eq!(error.symbol(), Some("poll"), "{error:?}");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    assert_eq!(
        revents_of(&f, at, 0),
        0x5A5A,
        "the first entry was answered and written before the call failed, which is the half-\
         updated buffer review finding M1 is about"
    );

    // And the same array with an `nfds` of one — wholly inside the mapping — is answered, so the
    // refusal above is about the range and not about the address.
    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, 1);
        asm.mov(2, 0);
    });
    assert_eq!(returned as i64, 1);
    assert_eq!(revents_of(&f, at, 0), POLLIN);
}

/// **`select` counts a descriptor once per set it is ready in, and zeroes the exception set.**
///
/// POSIX: the return value is the total number of bits set across all the masks, so a descriptor
/// in both the read and the write set counts **twice**. An implementation that counted
/// descriptors rather than bits returns 1 here and looks entirely reasonable.
#[test]
fn select_counts_bits_rather_than_descriptors_and_clears_the_exception_set() {
    let _guard = serialized();
    let (f, scratch) = rooted("select");
    std::fs::write(scratch.path("a.bin"), b"hello").expect("a file");
    let fd = open_through_guest(&f, "/a.bin", O_RDONLY);

    let readfds = f.guest.data + 0x400;
    let writefds = f.guest.data + 0x500;
    let exceptfds = f.guest.data + 0x600;
    let mut bits = vec![0u8; 128];
    bits[(fd / 8) as usize] = 1 << (fd % 8);
    f.guest.write_bytes(readfds, &bits);
    f.guest.write_bytes(writefds, &bits);
    f.guest.write_bytes(exceptfds, &bits);

    let returned = value_of(&f, "select", |asm| {
        asm.mov(0, (fd + 1) as u64);
        asm.mov(1, readfds as u64);
        asm.mov(2, writefds as u64);
        asm.mov(3, exceptfds as u64);
        asm.mov(4, 0);
    });
    assert_eq!(returned as i64, 2, "one descriptor, ready in two sets, is two bits");
    assert_ne!(read_guest(&f, readfds, 8), vec![0u8; 8], "the read set keeps its bit");
    assert_ne!(read_guest(&f, writefds, 8), vec![0u8; 8], "so does the write set");
    assert_eq!(
        read_guest(&f, exceptfds, 8),
        vec![0u8; 8],
        "nothing here can raise an exception condition, so that set is emptied"
    );
}

/// **`select` reports a bad descriptor as `EBADF` for the whole call and rewrites nothing.**
///
/// The bad descriptor is in the *third* set, so an implementation that answered set by set would
/// already have rewritten the first two by the time it found it. The sentinel bits in the first
/// two sets are what notices.
#[test]
fn select_with_a_bad_descriptor_is_ebadf_and_leaves_every_set_alone() {
    let _guard = serialized();
    let (f, scratch) = rooted("select-ebadf");
    std::fs::write(scratch.path("a.bin"), b"hello").expect("a file");
    let fd = open_through_guest(&f, "/a.bin", O_RDONLY);

    let readfds = f.guest.data + 0x400;
    let writefds = f.guest.data + 0x500;
    let exceptfds = f.guest.data + 0x600;
    let mut good = vec![0u8; 128];
    good[(fd / 8) as usize] = 1 << (fd % 8);
    let mut bad = vec![0u8; 128];
    bad[7] = 0x80; // descriptor 63, which nothing opened
    f.guest.write_bytes(readfds, &good);
    f.guest.write_bytes(writefds, &good);
    f.guest.write_bytes(exceptfds, &bad);

    let out = f.guest.data + 0x300;
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, 64);
    asm.mov(1, readfds as u64);
    asm.mov(2, writefds as u64);
    asm.mov(3, exceptfds as u64);
    asm.mov(4, 0);
    asm.bl(f.thunk("select"));
    asm.mov(22, out as u64);
    asm.push(str_imm(0, 22, 0));
    asm.bl(f.thunk("__errno"));
    asm.push(ldr_w(1, 0, 0));
    asm.push(str_imm(1, 22, 8));
    asm.push(ret(21));
    f.guest.load(asm.words());
    let mut cpu = f.guest.thread(&f.boundary);
    assert!(matches!(f.run(&mut cpu, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out) as i64, -1);
    assert_eq!(f.guest.read_u64(out + 8), EBADF_NET);
    assert_eq!(read_guest(&f, readfds, 8), good[..8], "no set may be rewritten before the check");
    assert_eq!(read_guest(&f, writefds, 8), good[..8]);
    assert_eq!(read_guest(&f, exceptfds, 8), bad[..8]);
}

/// An `nfds` outside `[0, FD_SETSIZE]` is `EINVAL`, in both directions.
///
/// Past `FD_SETSIZE` this is stricter than Linux, which clamps; the module documentation says so
/// and says why — a guest `fd_set` is 128 bytes, and honouring a larger `nfds` would read bits out
/// of whatever the guest put after it. The refusal is of the whole call, which is the opposite of
/// silently reading less than was asked for.
#[test]
fn select_with_an_nfds_outside_fd_setsize_is_einval() {
    let _guard = serialized();
    let (f, _scratch) = rooted("select-nfds");
    let out = f.guest.data + 0x300;
    for nfds in [0xFFFF_FFFF_FFFF_FFFFu64, 1025, 0x8000_0000] {
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, nfds);
        asm.mov(1, (f.guest.data + 0x400) as u64);
        asm.mov(2, 0);
        asm.mov(3, 0);
        asm.mov(4, 0);
        asm.bl(f.thunk("select"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
        asm.push(ret(21));
        f.guest.load(asm.words());
        let mut cpu = f.guest.thread(&f.boundary);
        assert!(matches!(f.run(&mut cpu, entry).expect("completes"), ExitReason::Returned { .. }));
        assert_eq!(f.guest.read_u64(out) as i64, -1, "nfds {nfds:#x}");
        assert_eq!(f.guest.read_u64(out + 8), EINVAL_NET, "nfds {nfds:#x}");
    }
    // `nfds` of exactly FD_SETSIZE is the last legal value and must not be refused.
    let returned = value_of(&f, "select", |asm| {
        asm.mov(0, 1024);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
        asm.mov(4, (f.guest.data + 0x700) as u64);
    });
    assert_eq!(returned as i64, 0, "an empty select with a zero timeout is zero");
}

/// A `struct timeval` the guest filled with absurd fields is `EINVAL`, and an unbounded wait is
/// refused by name.
#[test]
fn select_with_a_hostile_timeval_is_einval_and_a_null_timeout_is_refused() {
    let _guard = serialized();
    let f = fixture();
    let tv = f.guest.data + 0x700;
    let out = f.guest.data + 0x300;

    for (seconds, micros) in [(0i64, 1_000_000i64), (0, -1), (-1, 0), (i64::MIN, i64::MIN)] {
        f.guest.write_u64(tv, seconds as u64);
        f.guest.write_u64(tv + 8, micros as u64);
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
        asm.mov(4, tv as u64);
        asm.bl(f.thunk("select"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
        asm.push(ret(21));
        f.guest.load(asm.words());
        let mut cpu = f.guest.thread(&f.boundary);
        assert!(matches!(f.run(&mut cpu, entry).expect("completes"), ExitReason::Returned { .. }));
        assert_eq!(f.guest.read_u64(out) as i64, -1, "timeval {{{seconds}, {micros}}}");
        assert_eq!(f.guest.read_u64(out + 8), EINVAL_NET, "timeval {{{seconds}, {micros}}}");
    }

    // A `tv_sec` past the cap is a refusal rather than an errno: it is well formed and this layer
    // is declining to carry it out.
    f.guest.write_u64(tv, 61);
    f.guest.write_u64(tv + 8, 0);
    let error = refusal_of(&f, "select", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
        asm.mov(4, tv as u64);
    });
    assert_eq!(error.symbol(), Some("select"));
    assert!(error.to_string().contains("60 seconds"), "{error}");

    // And a null timeout is the unbounded wait.
    let error = refusal_of(&f, "select", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, 0);
        asm.mov(4, 0);
    });
    assert_eq!(error.symbol(), Some("select"));
    assert!(error.to_string().contains("indefinitely"), "{error}");
}

/// **A failed `select` leaves the guest's sets exactly as it found them.**
///
/// POSIX: "on failure, the objects pointed to by the readfds, writefds, and errorfds arguments
/// are not modified". The three ways this call fails after it has already read the sets are a
/// malformed `struct timeval`, a `timeout` pointer that is not readable, and a wait past the cap
/// — and in every one of them a guest that retries the call has to still have its sets.
///
/// **This is a defect the first version of the module had**, found by re-reading it rather than
/// by a failing test: the sets were zeroed and written back *before* the timeout was read, so a
/// `tv_usec` of 1,000,000 returned `-1`/`EINVAL` and took the guest's sets with it. The sentinel
/// bits below are what notices; without them, the `-1` and the errno are identical either way.
#[test]
fn a_failed_select_does_not_modify_the_guests_sets() {
    let _guard = serialized();
    // A filesystem root, because the set below names descriptor 0: `select` consults the
    // descriptor table for every descriptor any set mentions, and an instance with no root has
    // no table to consult.
    let (f, _scratch) = rooted("select-fail");
    let exceptfds = f.guest.data + 0x600;
    let tv = f.guest.data + 0x700;

    // Only the exception set has bits, so nothing is ready and the call reaches its timeout —
    // which is the path that used to zero the sets before it looked at the `timeval`.
    let mut bits = vec![0u8; 128];
    bits[0] = 0b0000_0001; // descriptor 0, which is stdin and is always open
    f.guest.write_bytes(exceptfds, &bits);

    let run = |setup: &dyn Fn(&mut Asm)| {
        let out = f.guest.data + 0x300;
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, 8);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, exceptfds as u64);
        setup(&mut asm);
        asm.bl(f.thunk("select"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
        asm.push(ret(21));
        f.guest.load(asm.words());
        let mut cpu = f.guest.thread(&f.boundary);
        let result = f.run(&mut cpu, entry);
        (result, f.guest.read_u64(out) as i64, f.guest.read_u64(out + 8))
    };

    // A malformed `timeval`: `-1`, `EINVAL`, and the set untouched.
    f.guest.write_u64(tv, 0);
    f.guest.write_u64(tv + 8, 1_000_000);
    let (result, returned, errno) = run(&|asm| {
        asm.mov(4, tv as u64);
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(returned, -1);
    assert_eq!(errno, EINVAL_NET);
    assert_eq!(read_guest(&f, exceptfds, 8), bits[..8], "a failed select modified the set");

    // A `timeout` pointer the guest has not mapped: a typed refusal, and the set untouched.
    let (result, _, _) = run(&|asm| {
        asm.mov(4, f.guest.unmapped as u64);
    });
    let error = result.expect_err("an unreadable timeval");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    assert_eq!(read_guest(&f, exceptfds, 8), bits[..8], "a refused select modified the set");

    // A wait past the cap: a refusal naming the cap, and the set untouched.
    f.guest.write_u64(tv, 61);
    f.guest.write_u64(tv + 8, 0);
    let (result, _, _) = run(&|asm| {
        asm.mov(4, tv as u64);
    });
    let error = result.expect_err("a wait past the cap");
    assert!(error.to_string().contains("60 seconds"), "{error}");
    assert_eq!(read_guest(&f, exceptfds, 8), bits[..8], "a refused select modified the set");

    // And the arm that must still zero it: a wait inside the cap, carried out.
    f.guest.write_u64(tv, 0);
    f.guest.write_u64(tv + 8, 1_000);
    let (result, returned, _) = run(&|asm| {
        asm.mov(4, tv as u64);
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(returned, 0);
    assert_eq!(
        read_guest(&f, exceptfds, 8),
        vec![0u8; 8],
        "a select that really timed out must zero the sets"
    );
}

// =========================================== M6: eventfd, which jni-surface.md row 21 reaches

/// **`eventfd` returns a descriptor, and the counter behaves as the kernel's does.**
///
/// One program does the whole round trip, so every step's answer is read out of guest memory
/// rather than inferred: create, write 3, write 4, read, read again. The **second read** is the
/// one that matters -- an eventfd read is destructive, so a counter that was not zeroed would
/// answer 7 again and a test that stopped after the first read could not tell the difference.
#[test]
fn an_eventfd_counts_and_its_read_is_destructive() {
    let _guard = serialized();
    let (f, _root) = rooted("eventfd-round-trip");
    let buf = f.guest.data + 0x300;

    let create = f.thunk("eventfd");
    let write = f.thunk("write");
    let read = f.thunk("read");

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(23, buf as u64);
    // eventfd(0, EFD_NONBLOCK)
    asm.mov(0, 0);
    asm.mov(1, 0o4000);
    asm.bl(create);
    asm.push(str_imm(0, 22, 0));
    asm.push(mov_reg(19, 0));
    // write 3, then 4
    for value in [3u64, 4] {
        asm.mov(0, value);
        asm.push(str_imm(0, 23, 0));
        asm.push(mov_reg(0, 19));
        asm.push(mov_reg(1, 23));
        asm.mov(2, 8);
        asm.bl(write);
        asm.push(str_imm(0, 22, 8));
    }
    // read: one delivery of the accumulated counter
    asm.push(mov_reg(0, 19));
    asm.push(mov_reg(1, 23));
    asm.mov(2, 8);
    asm.bl(read);
    asm.push(str_imm(0, 22, 16));
    asm.push(ldr_imm(0, 23, 0));
    asm.push(str_imm(0, 22, 24));
    // read again: consumed, so EAGAIN under EFD_NONBLOCK
    asm.push(mov_reg(0, 19));
    asm.push(mov_reg(1, 23));
    asm.mov(2, 8);
    asm.bl(read);
    asm.push(str_imm(0, 22, 32));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");

    let fd = f.guest.read_u64(f.guest.data) as i32;
    assert!(fd >= 3, "eventfd must return a real descriptor, got {fd}");
    assert_eq!(f.guest.read_u64(f.guest.data + 8) as i64, 8, "a write is eight bytes");
    assert_eq!(f.guest.read_u64(f.guest.data + 16) as i64, 8, "a read is eight bytes");
    assert_eq!(
        f.guest.read_u64(f.guest.data + 24),
        7,
        "the counter accumulates: 3 + 4, delivered as one read"
    );
    assert_eq!(
        f.guest.read_u64(f.guest.data + 32) as i64,
        -1,
        "the read consumed the counter, so the next one has nothing and EFD_NONBLOCK says EAGAIN"
    );
}

/// **An eventfd's readiness follows its counter**, which is what `poll` reports on it.
///
/// A table whose new kind fell into a "cannot block, therefore always ready" default would report
/// the eventfd readable while its counter was zero -- the exact defect `Entry::readiness` being a
/// `match` with no default arm exists to prevent, and the one this asserts against by polling
/// *before* anything is written.
#[test]
fn an_eventfd_is_not_readable_until_it_has_been_written() {
    let _guard = serialized();
    let (f, _root) = rooted("eventfd-readiness");
    let fs = f.bionic.filesystem().expect("a filesystem");
    let fd = fs.eventfd(0, 0o4000).expect("an eventfd");

    let empty = fs.readiness(fd).expect("the descriptor is open");
    assert!(!empty.readable, "a zero counter is not readable");
    assert!(empty.writable, "and it can always be added to");
    assert_eq!(fs.eventfd_value(fd), Some(0));

    assert_eq!(fs.write(fd, &1u64.to_ne_bytes()).expect("a write"), 8);
    let written = fs.readiness(fd).expect("still open");
    assert!(written.readable, "a non-zero counter is readable");
    assert_eq!(fs.eventfd_value(fd), Some(1));

    // And it is **one** descriptor, not a pair: `pipe_writers` answers only for a pipe, so this
    // says the two kinds are distinct rather than one being served by the other's arm.
    assert_eq!(fs.pipe_writers(fd), None, "an eventfd is not a pipe end");
}

/// **A flag `eventfd2` does not define is `EINVAL`, not an ignored bit.**
///
/// The kernel's answer, and this seam's rule everywhere else. Accepting it would tell the guest it
/// got a behaviour this layer cannot produce, and the flag most likely to be smuggled in is one
/// that changes blocking -- the single decision `omni-platform` refuses to fake.
#[test]
fn an_unknown_eventfd_flag_is_einval() {
    let _guard = serialized();
    let (f, _root) = rooted("eventfd-flags");
    let returned = value_of(&f, "eventfd", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0x4000_0000);
    });
    assert_eq!(returned as i32, -1, "an undefined flag must not produce a descriptor");
}


/// A fixture with a filesystem root **and** a network policy: everything a socket needs.
///
/// The policy is [`NetPolicy::loopback_only`] and never `unrestricted`, so no test in this file
/// can reach the internet even if one were written wrongly — the policy refuses the address
/// before any packet leaves. Creating a socket is not policy-checked (a socket has no destination
/// yet), so every test here that only creates one works under it unchanged.
fn networked(tag: &str) -> (Fixture, Scratch) {
    let (f, scratch) = rooted(tag);
    f.bionic
        .set_network_policy(Arc::new(omni_platform::net::NetPolicy::loopback_only()))
        .expect("a network policy");
    (f, scratch)
}

/// **A socket cannot be created without an embedding having said which network it may reach**,
/// and the refusal names the method that would say it.
///
/// This is D30's replacement for Global Constraint 8 asserted as a property of the *default*
/// rather than of a configuration: an instance that has decided nothing gets no socket at all,
/// and it gets that without anybody remembering to write a check. `NetPolicy::closed()` as a
/// default was rejected for the reason the refusal itself gives — an embedding that forgot would
/// be indistinguishable from one that decided, and the guest would report a network outage this
/// layer had invented.
#[test]
fn a_socket_needs_a_network_policy_and_the_refusal_names_what_would_supply_one() {
    let _guard = serialized();
    let (f, _root) = rooted("socket-no-policy");
    let error = refusal_of(&f, "socket", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, 1); // SOCK_STREAM
        asm.mov(2, 0);
    });
    assert_eq!(error.symbol(), Some("socket"), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("set_network_policy"), "the method that would supply one: {text}");
    assert!(text.contains("D30"), "the decision that made it a policy: {text}");
    assert!(
        text.contains("NetPolicy::closed()"),
        "and why the closed policy is not the default: {text}"
    );
}

/// **The four symbols that left the refusal list answer, and they answer through real guest
/// code.**
///
/// The other half of `the_final_split_of_the_reachable_set_is_what_the_record_claims`: three of
/// these (`socket`, `getaddrinfo`, `freeaddrinfo`) were phase 3d's network refusals and one
/// (`eventfd`) had been answering since M6 while still sitting in that list. A refusal that has
/// been closed belongs in neither place, so the contract that replaced it is pinned here.
///
/// **The resolution is of an address literal**, `127.0.0.1`, so nothing leaves the machine and
/// there is no DNS server to be down: `std` parses a literal itself rather than asking a
/// resolver. What is under test is this layer's marshalling, not the host's resolver.
#[test]
fn the_network_group_answers_once_the_embedding_has_said_what_it_may_reach() {
    let _guard = serialized();
    let (f, _root) = networked("net-answers");

    // `socket` hands out a descriptor out of the same table a file comes from.
    let fd = value_of(&f, "socket", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, 2); // SOCK_DGRAM
        asm.mov(2, 0);
    }) as i32;
    assert!(fd >= 3, "a socket must not collide with a standard stream: fd {fd}");
    assert!(f.bionic.filesystem().expect("a filesystem").is_socket(fd));

    // `eventfd` answers too, which is what stopped it belonging in the refusal list.
    let event = value_of(&f, "eventfd", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
    }) as i32;
    assert!(event >= 3 && event != fd, "two descriptors, two numbers");

    // `getaddrinfo` builds a list in guest memory and hands back its head.
    let node = f.cstring(f.guest.data + 0x100, b"127.0.0.1");
    let service = f.cstring(f.guest.data + 0x140, b"443");
    let res = f.guest.data + 0x180;
    let code = value_of(&f, "getaddrinfo", |asm| {
        asm.mov(0, node as u64);
        asm.mov(1, service as u64);
        asm.mov(2, 0); // no hints
        asm.mov(3, res as u64);
    }) as i32;
    assert_eq!(code, 0, "an address literal resolves without a query leaving the machine");
    let head = read_u64_guest(&f, res);
    assert_ne!(head, 0, "`res` must point at the list");
    assert_eq!(f.bionic.addrinfo_slab().live(), 1, "one result slot is live");

    // And `freeaddrinfo` gives the slot back.
    let returned = value_of(&f, "freeaddrinfo", |asm| {
        asm.mov(0, head);
    });
    let _ = returned;
    assert_eq!(f.bionic.addrinfo_slab().live(), 0, "the slot came back");
}

/// **The keep-alive timing options reach the socket in the GUEST's numbering, not the host's.**
///
/// The detector for the one defect in this area that no other test can see. Linux numbers these
/// `TCP_KEEPIDLE` = 4, `TCP_KEEPINTVL` = 5 and `TCP_KEEPCNT` = 6; Windows numbers the same three
/// 3, 17 and 16 and puts `TCP_MAXRT` on 5. **Every wrong mapping succeeds** — `setsockopt` returns
/// zero, the socket keeps working, and the only difference is a keep-alive that fires at the wrong
/// time on a connection nobody is watching. So the three are set to three *distinct* values and
/// all three are read back: a transposition cannot survive that, and neither a single option nor
/// three equal values would catch one.
///
/// The numbers are written into guest memory and passed as a `const void *`, which is what
/// `setsockopt` takes — a test that passed them in a register would be testing a different ABI.
#[test]
fn the_keep_alive_timing_options_reach_the_socket_in_the_guests_numbering() {
    let _guard = serialized();
    let (f, _root) = networked("keepalive-timing");

    // A stream socket: these are `IPPROTO_TCP` options and a datagram one refuses them.
    let fd = value_of(&f, "socket", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, 1); // SOCK_STREAM
        asm.mov(2, 0);
    }) as i32;
    assert!(fd >= 3, "a socket must not collide with a standard stream: fd {fd}");

    // `SO_KEEPALIVE` first, because the timing configures something that is off by default.
    let on = f.guest.data + 0x100;
    f.guest.write_bytes(on, &1i32.to_le_bytes());
    let rc = value_of(&f, "setsockopt", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, 1); // SOL_SOCKET
        asm.mov(2, 9); // SO_KEEPALIVE
        asm.mov(3, on as u64);
        asm.mov(4, 4);
    }) as i32;
    assert_eq!(rc, 0, "setsockopt(SOL_SOCKET, SO_KEEPALIVE)");

    // Three options, three distinct values, in the guest's own numbering.
    const ROWS: [(u64, i32); 3] = [(4, 120), (5, 31), (6, 7)];
    for (optname, value) in ROWS {
        let at = f.guest.data + 0x120;
        f.guest.write_bytes(at, &value.to_le_bytes());
        let rc = value_of(&f, "setsockopt", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, 6); // IPPROTO_TCP
            asm.mov(2, optname);
            asm.mov(3, at as u64);
            asm.mov(4, 4);
        }) as i32;
        assert_eq!(rc, 0, "setsockopt(IPPROTO_TCP, option {optname}) = {value}");
    }
    for (optname, expected) in ROWS {
        let value_at = f.guest.data + 0x140;
        let len_at = f.guest.data + 0x160;
        f.guest.write_bytes(value_at, &0i32.to_le_bytes());
        f.guest.write_bytes(len_at, &4u32.to_le_bytes());
        let rc = value_of(&f, "getsockopt", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, 6);
            asm.mov(2, optname);
            asm.mov(3, value_at as u64);
            asm.mov(4, len_at as u64);
        }) as i32;
        assert_eq!(rc, 0, "getsockopt(IPPROTO_TCP, option {optname})");
        assert_eq!(
            read_u32_guest(&f, value_at) as i32,
            expected,
            "option {optname} must come back from the host option it was written to; reading \
             another row's value here means the two are transposed"
        );
        assert_eq!(read_u32_guest(&f, len_at), 4, "an int, not a timeval");
    }
}

/// **Zero is `EINVAL` for all three keep-alive figures, and the errno is read the way a guest
/// reads it.**
///
/// Linux range-checks each of them in `do_tcp_setsockopt` and refuses zero. It matters more than
/// a range check usually would: zero is a perfectly good `u32`, it survives every conversion
/// between the guest and the host, and it would arrive at the host meaning *probe with no idle
/// time* or *give up after no probes* — a socket that works, configured to tear itself down.
/// Nothing downstream could tell that from a socket that had been configured properly.
#[test]
fn a_zero_keep_alive_figure_is_einval_rather_than_a_socket_that_tears_itself_down() {
    let _guard = serialized();
    let (f, _root) = networked("keepalive-zero");
    let fd = value_of(&f, "socket", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, 1);
        asm.mov(2, 0);
    }) as i32;
    let zero = f.guest.data + 0x100;
    f.guest.write_bytes(zero, &0i32.to_le_bytes());
    let out = f.guest.data + 0x400;
    for optname in [4u64, 5, 6] {
        let entry = program(&f, |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, 6); // IPPROTO_TCP
            asm.mov(2, optname);
            asm.mov(3, zero as u64);
            asm.mov(4, 4);
            asm.bl(f.thunk("setsockopt"));
            asm.mov(22, out as u64);
            asm.push(str_imm(0, 22, 0));
            asm.bl(f.thunk("__errno"));
            asm.push(ldr_w(1, 0, 0));
            asm.push(str_imm(1, 22, 8));
        });
        assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
        assert_eq!(
            f.guest.read_u64(out) as i64,
            -1,
            "setsockopt(IPPROTO_TCP, option {optname}) = 0 must fail"
        );
        assert_eq!(f.guest.read_u64(out + 8), 22, "EINVAL is Linux's 22");
    }
}

/// **`getsockname` reports the address the socket was bound to, and a short buffer is truncated
/// while the FULL length is reported.**
///
/// The second half is the rule that is easy to get backwards and impossible to notice: `addrlen`
/// is how a caller learns its buffer was not big enough, so writing the truncated length would
/// tell it the address it holds is complete. `recvfrom` shares the same marshalling, which is why
/// one defect here would be two wrong answers.
#[test]
fn getsockname_reports_the_bound_address_and_a_short_buffer_gets_the_full_length() {
    let _guard = serialized();
    let (f, _root) = networked("getsockname");
    let fd = value_of(&f, "socket", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, 2); // SOCK_DGRAM, which can be bound without a peer
        asm.mov(2, 0);
    }) as i32;

    // `bind(fd, {AF_INET, port 0, 127.0.0.1}, 16)`, so the host chooses the port.
    let sockaddr = f.guest.data + 0x100;
    let mut bytes = [0u8; 16];
    bytes[0..2].copy_from_slice(&2u16.to_le_bytes()); // sin_family = AF_INET
    bytes[2..4].copy_from_slice(&0u16.to_be_bytes()); // sin_port = 0, network order
    bytes[4..8].copy_from_slice(&[127, 0, 0, 1]);
    f.guest.write_bytes(sockaddr, &bytes);
    let rc = value_of(&f, "bind", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, sockaddr as u64);
        asm.mov(2, 16);
    }) as i32;
    assert_eq!(rc, 0, "bind to 127.0.0.1:0");

    // The whole address, into a buffer that fits it.
    let out = f.guest.data + 0x200;
    let len_at = f.guest.data + 0x240;
    f.guest.write_bytes(out, &[0xAAu8; 16]);
    f.guest.write_bytes(len_at, &16u32.to_le_bytes());
    let rc = value_of(&f, "getsockname", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, out as u64);
        asm.mov(2, len_at as u64);
    }) as i32;
    assert_eq!(rc, 0, "getsockname on a bound socket");
    assert_eq!(read_u32_guest(&f, len_at), 16, "sizeof(struct sockaddr_in)");
    let got = read_guest(&f, out, 16);
    assert_eq!(&got[0..2], &[2, 0], "sin_family = AF_INET, little-endian u16");
    assert_eq!(&got[4..8], &[127, 0, 0, 1], "the address it was bound to");
    let port = u16::from_be_bytes([got[2], got[3]]);
    assert_ne!(port, 0, "the host chose a port and getsockname is how the guest learns it");

    // **The same call into a buffer that does not fit it.** Eight bytes take the family, the port
    // and the address, and `addrlen` must still say sixteen.
    let short_out = f.guest.data + 0x280;
    let short_len = f.guest.data + 0x2C0;
    f.guest.write_bytes(short_out, &[0xAAu8; 16]);
    f.guest.write_bytes(short_len, &8u32.to_le_bytes());
    let rc = value_of(&f, "getsockname", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, short_out as u64);
        asm.mov(2, short_len as u64);
    }) as i32;
    assert_eq!(rc, 0, "a short buffer is not an error, it is a truncation");
    assert_eq!(
        read_u32_guest(&f, short_len),
        16,
        "the FULL length, not what fitted -- that is how the caller learns it was truncated"
    );
    let truncated = read_guest(&f, short_out, 16);
    assert_eq!(&truncated[0..8], &got[0..8], "the first eight bytes are the address's");
    assert_eq!(&truncated[8..16], &[0xAA; 8], "and nothing past the eight it was given");
}

/// **The `struct addrinfo` list the guest walks is bionic's layout, read back the way the guest
/// reads it.**
///
/// Every assertion follows a pointer out of guest memory rather than recomputing this layer's own
/// arithmetic: `res` to the head, `ai_addr` to the `sockaddr`, `ai_next` to the end. A test that
/// compared against a second copy of the encoder would agree with any layout, including glibc's —
/// and glibc's is the one that would hand the guest a null `char *` where its `sockaddr *`
/// belongs, with the same `sizeof` and the same field set.
#[test]
fn a_resolved_list_is_laid_out_the_way_bionic_lays_one_out() {
    let _guard = serialized();
    let (f, _root) = networked("gai-layout");

    let node = f.cstring(f.guest.data + 0x100, b"127.0.0.1");
    let service = f.cstring(f.guest.data + 0x140, b"443");
    let hints = f.guest.data + 0x200;
    // `ai_family = AF_INET`, `ai_socktype = SOCK_STREAM`, and nothing else set.
    f.guest.write_bytes(hints, &[0u8; 48]);
    f.guest.write_bytes(hints + 4, &(AF_INET as i32).to_le_bytes());
    f.guest.write_bytes(hints + 8, &1i32.to_le_bytes());
    let res = f.guest.data + 0x180;
    let code = value_of(&f, "getaddrinfo", |asm| {
        asm.mov(0, node as u64);
        asm.mov(1, service as u64);
        asm.mov(2, hints as u64);
        asm.mov(3, res as u64);
    }) as i32;
    assert_eq!(code, 0);

    let head = read_u64_guest(&f, res) as omni_cpu::GuestAddr;
    // `ai_flags`, `ai_family`, `ai_socktype`, `ai_protocol`, `ai_addrlen`.
    assert_eq!(read_u32_guest(&f, head + 4) as i32, AF_INET as i32, "ai_family");
    assert_eq!(read_u32_guest(&f, head + 8), 1, "ai_socktype = SOCK_STREAM");
    assert_eq!(read_u32_guest(&f, head + 12), 6, "ai_protocol = IPPROTO_TCP");
    assert_eq!(read_u32_guest(&f, head + 16), 16, "ai_addrlen = sizeof(struct sockaddr_in)");
    // **The two fields whose ORDER is the whole danger.** `ai_canonname` is at 24 and is null;
    // `ai_addr` is at 32 and points at a `sockaddr`. A glibc-derived layout swaps them, has the
    // same `sizeof`, and every size check still passes.
    assert_eq!(read_u64_guest(&f, head + 24), 0, "ai_canonname is null and is the FIRST pointer");
    let ai_addr = read_u64_guest(&f, head + 32) as omni_cpu::GuestAddr;
    assert_ne!(ai_addr, 0, "ai_addr is the SECOND pointer and is not null");
    assert_eq!(read_u64_guest(&f, head + 40), 0, "one address, so ai_next ends the list");

    // The `sockaddr` the guest would hand to `connect`, byte for byte.
    let sockaddr = read_guest(&f, ai_addr, 16);
    assert_eq!(&sockaddr[0..2], &[2, 0], "sin_family = AF_INET, little-endian u16");
    assert_eq!(&sockaddr[2..4], &[0x01, 0xBB], "sin_port = 443 in NETWORK order");
    assert_ne!(&sockaddr[2..4], &[0xBB, 0x01], "and not in the guest's own byte order");
    assert_eq!(&sockaddr[4..8], &[127, 0, 0, 1], "sin_addr, in the order it is written");
}

/// **`freeaddrinfo` matches the head pointer exactly, and a pointer into the middle of a live
/// list is refused rather than freed.**
///
/// The `void` return is what makes this worth a test of its own: there is no value for a wrong
/// answer to be wrong in, so a `freeaddrinfo` that accepted anything would be invisible until the
/// slab handed a live list's storage to the next resolution. The pointer used here —
/// `head + 48` — is `res->ai_next`'s target on a two-node list and is exactly what a guest that
/// walked and freed as it went would pass.
#[test]
fn freeaddrinfo_refuses_a_pointer_that_is_not_the_head_it_handed_out() {
    let _guard = serialized();
    let (f, _root) = networked("gai-free");

    let node = f.cstring(f.guest.data + 0x100, b"127.0.0.1");
    let res = f.guest.data + 0x180;
    let code = value_of(&f, "getaddrinfo", |asm| {
        asm.mov(0, node as u64);
        asm.mov(1, 0); // no service: port 0
        asm.mov(2, 0);
        asm.mov(3, res as u64);
    }) as i32;
    assert_eq!(code, 0);
    let head = read_u64_guest(&f, res);
    assert_eq!(f.bionic.addrinfo_slab().live(), 1);

    for wrong in [head + 48, head + 1, 0xDEAD_0000] {
        let error = refusal_of(&f, "freeaddrinfo", |asm| {
            asm.mov(0, wrong);
        });
        assert_eq!(error.symbol(), Some("freeaddrinfo"), "{error:?}");
        assert!(error.to_string().contains("void"), "{error}");
        assert_eq!(
            f.bionic.addrinfo_slab().live(),
            1,
            "a pointer that is not the head must not free the slot"
        );
    }
    // A null pointer is a no-op, which is what bionic's own `while (ai)` loop does with one.
    let _ = value_of(&f, "freeaddrinfo", |asm| {
        asm.mov(0, 0);
    });
    assert_eq!(f.bionic.addrinfo_slab().live(), 1, "freeaddrinfo(NULL) frees nothing");

    // And the real head does free it, exactly once.
    let _ = value_of(&f, "freeaddrinfo", |asm| {
        asm.mov(0, head);
    });
    assert_eq!(f.bionic.addrinfo_slab().live(), 0);
    let error = refusal_of(&f, "freeaddrinfo", |asm| {
        asm.mov(0, head);
    });
    assert!(matches!(error, AbiError::Refused { .. }), "a double free is refused: {error:?}");
}

/// **A full slab refuses by name and says which of the two things went wrong.**
///
/// Never an overwrite of a list the guest is still walking, and never a truncated one: a guest
/// walking a list cannot tell a node from a node that has been replaced, so reusing a live slot
/// would be silent. The refusal names `addrinfo_slab().live()`, which is what distinguishes a
/// guest that leaks lists from a slab that is too small — and the test reads that number as well,
/// so the two agree.
#[test]
fn a_full_addrinfo_slab_refuses_rather_than_reusing_a_live_slot() {
    let _guard = serialized();
    let (f, _root) = networked("gai-full");
    let node = f.cstring(f.guest.data + 0x100, b"127.0.0.1");
    let res = f.guest.data + 0x180;

    let mut heads = Vec::new();
    for taken in 0..omni_android::bionic::ADDRINFO_RESULTS {
        let code = value_of(&f, "getaddrinfo", |asm| {
            asm.mov(0, node as u64);
            asm.mov(1, 0);
            asm.mov(2, 0);
            asm.mov(3, res as u64);
        }) as i32;
        assert_eq!(code, 0, "slot {taken} of the slab");
        let head = read_u64_guest(&f, res);
        assert!(!heads.contains(&head), "slot {taken} was handed out twice");
        heads.push(head);
    }
    assert_eq!(f.bionic.addrinfo_slab().live(), omni_android::bionic::ADDRINFO_RESULTS);

    let before = read_u64_guest(&f, res);
    let error = refusal_of(&f, "getaddrinfo", |asm| {
        asm.mov(0, node as u64);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, res as u64);
    });
    assert_eq!(error.symbol(), Some("getaddrinfo"), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("slab is full"), "{text}");
    assert!(text.contains("addrinfo_slab().live()"), "and how to tell the two causes apart: {text}");
    assert_eq!(read_u64_guest(&f, res), before, "a refused call writes nothing to `res`");

    // Freeing one makes exactly one more available, which is the free list working rather than
    // the slab having grown.
    let _ = value_of(&f, "freeaddrinfo", |asm| {
        asm.mov(0, heads[2]);
    });
    let code = value_of(&f, "getaddrinfo", |asm| {
        asm.mov(0, node as u64);
        asm.mov(1, 0);
        asm.mov(2, 0);
        asm.mov(3, res as u64);
    }) as i32;
    assert_eq!(code, 0);
    assert_eq!(read_u64_guest(&f, res), heads[2], "the freed slot is the one handed out next");
}

/// **`AI_NUMERICHOST` is honoured, an unknown flag is `EAI_BADFLAGS`, and a family this layer has
/// no socket for is `EAI_FAMILY`.**
///
/// Three different `EAI_*` codes for three different mistakes, because a client that tried one
/// family and then the other branches on the difference. The numbering is bionic's, which counts
/// **up** from 1 where glibc counts down from -1 — so a table taken from the development host
/// would be wrong in a way no size or shape check could see.
#[test]
fn getaddrinfo_reports_the_bionic_eai_codes_and_not_the_hosts() {
    let _guard = serialized();
    let (f, _root) = networked("gai-codes");
    let res = f.guest.data + 0x180;
    let hints = f.guest.data + 0x200;
    let name = f.cstring(f.guest.data + 0x100, b"not-an-address.invalid");

    // AI_NUMERICHOST over a name that is not a literal: EAI_NONAME, and no query leaves.
    f.guest.write_bytes(hints, &[0u8; 48]);
    f.guest.write_bytes(hints, &4i32.to_le_bytes()); // ai_flags = AI_NUMERICHOST
    let code = value_of(&f, "getaddrinfo", |asm| {
        asm.mov(0, name as u64);
        asm.mov(1, 0);
        asm.mov(2, hints as u64);
        asm.mov(3, res as u64);
    }) as i32;
    assert_eq!(code, 8, "EAI_NONAME is 8 on bionic; glibc spells it -2");

    // A flag `netdb.h` does not define.
    f.guest.write_bytes(hints, &[0u8; 48]);
    f.guest.write_bytes(hints, &0x4000i32.to_le_bytes());
    let literal = f.cstring(f.guest.data + 0x100, b"127.0.0.1");
    let code = value_of(&f, "getaddrinfo", |asm| {
        asm.mov(0, literal as u64);
        asm.mov(1, 0);
        asm.mov(2, hints as u64);
        asm.mov(3, res as u64);
    }) as i32;
    assert_eq!(code, 3, "EAI_BADFLAGS");

    // A family this layer has no socket for: AF_UNIX.
    f.guest.write_bytes(hints, &[0u8; 48]);
    f.guest.write_bytes(hints + 4, &1i32.to_le_bytes());
    let code = value_of(&f, "getaddrinfo", |asm| {
        asm.mov(0, literal as u64);
        asm.mov(1, 0);
        asm.mov(2, hints as u64);
        asm.mov(3, res as u64);
    }) as i32;
    assert_eq!(code, 5, "EAI_FAMILY");

    // And the family the literal does not have is EAI_ADDRFAMILY, not EAI_NONAME: the name exists.
    f.guest.write_bytes(hints, &[0u8; 48]);
    f.guest.write_bytes(hints + 4, &10i32.to_le_bytes()); // AF_INET6
    let code = value_of(&f, "getaddrinfo", |asm| {
        asm.mov(0, literal as u64);
        asm.mov(1, 0);
        asm.mov(2, hints as u64);
        asm.mov(3, res as u64);
    }) as i32;
    assert_eq!(code, 1, "EAI_ADDRFAMILY -- the name resolved, just not to this family");
}

/// **An option this seam does not implement is refused by name, carrying the numbers the guest
/// passed.**
///
/// Global Constraint 1 in person.
///
/// # This test asserted the opposite of the code for one session, and that is worth keeping
///
/// It was written when the M6 network run found the engine's HTTP stack setting `SO_KEEPALIVE`
/// (option 9 at `SOL_SOCKET`) on the settings socket and this seam refusing it. The refusal was
/// the finding; the test pinned it. **Then `SO_KEEPALIVE` was implemented in the same uncommitted
/// change and this test was not moved**, so it sat here asserting that an implemented option
/// refuses -- red, in a tree nobody had run this target on.
///
/// It is `VERIFICATION.md` entry 5 exactly: *the regression test for a previous defect is where
/// the next gap hides*, and entry 13's half of it -- a justification that was true when it was
/// written down and false by the time it was used. The repair is the one the split-list tests in
/// this file already make: a symbol that becomes implemented has to be **moved**, and what
/// replaces it must be something that genuinely is not.
///
/// `SO_LINGER` (option 13 at `SOL_SOCKET`) is what replaced it. It is a real option, it is
/// imported-but-never-called territory, and nothing in `omni_platform::net::SocketOption` has a
/// variant for it -- so the refusal it produces is the contract this test is about. The
/// `SO_KEEPALIVE` end of the story is asserted by *calling* it, in
/// `the_keep_alive_timing_options_reach_the_socket_in_the_guests_numbering`.
#[test]
fn an_unimplemented_socket_option_is_refused_with_its_own_numbers() {
    let _guard = serialized();
    let (f, _root) = networked("setsockopt-unknown");
    let fd = value_of(&f, "socket", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, 1);
        asm.mov(2, 0);
    }) as i32;
    let value = f.guest.data + 0x100;
    f.guest.write_bytes(value, &1i32.to_le_bytes());

    let error = refusal_of(&f, "setsockopt", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, 1); // SOL_SOCKET
        asm.mov(2, 13); // SO_LINGER, which nothing implements
        asm.mov(3, value as u64);
        asm.mov(4, 4);
    });
    assert_eq!(error.symbol(), Some("setsockopt"), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("option 13"), "the number the guest passed: {text}");
    assert!(text.contains("level 1"), "and the level: {text}");
    assert!(text.contains("SO_RCVBUF"), "and what IS implemented: {text}");
    assert!(
        text.contains("silently ignored"),
        "and why it is not accepted: {text}"
    );
}

/// **The options that are implemented round-trip through `setsockopt` and `getsockopt`, and
/// `O_NONBLOCK` set with `fcntl` reaches the socket itself.**
///
/// `TCP_NODELAY` is the one asserted by value, because it is a boolean the kernel does not
/// reinterpret — `SO_RCVBUF` is doubled by Linux and rounded by Windows, so a round-trip test on
/// one asserts only that the kernel agreed to *something*.
#[test]
fn the_implemented_socket_options_round_trip_and_fcntl_reaches_the_socket() {
    let _guard = serialized();
    let (f, _root) = networked("sockopt-roundtrip");
    let fd = value_of(&f, "socket", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, 1); // SOCK_STREAM
        asm.mov(2, 0);
    }) as i32;

    let value = f.guest.data + 0x100;
    let length = f.guest.data + 0x140;
    f.guest.write_bytes(value, &1i32.to_le_bytes());
    let set = value_of(&f, "setsockopt", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, 6); // IPPROTO_TCP
        asm.mov(2, 1); // TCP_NODELAY
        asm.mov(3, value as u64);
        asm.mov(4, 4);
    }) as i32;
    assert_eq!(set, 0, "TCP_NODELAY is implemented");

    f.guest.write_bytes(value, &0i32.to_le_bytes());
    f.guest.write_bytes(length, &4u32.to_le_bytes());
    let got = value_of(&f, "getsockopt", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, 6);
        asm.mov(2, 1);
        asm.mov(3, value as u64);
        asm.mov(4, length as u64);
    }) as i32;
    assert_eq!(got, 0);
    assert_ne!(read_u32_guest(&f, value), 0, "TCP_NODELAY reads back as set");
    assert_eq!(read_u32_guest(&f, length), 4, "and the length is the option's own");

    // `SO_ERROR` on a socket nothing has done to it is zero, which is "no pending error".
    f.guest.write_bytes(length, &4u32.to_le_bytes());
    let got = value_of(&f, "getsockopt", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, 1); // SOL_SOCKET
        asm.mov(2, 4); // SO_ERROR
        asm.mov(3, value as u64);
        asm.mov(4, length as u64);
    }) as i32;
    assert_eq!(got, 0);
    assert_eq!(read_u32_guest(&f, value), 0, "a fresh socket has no pending error");

    // `fcntl(F_SETFL, O_NONBLOCK)` on a socket reaches the host, which is what a `recv` answers
    // to. Asserted through the descriptor table rather than through `F_GETFL` alone, because a
    // table that remembered the flag and never told the socket would pass the second on its own.
    let set = value_of(&f, "fcntl", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, 4); // F_SETFL
        asm.mov(2, 0o4000); // O_NONBLOCK
    }) as i32;
    assert_eq!(set, 0);
    let fs = f.bionic.filesystem().expect("a filesystem");
    assert!(fs.is_nonblocking(fd).expect("a socket answers"));
    assert!(
        fs.socket_at(fd).expect("the handle").lock().expect("not poisoned").nonblocking(),
        "the flag was recorded in the table and never reached the socket"
    );
    let flags = value_of(&f, "fcntl", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, 3); // F_GETFL
        asm.mov(2, 0);
    }) as i32;
    assert_eq!(flags & 0o4000, 0o4000, "F_GETFL reports it back");
}

/// **`poll` answers over a socket, and over a socket and a pipe at once, through real guest
/// code.**
///
/// This is the test D25 predicted: "the day a phase binds `socket` for real, that test fails and
/// this module has to grow a real readiness source with it". The readiness under it is now a host
/// `select`, not a table lookup, and three separate things can go wrong that a count of ready
/// descriptors cannot see —
///
/// * a socket reported `POLLIN` because something inherited a file's always-ready answer, which
///   would send the guest into a receive with nothing behind it;
/// * a timeout that never returns, because the wait picked the side of a **mixed** set that
///   cannot wake it — the alternating-slice hazard this module's header is about;
/// * a timeout that returns instantly, which looks like a working poll and is a busy loop.
///
/// Each is asserted on the named entry's own `revents` and on the clock, in a set that has a
/// socket, a pipe and a file in it at once.
#[test]
fn poll_answers_over_a_socket_and_a_pipe_in_the_same_call() {
    let _guard = serialized();
    let (f, _root) = networked("poll-socket");

    let sock = value_of(&f, "socket", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, 2); // SOCK_DGRAM
        asm.mov(2, 0);
    }) as i32;
    let pipefd = f.guest.data + 0x300;
    let made = value_of(&f, "pipe", |asm| {
        asm.mov(0, pipefd as u64);
    }) as i32;
    assert_eq!(made, 0);
    let read_fd = read_u32_guest(&f, pipefd) as i32;

    // Three entries: the socket asked about both directions, the pipe's read end asked about
    // reading, and a descriptor that is not open at all.
    let fds = f.guest.data + 0x400;
    let entry = |slot: usize, fd: i32, events: i16| {
        let at = fds + slot * 8;
        f.guest.write_bytes(at, &fd.to_le_bytes());
        f.guest.write_bytes(at + 4, &events.to_le_bytes());
        f.guest.write_bytes(at + 6, &0i16.to_le_bytes());
    };
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    const POLLNVAL: i16 = 0x020;
    entry(0, sock, POLLIN | POLLOUT);
    entry(1, read_fd, POLLIN);
    entry(2, 4095, POLLIN);

    let started = std::time::Instant::now();
    let ready = value_of(&f, "poll", |asm| {
        asm.mov(0, fds as u64);
        asm.mov(1, 3);
        asm.mov(2, 200); // milliseconds
    }) as i32;
    let took = started.elapsed();

    let revents = |slot: usize| {
        i16::from_le_bytes(read_guest(&f, fds + slot * 8 + 6, 2).try_into().expect("two bytes"))
    };
    // **An unconnected datagram socket is writable and is not readable.** The writable half is
    // the host's answer and not this layer's; the readable half is what `Readiness::ALWAYS` would
    // have got wrong.
    assert_eq!(revents(0) & POLLOUT, POLLOUT, "a datagram socket's send buffer has room");
    assert_eq!(revents(0) & POLLIN, 0, "and nothing has arrived on it");
    // An empty pipe with a live writer is not readable.
    assert_eq!(revents(1), 0, "an empty pipe read end reports nothing");
    // A descriptor nobody opened is `POLLNVAL`, whether or not it was asked for.
    assert_eq!(revents(2), POLLNVAL);
    assert_eq!(ready, 2, "the socket and the bad descriptor; the pipe is not ready");
    assert!(
        took < std::time::Duration::from_secs(5),
        "a poll with something already ready must not sleep: {took:?}"
    );

    // And a mixed set with **nothing** ready returns 0 when its timeout expires, rather than
    // hanging on the side of the set that cannot wake it. Asserted on the clock in both
    // directions: a wait that returned instantly is a busy loop, and one that never returns is
    // the alternating-slice hazard.
    entry(0, sock, POLLIN);
    entry(1, read_fd, POLLIN);
    entry(2, -1, POLLIN);
    let started = std::time::Instant::now();
    let ready = value_of(&f, "poll", |asm| {
        asm.mov(0, fds as u64);
        asm.mov(1, 3);
        asm.mov(2, 150);
    }) as i32;
    let took = started.elapsed();
    assert_eq!(ready, 0, "nothing in the set can become ready inside the timeout");
    assert_eq!(revents(2), 0, "a negative descriptor is ignored and its revents zeroed");
    assert!(
        took >= std::time::Duration::from_millis(120),
        "the call returned in {took:?} from a 150 ms timeout, which is a busy loop"
    );
    assert!(
        took < std::time::Duration::from_secs(10),
        "the call waited {took:?} for a 150 ms timeout: the wait picked a side of the mixed set \
         that cannot wake it"
    );
}

/// **A family or a socket type this layer has no primitive for is refused by name, not answered
/// `-1`.**
///
/// The argument the old blanket `socket` refusal made, kept for the cases that are still true: a
/// networked program branches on `EAFNOSUPPORT` quietly — it switches off whatever needed the
/// socket — so the run would complete and nothing anywhere would record that this layer, rather
/// than the device, had decided. `AF_INET6` is **no longer** in this list, which is the part that
/// changed: it is created for real, and a host with no IPv6 is what reports `EAFNOSUPPORT` now.
#[test]
fn a_socket_family_with_no_primitive_is_refused_by_name_rather_than_answered() {
    let _guard = serialized();
    let (f, _root) = networked("socket-refusals");

    for (domain, kind, named) in [(1u64, 1u64, "AF_UNIX"), (16, 2, "AF_NETLINK")] {
        let error = refusal_of(&f, "socket", |asm| {
            asm.mov(0, domain);
            asm.mov(1, kind);
            asm.mov(2, 0);
        });
        assert_eq!(error.symbol(), Some("socket"), "{error:?}");
        let text = error.to_string();
        assert!(text.contains(named), "the family, named: {text}");
        assert!(text.contains("EAFNOSUPPORT"), "the wrong answer it declines: {text}");
    }
    // `SOCK_RAW` is a type with no primitive, and it is refused with the type named.
    let error = refusal_of(&f, "socket", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, 3); // SOCK_RAW
        asm.mov(2, 0);
    });
    assert!(error.to_string().contains("SOCK_RAW"), "{error}");
}

/// **Every symbol that produces a descriptor has had its readiness decided, asserted
/// mechanically.**
///
/// This test's predecessor asserted something stronger and now false: that the descriptor space
/// was closed under kinds that *cannot block*, so `poll` could report every open descriptor as
/// ready. D25 wrote down what it was for — "the day a phase binds `socket` for real, that test
/// fails and this module has to grow a real readiness source with it" — and **M5 was that day**,
/// with `pipe` rather than `socket`.
///
/// It was replaced rather than updated, which is the distinction that matters: updating it would
/// have kept a green test that no longer asserted anything. What it asserts now is that the list
/// of descriptor-producing symbols bound here is exactly the list whose readiness
/// `omni-platform`'s `Entry::readiness` has decided, so binding an eleventh one without deciding
/// still fails.
#[test]
fn every_symbol_that_produces_a_descriptor_has_had_its_readiness_decided() {
    let bound: std::collections::BTreeSet<&str> = Bionic::bound_symbols().collect();
    // Every POSIX symbol that hands out a descriptor, whether or not it is in the reachable 188.
    let descriptor_makers = [
        "accept", "accept4", "creat", "dup", "dup2", "dup3", "epoll_create", "epoll_create1",
        "eventfd", "inotify_init", "inotify_init1", "memfd_create", "open", "openat", "__open_2",
        "opendir", "pipe", "pipe2", "signalfd", "socket", "socketpair", "timerfd_create",
    ];
    let present: Vec<&str> =
        descriptor_makers.iter().copied().filter(|s| bound.contains(s)).collect();
    assert_eq!(
        present,
        vec![
            "epoll_create1",
            "eventfd",
            "open",
            "__open_2",
            "opendir",
            "pipe",
            "socket",
            "timerfd_create"
        ],
        "a symbol that produces a descriptor was bound without `poll` being told about it"
    );
    // **`epoll_create1` is the seventh, and its decision is a refusal rather than an answer.** An
    // epoll descriptor's readiness is its members', possibly on both waiting sides at once; no run
    // has polled one, so `Filesystem::readiness` refuses it by name and every consumer that would
    // ask -- `poll`, `select`, `ALooper_addFd` -- refuses before it can read that refusal as
    // "closed". Asserted at the end of this test for `poll`, the one whose old arm said no such
    // refusal could reach it.
    // **And all six of them now answer, which is the sentence that changed in M6.** The version
    // of this test before it asserted the opposite for two of them -- "nothing in this runtime
    // models a socket's readiness, and `poll` would have to answer for one" -- and called them by
    // name to prove it. That was true and D25 wrote down the day it would stop being true: a
    // socket's readiness is now `omni_platform::net::Socket::readiness`, which is a real `select`
    // on the host, and `Entry::readiness` has an arm for it with no default to inherit.
    //
    // So what is asserted instead is the thing that can still go wrong: every descriptor kind
    // this layer hands out has a `ReadinessSource`, and the three sources are distinct. A seventh
    // kind added without deciding fails `Filesystem::readiness_source`'s own match, which has no
    // default arm either.
    let _guard = serialized();
    let (f, _root) = networked("readiness-decided");
    let path = f.cstring(f.guest.data + 0x100, b"/f.txt");
    let file = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_WRONLY | O_CREAT);
        asm.mov(2, 0o644);
    }) as i32;
    let event = value_of(&f, "eventfd", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
    }) as i32;
    let sock = value_of(&f, "socket", |asm| {
        asm.mov(0, AF_INET);
        asm.mov(1, 2);
        asm.mov(2, 0);
    }) as i32;
    let fs = f.bionic.filesystem().expect("a filesystem");
    use omni_platform::fs::ReadinessSource;
    assert_eq!(fs.readiness_source(file), Some(ReadinessSource::Immediate));
    assert_eq!(fs.readiness_source(event), Some(ReadinessSource::Gate));
    assert_eq!(
        fs.readiness_source(sock),
        Some(ReadinessSource::Host),
        "a socket's readiness is the operating system's answer and nothing else's"
    );
    assert_ne!(
        fs.readiness(sock).expect("the host answered"),
        omni_platform::fs::Readiness::ALWAYS,
        "ALWAYS is a regular file's answer and would send `poll` into a receive with nothing          behind it"
    );
    let epfd = value_of(&f, "epoll_create1", |asm| {
        asm.mov(0, 0);
    }) as i32;
    assert_eq!(fs.readiness_source(epfd), None, "no one waiting side is true of an epoll set");
    let at = f.guest.data + 0x400;
    let mut entry = [0u8; 8];
    entry[0..4].copy_from_slice(&epfd.to_le_bytes());
    entry[4..6].copy_from_slice(&1i16.to_le_bytes());
    f.guest.write_bytes(at, &entry);
    let error = refusal_of(&f, "poll", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, 1);
        asm.mov(2, 0);
    });
    assert!(
        error.to_string().contains("epoll"),
        "poll on an epoll descriptor refuses by name rather than answering POLLNVAL: {error}"
    );
    // **`timerfd_create` is the eighth**, and the first whose readiness changes with time: its
    // source says so, which is what makes a waiter cap its wait at the deadline.
    let timer = value_of(&f, "timerfd_create", |asm| {
        asm.mov(0, 1);
        asm.mov(1, 0o4000);
    }) as i32;
    assert_eq!(fs.readiness_source(timer), Some(ReadinessSource::Timer));
}

/// **The kinds that are modelled each answer for themselves, through real guest code.**
///
/// A file is always ready; an empty pipe's read end is not readable and its write end is; and the
/// same read end is readable the moment a byte is in it. *A count of ready descriptors cannot see
/// any of that* — every assertion here is on the named entry's own `revents`, which is this
/// project's first rule about counts.
#[test]
fn poll_reports_a_pipes_real_readiness_and_not_merely_that_it_is_open() {
    let _guard = serialized();
    let (f, scratch) = rooted("poll-pipe");
    std::fs::write(scratch.path("a.bin"), b"hello").expect("a file to poll");
    let file_fd = open_through_guest(&f, "/a.bin", O_RDONLY);
    assert!(file_fd >= 3, "a real descriptor: {file_fd}");

    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    const POLLHUP: i16 = 0x010;

    let (read_fd, write_fd) = pipe_through_guest(&f);
    assert!(read_fd >= 3 && write_fd > read_fd, "a real pipe: {read_fd} and {write_fd}");

    let at = f.guest.data + 0x400;
    let mut array = Vec::new();
    array.extend_from_slice(&pollfd(file_fd, POLLIN | POLLOUT));
    array.extend_from_slice(&pollfd(read_fd, POLLIN));
    array.extend_from_slice(&pollfd(write_fd, POLLOUT));
    f.guest.write_bytes(at, &array);
    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, 3);
        asm.mov(2, 0);
    });
    assert_eq!(revents_of(&f, at, 0), POLLIN | POLLOUT, "a file is ready for both");
    assert_eq!(revents_of(&f, at, 1), 0, "an empty pipe's read end is not readable");
    assert_eq!(revents_of(&f, at, 2), POLLOUT, "an empty pipe's write end is writable");
    assert_eq!(returned as i64, 2, "the count agrees, which is the weaker statement");

    // One byte through the guest's own `write`, and the read end changes its answer.
    let payload = f.cstring(f.guest.data + 0x80, b"!");
    let written = value_of(&f, "write", |asm| {
        asm.mov(0, write_fd as u64);
        asm.mov(1, payload as u64);
        asm.mov(2, 1);
    });
    assert_eq!(written as i64, 1, "one byte into the pipe");
    f.guest.write_bytes(at, &pollfd(read_fd, POLLIN));
    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, 1);
        asm.mov(2, 0);
    });
    assert_eq!(revents_of(&f, at, 0), POLLIN, "a pipe with a byte in it is readable");
    assert_eq!(returned as i64, 1);

    // And with the write end closed and the byte drained, the read end reports `POLLHUP` —
    // **whether or not it was asked for**, which is what makes the canonical drain loop see end
    // of file instead of spinning.
    let buffer = f.guest.data + 0x200;
    let read = value_of(&f, "read", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, buffer as u64);
        asm.mov(2, 16);
    });
    assert_eq!(read as i64, 1, "the one byte comes back");
    let closed = value_of(&f, "close", |asm| {
        asm.mov(0, write_fd as u64);
    });
    assert_eq!(closed as i64, 0);
    f.guest.write_bytes(at, &pollfd(read_fd, POLLIN));
    let returned = value_of(&f, "poll", |asm| {
        asm.mov(0, at as u64);
        asm.mov(1, 1);
        asm.mov(2, 0);
    });
    assert_eq!(
        revents_of(&f, at, 0),
        POLLIN | POLLHUP,
        "end of file is readable, and POLLHUP is reported without being asked for"
    );
    assert_eq!(returned as i64, 1);
}

// =================================================================== M5: pipes and fcntl

/// `O_NONBLOCK` as the guest spells it: Linux UAPI `asm-generic/fcntl.h`, octal 4000.
const O_NONBLOCK_GUEST: u64 = 0o4000;
/// `F_GETFL` and `F_SETFL`.
const F_GETFL_GUEST: u64 = 3;
const F_SETFL_GUEST: u64 = 4;
/// `EAGAIN` and `EPIPE`, as the guest's own `errno` must carry them.
const EAGAIN_GUEST: u64 = 11;
const EPIPE_GUEST: u64 = 32;

/// **Bytes written into one end come out of the other, through real translated ARM64 code.**
///
/// The `int[2]` is asserted against a sentinel, so a handler that returned `0` and wrote nothing
/// is distinguishable from one that wrote two descriptors.
#[test]
fn a_pipe_round_trips_bytes_through_real_guest_code() {
    let _guard = serialized();
    let (f, _scratch) = rooted("pipe-roundtrip");
    let (read_fd, write_fd) = pipe_through_guest(&f);
    assert!(read_fd >= 3, "a real descriptor: {read_fd}");
    assert_eq!(write_fd, read_fd + 1, "the lowest two free descriptors, in order");

    let source = f.cstring(f.guest.data + 0x100, b"APP_CMD");
    let written = value_of(&f, "write", |asm| {
        asm.mov(0, write_fd as u64);
        asm.mov(1, source as u64);
        asm.mov(2, 7);
    });
    assert_eq!(written as i64, 7);

    let destination = f.guest.data + 0x200;
    f.guest.write_u64(destination, 0x5A5A_5A5A_5A5A_5A5A);
    let read = value_of(&f, "read", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, destination as u64);
        asm.mov(2, 64);
    });
    assert_eq!(read as i64, 7, "a read from a pipe returns what is there, not what was asked for");
    assert_eq!(&read_guest(&f, destination, 7), b"APP_CMD", "the bytes, in order");

    // And the pipe is empty again: a second read on the now-non-blocking end says so rather than
    // repeating the bytes.
    let set = value_of(&f, "fcntl", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, F_SETFL_GUEST);
        asm.mov(2, O_NONBLOCK_GUEST);
    });
    assert_eq!(set as i64, 0);
    let out = f.guest.data + 0x400;
    let entry = program(&f, |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, destination as u64);
        asm.mov(2, 64);
        asm.bl(f.thunk("read"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out) as i64, -1, "an empty non-blocking pipe reads -1");
    assert_eq!(f.guest.read_u64(out + 8), EAGAIN_GUEST, "with EAGAIN, which is Linux's 11");
}

/// **`fcntl` round-trips `O_NONBLOCK` and refuses every other command by name.**
///
/// `F_GETFL` answers the flag and nothing else — in particular not a fabricated access mode,
/// which is the value a guest would branch on.
#[test]
fn fcntl_round_trips_o_nonblock_and_refuses_every_other_command() {
    let _guard = serialized();
    let (f, _scratch) = rooted("fcntl");
    let (read_fd, _write_fd) = pipe_through_guest(&f);

    let flags = value_of(&f, "fcntl", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, F_GETFL_GUEST);
        asm.mov(2, 0);
    });
    assert_eq!(flags as i64, 0, "a fresh pipe is blocking, and no access mode is invented");

    let set = value_of(&f, "fcntl", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, F_SETFL_GUEST);
        asm.mov(2, O_NONBLOCK_GUEST);
    });
    assert_eq!(set as i64, 0);
    let flags = value_of(&f, "fcntl", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, F_GETFL_GUEST);
        asm.mov(2, 0);
    });
    assert_eq!(flags, O_NONBLOCK_GUEST, "what was set comes back");

    // Clearing it is the other direction, and a layer that only ever sets would pass every
    // assertion above.
    let cleared = value_of(&f, "fcntl", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, F_SETFL_GUEST);
        asm.mov(2, 0);
    });
    assert_eq!(cleared as i64, 0);
    let flags = value_of(&f, "fcntl", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, F_GETFL_GUEST);
        asm.mov(2, 0);
    });
    assert_eq!(flags, 0, "and clearing it clears it");

    // A descriptor nobody opened is `EBADF`, which is an answer rather than a refusal.
    let out = f.guest.data + 0x400;
    let entry = program(&f, |asm| {
        asm.mov(0, 61);
        asm.mov(1, F_GETFL_GUEST);
        asm.mov(2, 0);
        asm.bl(f.thunk("fcntl"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out) as i64, -1);
    assert_eq!(f.guest.read_u64(out + 8), EBADF_NET, "EBADF is Linux's 9");

    // **Every other command refuses, and the refusal names it.** `F_DUPFD` and `F_SETFD` are the
    // two a guest is most likely to reach for, and `F_GETPIPE_SZ` is the one whose believable
    // wrong answer -- this layer's own capacity -- would be a promise about a pipe the guest
    // could then resize.
    for (command, name) in [(0u64, "F_DUPFD"), (2, "F_SETFD"), (1032, "F_GETPIPE_SZ")] {
        let error = refusal_of(&f, "fcntl", |asm| {
            asm.mov(0, read_fd as u64);
            asm.mov(1, command);
            asm.mov(2, 0);
        });
        assert_eq!(error.symbol(), Some("fcntl"));
        assert!(error.to_string().contains(name), "the refusal must name {name}: {error}");
    }
    // And one nobody has a name for still refuses, with its number.
    let error = refusal_of(&f, "fcntl", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, 4242);
        asm.mov(2, 0);
    });
    assert!(error.to_string().contains("4242"), "{error}");

    // A bit beyond O_NONBLOCK is refused rather than ignored: O_ASYNC (0o20000) asks for a signal
    // this runtime does not deliver, and Linux's own `F_SETFL` would accept it.
    let error = refusal_of(&f, "fcntl", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, F_SETFL_GUEST);
        asm.mov(2, 0o20000);
    });
    assert!(error.to_string().contains("O_ASYNC"), "{error}");
}

/// **`pwrite` writes at an offset through real guest code and leaves the sequential offset alone.**
///
/// Asserted on the bytes the host file ends up holding, which is the only place the two defects
/// this guards show: a `pwrite` that moved the offset puts the following `write` after the page
/// instead of at the end, and a `pwrite` that ignored its offset appends. Both leave every return
/// value looking right.
#[test]
fn pwrite_writes_at_its_offset_and_the_next_write_continues_where_write_left_off() {
    let _guard = serialized();
    let (f, scratch) = rooted("pwrite");
    let fd = open_through_guest(&f, "/pages", O_RDWR | O_CREAT);
    assert!(fd >= 3, "open returned {fd}");
    let source = f.guest.data + 0x200;
    let write_through_guest = |symbol: &str, bytes: &[u8], offset: Option<u64>| -> i64 {
        f.guest.write_bytes(source, bytes);
        value_of(&f, symbol, |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, source as u64);
            asm.mov(2, bytes.len() as u64);
            if let Some(offset) = offset {
                asm.mov(3, offset);
            }
        }) as i64
    };
    assert_eq!(write_through_guest("write", b"0123456789", None), 10);
    assert_eq!(write_through_guest("pwrite", b"ab", Some(2)), 2, "pwrite returns what it wrote");
    assert_eq!(write_through_guest("write", b"XY", None), 2);
    assert_eq!(
        std::fs::read(scratch.path("pages")).expect("the host file"),
        b"01ab456789XY",
        "the page landed at offset 2 and the sequential write at the end"
    );
    // A negative offset is EINVAL, not a write somewhere.
    assert_eq!(write_through_guest("pwrite", b"zz", Some(u64::MAX)), -1);
    assert_eq!(
        std::fs::read(scratch.path("pages")).expect("the host file"),
        b"01ab456789XY",
        "nothing was written"
    );
}

/// **`geteuid` answers the application uid the embedding gave, and refuses until it is given.**
///
/// Three things, each a separate failure: the refusal names the setter (a guest that dies on it
/// must say what to do); the value is exactly the one given (a uid chosen here would be the
/// plausible wrong answer); and the setter refuses root and anything outside the application
/// range, because SQLite -- the measured caller -- `fchown`s every file it creates when it is
/// root.
#[test]
fn geteuid_answers_the_application_uid_the_embedding_gave_and_nothing_else() {
    let _guard = serialized();
    let f = fixture();
    let error = refusal_of(&f, "geteuid", |_| {});
    assert_eq!(error.symbol(), Some("geteuid"));
    assert!(error.to_string().contains("set_app_uid"), "{error}");

    for bad in [0u32, 1000, 9_999, 20_000, 100_000, 99_999] {
        assert!(f.bionic.set_app_uid(bad).is_err(), "uid {bad} is not an application uid");
    }
    assert_eq!(f.bionic.app_uid(), None, "a refused uid is not kept");

    // User 10's copy of an app, to show the per-user offset is understood rather than ignored.
    let uid = 10 * 100_000 + 10_123;
    f.bionic.set_app_uid(uid).expect("an application uid");
    assert_eq!(value_of(&f, "geteuid", |_| {}) as u32, uid);
    assert!(f.bionic.set_app_uid(10_124).is_err(), "a process's uid does not change under it");
    assert_eq!(value_of(&f, "geteuid", |_| {}) as u32, uid, "and the first one stands");
}

/// `F_GETLK`, `F_SETLK`, `F_SETLKW` and the three `l_type`s, Linux `asm-generic/fcntl.h`.
const F_GETLK_GUEST: u64 = 5;
const F_SETLK_GUEST: u64 = 6;
const F_SETLKW_GUEST: u64 = 7;
const F_RDLCK_GUEST: i16 = 0;
const F_WRLCK_GUEST: i16 = 1;
const F_UNLCK_GUEST: i16 = 2;
/// SQLite's `PENDING_BYTE`, the byte the engine's SQLite locked when the refusal killed it.
const SQLITE_PENDING_BYTE: i64 = 0x4000_0000;

/// One `fcntl` record-lock call through real guest code: the `struct flock` is written into
/// guest memory in LP64 layout, and `(return value, errno, the struct afterwards)` comes back.
fn lock_through_guest(
    f: &Fixture,
    fd: i32,
    command: u64,
    (l_type, l_whence, l_start, l_len): (i16, i16, i64, i64),
) -> (i64, u64, Vec<u8>) {
    let flock = f.guest.data + 0x300;
    let mut bytes = [0u8; 32];
    bytes[0..2].copy_from_slice(&l_type.to_le_bytes());
    bytes[2..4].copy_from_slice(&l_whence.to_le_bytes());
    bytes[8..16].copy_from_slice(&l_start.to_le_bytes());
    bytes[16..24].copy_from_slice(&l_len.to_le_bytes());
    f.guest.write_bytes(flock, &bytes);
    let out = f.guest.data + 0x400;
    // errno is cleared first, so a success that left a stale value cannot read as a failure.
    f.guest.write_u64(out + 8, 0);
    let entry = program(f, |asm| {
        asm.bl(f.thunk("__errno"));
        asm.mov(9, 0);
        asm.push(str_w(9, 0, 0));
        asm.mov(0, fd as u64);
        asm.mov(1, command);
        asm.mov(2, flock as u64);
        asm.bl(f.thunk("fcntl"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
    });
    assert!(matches!(run_program(f, entry).expect("completes"), ExitReason::Returned { .. }));
    (f.guest.read_u64(out) as i64, f.guest.read_u64(out + 8), read_guest(f, flock, 32))
}

/// **The engine's SQLite takes POSIX record locks, and they are granted -- validated the way
/// Linux validates them, in Linux's order.**
///
/// MEASURED on M6's gate: a guest worker died on `fcntl(F_SETLK)` over SQLite's `PENDING_BYTE`
/// (`unixFileLock`, `libroblox.so` link `0x22d72a4`, called from `0x22d70d0`). A record lock
/// belongs to a process and this instance is the only process that can lock its files, so a
/// valid request always succeeds -- see `Filesystem::record_lock`. What *can* fail is asserted
/// errno by errno, because each is a branch SQLite has.
#[test]
fn the_engines_sqlite_record_locks_are_granted_and_validated_like_linux() {
    let _guard = serialized();
    let (f, _scratch) = rooted("fcntl-lock");
    let rw = open_through_guest(&f, "/db", O_RDWR | O_CREAT);
    let ro = open_through_guest(&f, "/db", O_RDONLY);
    let wo = open_through_guest(&f, "/db", O_WRONLY);
    assert!(rw >= 3 && ro >= 3 && wo >= 3, "{rw} {ro} {wo}");

    // SQLite's own request, byte for byte: a shared lock on PENDING_BYTE, then the others.
    let pending = |l_type| (l_type, 0, SQLITE_PENDING_BYTE, 1);
    for (command, lock, name) in [
        (F_SETLK_GUEST, pending(F_RDLCK_GUEST), "F_SETLK F_RDLCK"),
        (F_SETLK_GUEST, pending(F_WRLCK_GUEST), "F_SETLK F_WRLCK"),
        (F_SETLKW_GUEST, pending(F_WRLCK_GUEST), "F_SETLKW F_WRLCK -- which must not wait"),
        (F_SETLK_GUEST, pending(F_UNLCK_GUEST), "F_SETLK F_UNLCK"),
        (F_SETLK_GUEST, (F_WRLCK_GUEST, 0, 0, 0), "a zero length, which is to end of file"),
    ] {
        let (ret, errno, _) = lock_through_guest(&f, rw, command, lock);
        assert_eq!((ret, errno), (0, 0), "{name} on a read-write descriptor");
    }

    // F_GETLK: nothing conflicts, so l_type comes back F_UNLCK and nothing else is touched.
    let (ret, errno, after) =
        lock_through_guest(&f, ro, F_GETLK_GUEST, (F_WRLCK_GUEST, 0, SQLITE_PENDING_BYTE, 510));
    assert_eq!((ret, errno), (0, 0), "F_GETLK does not check the access mode");
    assert_eq!(i16::from_le_bytes([after[0], after[1]]), F_UNLCK_GUEST, "no conflicting lock");
    assert_eq!(i64::from_le_bytes(after[8..16].try_into().unwrap()), SQLITE_PENDING_BYTE);
    assert_eq!(i64::from_le_bytes(after[16..24].try_into().unwrap()), 510);

    // EBADF: a lock the descriptor's access mode cannot hold.
    let (ret, errno, _) = lock_through_guest(&f, ro, F_SETLK_GUEST, pending(F_WRLCK_GUEST));
    assert_eq!((ret, errno), (-1, EBADF_NET), "an exclusive lock on a read-only descriptor");
    let (ret, errno, _) = lock_through_guest(&f, wo, F_SETLK_GUEST, pending(F_RDLCK_GUEST));
    assert_eq!((ret, errno), (-1, EBADF_NET), "a shared lock on a write-only descriptor");
    let (ret, errno, _) = lock_through_guest(&f, ro, F_SETLK_GUEST, pending(F_UNLCK_GUEST));
    assert_eq!((ret, errno), (0, 0), "a release needs neither");

    // EINVAL and EOVERFLOW, and the ORDER: a bad l_whence on a read-only descriptor asking for an
    // exclusive lock is EINVAL, not EBADF, because Linux checks the request before the mode.
    for (fd, command, lock, want, name) in [
        (rw, F_SETLK_GUEST, (7, 0, 0, 1), EINVAL_NET, "an l_type that is none of the three"),
        (rw, F_GETLK_GUEST, pending(F_UNLCK_GUEST), EINVAL_NET, "F_GETLK asking about F_UNLCK"),
        (ro, F_SETLK_GUEST, (F_WRLCK_GUEST, 9, 0, 1), EINVAL_NET, "l_whence 9, before EBADF"),
        (rw, F_SETLK_GUEST, (F_WRLCK_GUEST, 0, -1, 1), EINVAL_NET, "a negative start"),
        (rw, F_SETLK_GUEST, (F_WRLCK_GUEST, 0, 4, -5), EINVAL_NET, "a length reaching below 0"),
        (rw, F_SETLK_GUEST, (F_WRLCK_GUEST, 0, i64::MAX, 2), 75, "an end past off_t: EOVERFLOW"),
    ] {
        let (ret, errno, _) = lock_through_guest(&f, fd, command, lock);
        assert_eq!((ret, errno), (-1, want), "{name}");
    }
    // A negative length that stays at or above zero is legal: it locks the bytes before start.
    let (ret, errno, _) = lock_through_guest(&f, rw, F_SETLK_GUEST, (F_WRLCK_GUEST, 0, 4, -4));
    assert_eq!((ret, errno), (0, 0), "bytes 0..4, spelled backwards");

    // Refused by name rather than granted unmeasured: a lock relative to the offset, and a lock
    // on a descriptor that is not a regular file.
    let flock = f.guest.data + 0x300;
    let mut relative = [0u8; 32];
    relative[2..4].copy_from_slice(&1i16.to_le_bytes());
    relative[16..24].copy_from_slice(&1i64.to_le_bytes());
    f.guest.write_bytes(flock, &relative);
    let error = refusal_of(&f, "fcntl", |asm| {
        asm.mov(0, rw as u64);
        asm.mov(1, F_SETLK_GUEST);
        asm.mov(2, flock as u64);
    });
    assert!(error.to_string().contains("SEEK_CUR"), "{error}");
    let (read_fd, _write_fd) = pipe_through_guest(&f);
    f.guest.write_bytes(flock, &[0u8; 32]);
    let error = refusal_of(&f, "fcntl", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, F_SETLK_GUEST);
        asm.mov(2, flock as u64);
    });
    assert!(error.to_string().contains("not a regular file"), "{error}");
}

/// One call through real guest code: `(x0 as i64, errno)`, with errno cleared first so a
/// success cannot inherit a stale value.
fn call_with_errno(f: &Fixture, symbol: &str, args: &[u64]) -> (i64, u64) {
    let out = f.guest.data + 0x400;
    f.guest.write_u64(out + 8, 0);
    let entry = program(f, |asm| {
        asm.bl(f.thunk("__errno"));
        asm.mov(9, 0);
        asm.push(str_w(9, 0, 0));
        for (register, value) in args.iter().enumerate() {
            asm.mov(register as u32, *value);
        }
        asm.bl(f.thunk(symbol));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
    });
    assert!(matches!(run_program(f, entry).expect("completes"), ExitReason::Returned { .. }));
    (f.guest.read_u64(out) as i64, f.guest.read_u64(out + 8))
}

/// `EPOLLIN`, `EPOLLOUT`, `EPOLLHUP` and `EPOLLET`, as the guest spells them.
const EPOLLIN_GUEST: u32 = 0x001;
const EPOLLOUT_GUEST: u32 = 0x004;
const EPOLLHUP_GUEST: u32 = 0x010;
const EPOLLET_GUEST: u32 = 0x8000_0000;

/// **The engine's transport's epoll, through real guest code: level-triggered events carrying the
/// guest's own data back, the kernel's refusals errno by errno, and close leaving the list.**
///
/// Asserted on the 16-byte arm64 `epoll_event` the guest reads, not on the count: a count cannot
/// see a `data` word written at x86-64's offset 4 instead of 8, and that is the difference the
/// layout constant exists for.
#[test]
fn epoll_reports_level_triggered_events_with_the_guests_data_and_the_kernels_errors() {
    let _guard = serialized();
    let (f, scratch) = rooted("epoll");
    let event = f.guest.data + 0x300;
    let events = f.guest.data + 0x500;
    let set_event = |bits: u32, data: u64| {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&bits.to_le_bytes());
        bytes[4..8].copy_from_slice(&[0xEE; 4]);
        bytes[8..16].copy_from_slice(&data.to_le_bytes());
        f.guest.write_bytes(event, &bytes);
    };
    let ctl = |epfd: i32, op: u64, fd: i32| {
        call_with_errno(&f, "epoll_ctl", &[epfd as u64, op, fd as u64, event as u64])
    };
    let wait = |epfd: i32, max: u64, timeout: i64| {
        call_with_errno(&f, "epoll_wait", &[epfd as u64, events as u64, max, timeout as u64])
    };
    let reported = |index: usize| {
        let bytes = read_guest(&f, events + index * 16, 16);
        (
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        )
    };

    assert_eq!(call_with_errno(&f, "epoll_create1", &[1]), (-1, EINVAL_NET), "an unknown flag");
    let (epfd, errno) = call_with_errno(&f, "epoll_create1", &[0]);
    assert!(epfd >= 3 && errno == 0, "{epfd} {errno}");
    let epfd = epfd as i32;
    let (read_fd, write_fd) = pipe_through_guest(&f);

    set_event(EPOLLIN_GUEST, 0x1122_3344_5566_7788);
    assert_eq!(ctl(epfd, 1, read_fd), (0, 0), "ADD the read end for input");
    assert_eq!(wait(epfd, 8, 0), (0, 0), "an empty pipe is not readable");

    f.guest.write_bytes(f.guest.data + 0x200, b"x");
    assert_eq!(
        call_with_errno(&f, "write", &[write_fd as u64, (f.guest.data + 0x200) as u64, 1]),
        (1, 0)
    );
    assert_eq!(wait(epfd, 8, 0), (1, 0), "one byte makes it readable");
    assert_eq!(reported(0), (EPOLLIN_GUEST, 0x1122_3344_5566_7788), "events at 0, data at 8");
    assert_eq!(wait(epfd, 8, 0), (1, 0), "level-triggered: still readable, reported again");

    set_event(EPOLLOUT_GUEST, 2);
    assert_eq!(ctl(epfd, 1, write_fd), (0, 0), "ADD the write end for output");
    assert_eq!(wait(epfd, 8, 0), (2, 0), "both ends now");
    assert_eq!(wait(epfd, 1, 0), (1, 0), "but never more than maxevents");

    // The kernel's answers, each by number.
    set_event(EPOLLIN_GUEST, 3);
    assert_eq!(ctl(epfd, 1, read_fd), (-1, 17), "ADD twice is EEXIST");
    assert_eq!(ctl(epfd, 3, 99), (-1, EBADF_NET), "a descriptor that is not open is EBADF");
    assert_eq!(ctl(epfd, 1, epfd), (-1, EINVAL_NET), "an instance cannot watch itself");
    assert_eq!(ctl(epfd, 9, read_fd), (-1, EINVAL_NET), "an unknown op");
    let file = open_through_guest(&f, "/plain", O_RDWR | O_CREAT);
    assert_eq!(ctl(epfd, 1, file), (-1, 1), "a regular file is EPERM");
    assert_eq!(ctl(file, 1, read_fd), (-1, EINVAL_NET), "epfd that is not an epoll is EINVAL");
    assert_eq!(wait(epfd, 0, 0), (-1, EINVAL_NET), "maxevents 0");
    set_event(EPOLLIN_GUEST, 4);
    let (_, missing) = ctl(epfd, 3, file);
    assert_eq!(missing, 1, "MOD of a file is refused as EPERM before the list is consulted");
    assert_eq!(ctl(epfd, 2, file), (-1, 1), "and so is DEL: `file_can_poll` comes first");

    // MOD replaces the data, DEL removes, and a refused bit refuses by name.
    set_event(EPOLLIN_GUEST, 0xABCD);
    assert_eq!(ctl(epfd, 3, read_fd), (0, 0));
    assert_eq!(ctl(epfd, 2, write_fd), (0, 0), "DEL");
    assert_eq!(ctl(epfd, 2, write_fd), (-1, 2), "DEL twice is ENOENT");
    assert_eq!(wait(epfd, 8, 0), (1, 0));
    assert_eq!(reported(0), (EPOLLIN_GUEST, 0xABCD), "the modified data");
    set_event(EPOLLIN_GUEST | EPOLLET_GUEST, 5);
    let error = refusal_of(&f, "epoll_ctl", |asm| {
        asm.mov(0, epfd as u64);
        asm.mov(1, 3);
        asm.mov(2, read_fd as u64);
        asm.mov(3, event as u64);
    });
    assert!(error.to_string().contains("EPOLLET"), "{error}");

    // Closing the write end hangs the read end up once it drains; HUP is reported unasked.
    assert_eq!(call_with_errno(&f, "close", &[write_fd as u64]), (0, 0));
    let buf = (f.guest.data + 0x200) as u64;
    assert_eq!(call_with_errno(&f, "read", &[read_fd as u64, buf, 8]), (1, 0), "drain the byte");
    assert_eq!(wait(epfd, 8, 0), (1, 0));
    assert_eq!(reported(0).0 & EPOLLHUP_GUEST, EPOLLHUP_GUEST, "a hangup nobody asked about");

    // And closing a watched descriptor takes it out of the list: a number reused by `open` must
    // not inherit the watch.
    assert_eq!(call_with_errno(&f, "close", &[read_fd as u64]), (0, 0));
    assert_eq!(wait(epfd, 8, 0), (0, 0), "nothing left to report");
    std::fs::write(scratch.path("reuse"), b"").expect("a host file");
    let reused = open_through_guest(&f, "/reuse", O_RDONLY);
    assert_eq!(reused, read_fd, "the lowest free number came back");
    assert_eq!(wait(epfd, 8, 0), (0, 0), "and it is not watched");
}

/// **`epoll_wait(-1)` waits until something is ready -- here, a write from another thread -- and
/// refuses when nothing in its list ever could be.**
///
/// The first half is the engine's I/O thread idling, which `poll`'s rules would have refused;
/// the second is the case that makes an unbounded wait unrecoverable, and is still refused.
#[test]
fn an_indefinite_epoll_wait_is_woken_by_a_write_and_refused_over_nothing() {
    let _guard = serialized();
    let (f, _scratch) = rooted("epoll-wait");
    let (epfd, _) = call_with_errno(&f, "epoll_create1", &[0]);
    let epfd = epfd as i32;
    let events = f.guest.data + 0x500;
    let error = refusal_of(&f, "epoll_wait", |asm| {
        asm.mov(0, epfd as u64);
        asm.mov(1, events as u64);
        asm.mov(2, 4);
        asm.mov(3, u64::MAX);
    });
    assert!(error.to_string().contains("nothing can become ready"), "{error}");

    let (read_fd, write_fd) = pipe_through_guest(&f);
    let event = f.guest.data + 0x300;
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&EPOLLIN_GUEST.to_le_bytes());
    bytes[8..16].copy_from_slice(&77u64.to_le_bytes());
    f.guest.write_bytes(event, &bytes);
    assert_eq!(
        call_with_errno(&f, "epoll_ctl", &[epfd as u64, 1, read_fd as u64, event as u64]),
        (0, 0)
    );
    let bionic = Arc::clone(&f.bionic);
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        bionic.filesystem().expect("a filesystem").write(write_fd, b"!").expect("a write")
    });
    let started = std::time::Instant::now();
    let (ready, errno) =
        call_with_errno(&f, "epoll_wait", &[epfd as u64, events as u64, 4, u64::MAX]);
    let waited = started.elapsed();
    assert_eq!(writer.join().expect("the writer"), 1);
    assert_eq!((ready, errno), (1, 0), "woken by the write, after {waited:?}");
    assert!(waited >= std::time::Duration::from_millis(100), "it did wait: {waited:?}");
    assert_eq!(f.guest.read_u64(events + 8), 77, "and handed back the guest's data");
}

/// Write an aarch64 `struct itimerspec`: interval then value, each `(tv_sec, tv_nsec)`.
fn write_itimerspec(f: &Fixture, at: omni_cpu::GuestAddr, interval: (i64, i64), value: (i64, i64)) {
    let mut bytes = [0u8; 32];
    for (offset, part) in [(0, interval.0), (8, interval.1), (16, value.0), (24, value.1)] {
        bytes[offset..offset + 8].copy_from_slice(&part.to_le_bytes());
    }
    f.guest.write_bytes(at, &bytes);
}

/// **A timerfd in an epoll set wakes an indefinite `epoll_wait` at its deadline** -- which
/// nothing announces, so this is the test that the wait caps itself there -- and is read as one
/// expiration, relative and absolute, on the clock the guest's own `clock_gettime` reads.
#[test]
fn a_timerfd_wakes_epoll_at_its_deadline_on_the_guests_own_monotonic_clock() {
    let _guard = serialized();
    let (f, _scratch) = rooted("timerfd");
    let spec = f.guest.data + 0x300;
    let events = f.guest.data + 0x500;
    let (timer, errno) = call_with_errno(&f, "timerfd_create", &[1, 0o4000]);
    assert!(timer >= 3 && errno == 0, "{timer} {errno}");
    let (epfd, _) = call_with_errno(&f, "epoll_create1", &[0]);
    let event = f.guest.data + 0x340;
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&EPOLLIN_GUEST.to_le_bytes());
    bytes[8..16].copy_from_slice(&0x7117u64.to_le_bytes());
    f.guest.write_bytes(event, &bytes);
    assert_eq!(
        call_with_errno(&f, "epoll_ctl", &[epfd as u64, 1, timer as u64, event as u64]),
        (0, 0)
    );
    let buf = (f.guest.data + 0x200) as u64;
    assert_eq!(call_with_errno(&f, "read", &[timer as u64, buf, 8]), (-1, EAGAIN_GUEST), "disarmed");

    // Relative: 80 ms from now, and an epoll_wait(-1) that only a capped wait can end.
    write_itimerspec(&f, spec, (0, 0), (0, 80_000_000));
    assert_eq!(call_with_errno(&f, "timerfd_settime", &[timer as u64, 0, spec as u64, 0]), (0, 0));
    let started = std::time::Instant::now();
    let (ready, _) = call_with_errno(&f, "epoll_wait", &[epfd as u64, events as u64, 4, u64::MAX]);
    let waited = started.elapsed();
    assert_eq!(ready, 1, "the timer fired");
    assert_eq!(f.guest.read_u64(events + 8), 0x7117, "and handed back the guest's data");
    assert!(waited >= std::time::Duration::from_millis(70), "not early: {waited:?}");
    assert!(waited < std::time::Duration::from_millis(1500), "not a slice late: {waited:?}");
    assert_eq!(call_with_errno(&f, "read", &[timer as u64, buf, 8]), (8, 0));
    assert_eq!(f.guest.read_u64(buf as omni_cpu::GuestAddr), 1, "one expiration");
    assert_eq!(call_with_errno(&f, "read", &[timer as u64, buf, 8]), (-1, EAGAIN_GUEST), "consumed");

    // Absolute, on the guest's own CLOCK_MONOTONIC as the guest reads it.
    let now = f.guest.data + 0x380;
    assert_eq!(call_with_errno(&f, "clock_gettime", &[1, now as u64]), (0, 0));
    let (seconds, nanos) = (f.guest.read_u64(now) as i64, f.guest.read_u64(now + 8) as i64);
    let deadline = (seconds * 1_000_000_000 + nanos) + 60_000_000;
    write_itimerspec(&f, spec, (0, 0), (deadline / 1_000_000_000, deadline % 1_000_000_000));
    assert_eq!(call_with_errno(&f, "timerfd_settime", &[timer as u64, 1, spec as u64, 0]), (0, 0));
    let started = std::time::Instant::now();
    let (ready, _) = call_with_errno(&f, "epoll_wait", &[epfd as u64, events as u64, 4, u64::MAX]);
    assert_eq!(ready, 1);
    assert!(started.elapsed() < std::time::Duration::from_millis(1500), "{:?}", started.elapsed());
    assert_eq!(call_with_errno(&f, "read", &[timer as u64, buf, 8]), (8, 0));
    // **An absolute deadline already in the past is expired at once** -- the case that tells
    // absolute from relative: read as a delay, it would land as far in the future as the guest's
    // clock is past its epoch. At least 70 ms have passed on that clock by here.
    assert_eq!(call_with_errno(&f, "clock_gettime", &[1, now as u64]), (0, 0));
    let past = f.guest.read_u64(now) as i64 * 1_000_000_000 + f.guest.read_u64(now + 8) as i64
        - 20_000_000;
    write_itimerspec(&f, spec, (0, 0), (past / 1_000_000_000, past % 1_000_000_000));
    assert_eq!(call_with_errno(&f, "timerfd_settime", &[timer as u64, 1, spec as u64, 0]), (0, 0));
    assert_eq!(
        call_with_errno(&f, "epoll_wait", &[epfd as u64, events as u64, 4, 0]),
        (1, 0),
        "a deadline in the past is already expired"
    );

    // The kernel's refusals.
    write_itimerspec(&f, spec, (0, 0), (0, 1_000_000_000));
    assert_eq!(
        call_with_errno(&f, "timerfd_settime", &[timer as u64, 0, spec as u64, 0]),
        (-1, EINVAL_NET),
        "tv_nsec of a whole second"
    );
    let (read_fd, _write_fd) = pipe_through_guest(&f);
    write_itimerspec(&f, spec, (0, 0), (0, 1));
    assert_eq!(
        call_with_errno(&f, "timerfd_settime", &[read_fd as u64, 0, spec as u64, 0]),
        (-1, EINVAL_NET),
        "not a timerfd"
    );
    let error = refusal_of(&f, "timerfd_create", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
    });
    assert!(error.to_string().contains("CLOCK_MONOTONIC"), "{error}");
}

/// **`fsync` syncs a regular file and answers the kernel's `EINVAL` for a pipe.**
#[test]
fn fsync_syncs_a_file_and_is_einval_for_a_pipe() {
    let _guard = serialized();
    let (f, _scratch) = rooted("fsync");
    let file = open_through_guest(&f, "/db", O_RDWR | O_CREAT);
    assert_eq!(call_with_errno(&f, "fsync", &[file as u64]), (0, 0));
    let (read_fd, _write_fd) = pipe_through_guest(&f);
    assert_eq!(call_with_errno(&f, "fsync", &[read_fd as u64]), (-1, EINVAL_NET));
    assert_eq!(call_with_errno(&f, "fsync", &[77]), (-1, EBADF_NET));
}

/// **A write to a pipe whose reader has closed is `EPIPE`**, not a short write and not a refusal.
///
/// On a device it is also `SIGPIPE`; there is no signal delivery here, so the errno is the whole
/// of what the guest gets, which is what a caller that has set `SIG_IGN` sees on a device.
#[test]
fn a_write_to_a_pipe_whose_reader_has_closed_is_epipe() {
    let _guard = serialized();
    let (f, _scratch) = rooted("pipe-epipe");
    let (read_fd, write_fd) = pipe_through_guest(&f);
    let closed = value_of(&f, "close", |asm| {
        asm.mov(0, read_fd as u64);
    });
    assert_eq!(closed as i64, 0);

    let source = f.cstring(f.guest.data + 0x100, b"x");
    let out = f.guest.data + 0x400;
    let entry = program(&f, |asm| {
        asm.mov(0, write_fd as u64);
        asm.mov(1, source as u64);
        asm.mov(2, 1);
        asm.bl(f.thunk("write"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out) as i64, -1, "a broken pipe is -1, not a short write");
    assert_eq!(f.guest.read_u64(out + 8), EPIPE_GUEST, "EPIPE is Linux's 32");
}

/// **A blocking read waits**, and the assertion is the value it returns rather than how long it
/// took.
///
/// A host thread writes one byte after a delay that is long against everything the guest does to
/// reach the call. If the blocking wait were missing, the read would report `-1`/`EAGAIN`
/// immediately and this fails deterministically; if it is there, the read returns the byte. The
/// test can therefore **fail only when the wait is absent** — it cannot fail because the machine
/// was slow, which `VERIFICATION.md` entry 6 is about. Under a slow enough machine it stops
/// *exercising* the wait and still passes, which is the safe direction for a bound of this shape.
#[test]
fn a_blocking_read_on_an_empty_pipe_waits_for_a_writer() {
    let _guard = serialized();
    let (f, _scratch) = rooted("pipe-blocking");
    let (read_fd, write_fd) = pipe_through_guest(&f);

    // The read end is left **blocking** -- the default -- and the write end is the host's here,
    // because a second guest thread would be testing the thread layer rather than the wait.
    let bionic = Arc::clone(&f.bionic);
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let fs = bionic.filesystem().expect("the instance has a root");
        fs.write(write_fd, b"Q").expect("one byte into the pipe")
    });

    let destination = f.guest.data + 0x200;
    f.guest.write_u64(destination, 0x5A5A_5A5A_5A5A_5A5A);
    let read = value_of(&f, "read", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, destination as u64);
        asm.mov(2, 8);
    });
    assert_eq!(writer.join().expect("the writer thread"), 1);
    assert_eq!(
        read as i64,
        1,
        "a blocking read must wait for the writer; -1 here means it reported EAGAIN to a \
         descriptor that never asked to be non-blocking"
    );
    assert_eq!(read_guest(&f, destination, 1), b"Q");
}

/// **A pipe past the descriptor ceiling is all-or-nothing, and a null `pipefd` is a refusal.**
#[test]
fn pipe_refuses_a_null_pointer_and_leaves_nothing_open_when_it_cannot_fit() {
    let _guard = serialized();
    let (f, _scratch) = rooted("pipe-refusals");
    let error = refusal_of(&f, "pipe", |asm| {
        asm.mov(0, 0);
    });
    assert_eq!(error.symbol(), Some("pipe"));
    assert!(error.to_string().contains("null"), "{error}");

    // Fill the table to one short of the ceiling, then ask for two.
    let fs = f.bionic.filesystem().expect("a root");
    let ceiling = omni_platform::fs::MAX_OPEN_FILES;
    while fs.open_count() < ceiling - 1 {
        fs.open(b"/dev/null", omni_platform::fs::OpenFlags {
            read: true,
            ..omni_platform::fs::OpenFlags::default()
        })
        .expect("a device descriptor");
    }
    let before = fs.open_count();
    let at = f.guest.data + 0x40;
    f.guest.write_u64(at, 0x5A5A_5A5A_5A5A_5A5A);
    let out = f.guest.data + 0x400;
    let entry = program(&f, |asm| {
        asm.mov(0, at as u64);
        asm.bl(f.thunk("pipe"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out) as i64, -1, "one free slot is not two");
    assert_eq!(f.guest.read_u64(out + 8), 24, "EMFILE is Linux's 24");
    assert_eq!(fs.open_count(), before, "a refused pipe leaks no descriptor");
    assert_eq!(
        f.guest.read_u64(at),
        0x5A5A_5A5A_5A5A_5A5A,
        "and it wrote nothing into the guest's `int[2]`"
    );
}

// =================================================================== phase 3e: the last four
//
// `time`, `clock`, `mallinfo`, `longjmp`. The other two of the six — `__gcov_dump` and
// `__gcov_flush` — are not bound at all: they are declared **absent**, and
// `crates/omni-android/tests/libroblox.rs` asserts that against the real library.

/// **`time` is the same wall clock `gettimeofday` reads**, and it writes its argument.
///
/// A guest that called both and compared them would otherwise be able to see two clocks where a
/// device has one. The `tloc` write is checked against a sentinel, because a handler that
/// returned the right value and wrote nothing is the failure this test exists for.
#[test]
fn time_is_the_same_wall_clock_gettimeofday_reads_and_writes_its_argument() {
    let _guard = serialized();
    let f = fixture();
    let tloc = f.guest.data + 0x100;
    f.guest.write_u64(tloc, 0xDEAD_BEEF_DEAD_BEEF);

    let returned = value_of(&f, "time", |asm| {
        asm.mov(0, tloc as u64);
    }) as i64;
    // 2020-01-01 .. 2100-01-01, the same bound `omni_platform::clock`'s own test uses: wide
    // enough that only a wrong unit or a wrong epoch can fail it.
    assert!((1_577_836_800..4_102_444_800).contains(&returned), "time() returned {returned}");
    assert_eq!(f.guest.read_u64(tloc) as i64, returned, "tloc receives what was returned");

    // The same second, through a different symbol.
    let tv = f.guest.data + 0x200;
    let ok = value_of(&f, "gettimeofday", |asm| {
        asm.mov(0, tv as u64);
        asm.mov(1, 0);
    });
    assert_eq!(ok, 0);
    let from_gettimeofday = f.guest.read_u64(tv) as i64;
    assert!(
        (from_gettimeofday - returned).abs() <= 2,
        "time() said {returned} and gettimeofday() said {from_gettimeofday}: two clocks where a \
         device has one"
    );

    // A null `tloc` is the ordinary form and must not fault.
    let again = value_of(&f, "time", |asm| {
        asm.mov(0, 0);
    }) as i64;
    assert!(again >= returned, "the wall clock does not run backwards over one test");
}

/// A `tloc` this guest cannot write is a typed refusal, and the call does **not** report a time.
#[test]
fn time_with_an_unwritable_tloc_refuses_rather_than_reporting_a_time_it_did_not_store() {
    let _guard = serialized();
    let f = fixture();
    let error = refusal_of(&f, "time", |asm| {
        asm.mov(0, f.guest.readonly as u64);
    });
    assert_eq!(error.symbol(), Some("time"), "{error:?}");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
}

/// **`clock` is process CPU time in microseconds, and it advances with real guest work.**
///
/// Two assertions here, one per way this symbol can be plausibly wrong *at the guest boundary*.
/// It must advance under work the translator really executed, which is what a constant or a
/// zero fails. And it must be small enough that it cannot be a wall-clock timestamp scaled to
/// microseconds — this process has run for seconds, not for fifty-six years — which is what a
/// `realtime_now` mistake fails.
///
/// **The wall-versus-CPU discrimination is not made here, and that is deliberate.** It belongs to
/// the primitive rather than to the binding, it needs several threads burning one interval of
/// wall time to be made without flaking, and it is made once in
/// `omni_platform::process`'s own suite — Global Constraint 14: a measured quantity appears once.
/// A first attempt at it *here* asserted "a sleep charges no CPU", which is true of a thread
/// clock and false of the process clock this reports; it failed in the whole-workspace run, where
/// other tests were executing during the sleep (MEASURED: 93.75 ms charged across a 50 ms sleep).
#[test]
fn clock_is_process_cpu_time_in_microseconds_rather_than_wall_time() {
    let _guard = serialized();
    let f = fixture();
    let first = value_of(&f, "clock", |_asm| {}) as i64;
    assert!(first >= 0, "clock() returned {first}");
    assert!(
        first < 60 * 60 * 24 * 1_000_000,
        "clock() returned {first} microseconds, which is more than a day of CPU: that is a wall \
         clock, not a process clock"
    );

    // Real work, in the guest: a loop of a few million instructions through the translator.
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, 3_000_000);
    asm.push(subs_imm(0, 0, 1));
    asm.push(b_cond(1, -1)); // b.ne back one instruction
    asm.push(ret(21));
    f.guest.load(asm.words());

    // **The work is repeated until the clock moves, under a wall-clock deadline**, and the
    // assertion is that it moved rather than that one pass was enough.
    //
    // A single pass is a **flake**, and it was seen as one: MEASURED in a whole-workspace run,
    // `1843750 -> 1843750`. Windows charges process CPU in scheduler ticks of 15.625 ms — that
    // failing figure is exactly 118 of them — and three million guest instructions through the
    // translator do not reliably cross a tick boundary. So the old assertion was really "this
    // pass happened to straddle a tick", which is `VERIFICATION.md` entry 6's shape: a
    // timing-dependent test that must be made structural rather than given a bigger number.
    //
    // A deadline rather than a fixed repeat count, because the quantum is the host's and a count
    // chosen against this machine is the same assumption one level up.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut passes = 0u32;
    let after_work = loop {
        let mut cpu = f.guest.thread(&f.boundary);
        assert!(matches!(f.run(&mut cpu, entry).expect("completes"), ExitReason::Returned { .. }));
        passes += 1;
        let now = value_of(&f, "clock", |_asm| {}) as i64;
        if now > first {
            break now;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{passes} passes of three million guest instructions moved the process CPU clock not \
             at all in 20 seconds of wall time: {first} -> {now}. That is not the host's \
             15.625 ms tick quantum; it is a clock that does not advance"
        );
    };
    assert!(after_work > first, "{first} -> {after_work} over {passes} passes");

    // It never goes backwards across two calls through the boundary.
    let last = value_of(&f, "clock", |_asm| {}) as i64;
    assert!(last >= after_work, "process CPU time went backwards: {after_work} -> {last}");
}

/// **`clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` answers, and the thread clock still refuses.**
///
/// The correction to D22, asserted in both directions. Answering the process clock is now
/// required — refusing it would mean this layer gave two different answers to one question, since
/// `clock()` reports the figure — and answering the *thread* clock is still forbidden, because
/// the process figure would report every thread as having consumed the whole program's CPU.
#[test]
fn the_process_cpu_clock_is_answered_and_the_thread_cpu_clock_is_still_refused() {
    let _guard = serialized();
    let f = fixture();
    let ts = f.guest.data + 0x100;
    f.guest.write_u64(ts, 0xDEAD_BEEF);
    f.guest.write_u64(ts + 8, 0xDEAD_BEEF);

    let ok = value_of(&f, "clock_gettime", |asm| {
        asm.mov(0, CLOCK_PROCESS_CPUTIME_ID);
        asm.mov(1, ts as u64);
    });
    assert_eq!(ok, 0);
    let seconds = f.guest.read_u64(ts) as i64;
    let nanos = f.guest.read_u64(ts + 8) as i64;
    assert!((0..86_400).contains(&seconds), "{seconds} seconds of process CPU time");
    assert!((0..1_000_000_000).contains(&nanos), "tv_nsec must be normalised: {nanos}");

    // **It must agree with `clock()`**, which is the whole reason the refusal had to go — and the
    // tolerance has to be tight enough that a wrong *unit* cannot pass. `CLOCKS_PER_SEC` is a
    // million, so a `clock()` reporting milliseconds is a thousand-fold error that still rises
    // and still looks like a time; 50 ms of slack separates the two calls' own drift — the
    // accounting quantum is ~15.6 ms — from that.
    //
    // The CPU is **burned here rather than assumed**. Run alone rather than in a full suite this
    // binary has used about 60 ms by the time it reaches this point (MEASURED: 62,500
    // microseconds), and a unit check needs the figure to be large against its own tolerance.
    // 250 ms of real arithmetic makes the precondition hold whatever else ran first, which is
    // what stops this test depending on the order libtest happens to pick.
    let burn = std::time::Instant::now();
    let mut acc: u64 = 1;
    while burn.elapsed() < std::time::Duration::from_millis(250) {
        acc = acc.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    }
    assert_ne!(acc, 0, "the busy loop must not be optimised away");

    let ok = value_of(&f, "clock_gettime", |asm| {
        asm.mov(0, CLOCK_PROCESS_CPUTIME_ID);
        asm.mov(1, ts as u64);
    });
    assert_eq!(ok, 0);
    let seconds = f.guest.read_u64(ts) as i64;
    let nanos = f.guest.read_u64(ts + 8) as i64;
    let from_clock = value_of(&f, "clock", |_asm| {}) as i64;
    let from_clock_gettime = seconds * 1_000_000 + nanos / 1_000;
    assert!(
        from_clock_gettime > 200_000,
        "this process has used only {from_clock_gettime} microseconds of CPU after a 250 ms busy \
         loop, which is too little for the unit check below to mean anything"
    );
    assert!(
        (from_clock - from_clock_gettime).abs() < 50_000,
        "clock() said {from_clock} microseconds and clock_gettime said {from_clock_gettime}: one \
         question, two answers"
    );

    // And the thread clock, which is a different fact this layer does not have.
    let error = refusal_of(&f, "clock_gettime", |asm| {
        asm.mov(0, CLOCK_THREAD_CPUTIME_ID);
        asm.mov(1, ts as u64);
    });
    assert_eq!(error.symbol(), Some("clock_gettime"));
    let text = error.to_string();
    assert!(text.contains("CLOCK_THREAD_CPUTIME_ID"), "{text}");
    assert!(text.contains("GetThreadTimes"), "it must name the missing primitive: {text}");
}

/// **`mallinfo` reports the empty libc heap that is genuinely there.**
///
/// It refused until M6, and the refusal's own text conceded that ten zeroed fields would be
/// *arithmetically true* of a heap nothing has allocated from. `libroblox.so` imports no allocator
/// (D17), so nothing ever has. `nativePostClientSettingsLoadedInitialization3` stops on this call,
/// and a true answer that unblocks it is not what the no-stubs rule is about.
///
/// The one reachable import that returns through `X8`, so this is also the only test that
/// exercises the indirect result register at all.
#[test]
fn mallinfo_reports_the_empty_libc_heap_through_the_indirect_result_register() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x500;
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    // `X8` is the indirect result register: the caller allocates the 80 bytes and passes their
    // address there. Nothing else in the reachable set does this.
    asm.mov(8, out as u64);
    asm.bl(f.thunk("mallinfo"));
    asm.push(ret(21));
    f.guest.load(asm.words());
    // A non-zero pattern across the whole struct first, so that "all ten fields are zero" is a
    // thing this call did rather than a thing the buffer already was. Without it the assertion
    // below passes just as well against a handler that writes nothing at all.
    for field in 0..10usize {
        f.guest.write_u64(out + field * 8, 0xDEAD_0000_0000_0000 | field as u64);
    }
    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("mallinfo returns");
    for field in 0..10usize {
        assert_eq!(
            f.guest.read_u64(out + field * 8),
            0,
            "field {field} of the ten: the libc heap this runtime has is empty, and every one of \
             its counts says so",
        );
    }

    // **A hostile `X8` is the boundary's refusal, not a host fault.** The eighty bytes go through
    // `GuestMem`, so an unmapped result buffer is refused by name with the length and the access
    // in it — the property that makes writing through a guest-supplied pointer safe at all, and
    // the half of this symbol a returning handler could most easily lose.
    let hostile = f.guest.next_entry();
    let mut asm = Asm::at(hostile);
    asm.push(mov_reg(21, 30));
    asm.mov(8, 0);
    asm.bl(f.thunk("mallinfo"));
    asm.push(ret(21));
    f.guest.load(asm.words());
    let mut cpu = f.guest.thread(&f.boundary);
    let error = match f.run(&mut cpu, hostile) {
        Err(error) => error,
        Ok(exit) => panic!("a null X8 completed with {exit:?}"),
    };
    assert_eq!(error.symbol(), Some("mallinfo"), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("80"), "the length it would have written: {text}");
    assert!(text.contains("writing"), "the access it was refused for: {text}");
}

/// **`longjmp` refuses**, and the refusal names what restoring a `jmp_buf` would take.
///
/// It also states the `val` the matching `setjmp` would have seen, including C's rule that a zero
/// is delivered as one — the one part of this function's contract that can be honoured without
/// restoring anything, and worth carrying because it is the value a reader will be looking for.
#[test]
fn longjmp_refuses_and_names_what_restoring_a_jmp_buf_would_take() {
    let _guard = serialized();
    let f = fixture();
    let env = f.guest.data + 0x100;
    let error = refusal_of(&f, "longjmp", |asm| {
        asm.mov(0, env as u64);
        asm.mov(1, 0);
    });
    assert_eq!(error.symbol(), Some("longjmp"), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("X19-X28"), "the registers it would restore: {text}");
    assert!(text.contains("delivering 1"), "C's zero-becomes-one rule: {text}");
    assert!(text.contains("setjmp"), "and that nothing here can have filled the jmp_buf: {text}");
}

/// **The final split of the 188, asserted by calling every symbol rather than by counting.**
///
/// M3 task 3 ends here, so the number that goes into the record is this one — and this project's
/// most repeated mistake is a *total* that stays right while its membership drifts (D21's data
/// list wrong by two in each direction, D24's arena test restating its own definition). So the
/// split is derived by **calling** each symbol and seeing what it does, not by reading a table:
///
/// * **8 refuse by name, whatever they are passed.** Each is called with zeroed arguments, the
///   refusal has to be `AbiError::Refused` — not `Unbound`, which would mean nothing implements
///   it, and not a plausible value — and it must **still** refuse when it is given something
///   plausible, which is the half that keeps a conditional answer out of this list.
/// * **3 report a guest termination**, which is a third outcome rather than a refusal: `abort`
///   and `__stack_chk_fail` report `GuestAborted` and `_exit` reports `GuestExited`, because this
///   process hosts several guest instances and a host `abort()` would take all of them (D22).
/// * **157 answer**, which is what is left of the 168 bound.
/// * **18** are data objects and **2** are deliberately absent.
///
/// 157 + 8 + 3 + 18 + 2 = 188.
///
/// # M6's network phase moved FOUR out of the refusal list, and the fourth was already wrong
///
/// `socket`, `getaddrinfo` and `freeaddrinfo` were phase 3d's network refusals and D30 withdrew
/// the constraint that produced them; each answers now, once an embedding has said which network
/// this instance may reach. `eventfd` is the fourth and it had **already** started answering in
/// M6 — it stayed in this list because it happens to refuse in the bare fixture, which is exactly
/// the "membership adjusted to keep a total right" failure this test's own heading complains
/// about, found while removing the other three.
///
/// Their new contract is asserted by *calling* them, in
/// `the_network_group_answers_once_the_embedding_has_said_what_it_may_reach`, which is the same
/// treatment `mallinfo` got when it left this list: a refusal that has been closed belongs in
/// neither place, and the answer that replaced it has to be pinned somewhere real.
///
/// # It was 143 + 22 until M3's gate, and SIX symbols changed category
///
/// **The category is "refuses whatever it is passed", and the comment below has said so since
/// phase 3a — while the list did not obey it.** Task 4 made six symbols answer *some* arguments,
/// and leaving them in a list whose own rule excludes them would have been this project's most
/// repeated mistake once more: a total that stays right because the membership was adjusted to
/// keep it right.
///
/// | symbol | what it answers now |
/// |---|---|
/// | `sysconf` | the page size and the processor count (D27) |
/// | `sysinfo` | the guest's own world, once the embedding states its memory budget |
/// | `prctl` | `PR_SET_VMA`, and `EINVAL` for the two transparent-huge-page options |
/// | `syscall` | `gettid`, and `rt_sigprocmask` — which the engine uses as a pointer probe |
/// | `dlsym`, `dlclose` | a handle this layer issued |
///
/// `dlopen`, `fprintf` and `vfprintf` left the list outright: they answer for everything.
#[test]
fn the_final_split_of_the_reachable_set_is_what_the_record_claims() {
    let _guard = serialized();
    let f = fixture();

    // Every symbol that refuses **whatever it is passed**. A conditional refusal is not in this
    // list: `getauxval` refuses only while the `AT_HWCAP` decision is open, `sched_getcpu` only
    // on a target whose process backend is structural, and `mmap` only for a shape it cannot
    // honour — each of those answers on some path, so each is an answer. Six symbols joined them
    // in M3's gate; see this test's documentation.
    let refusals = [
        // phase 1: the printf family that cannot be serviced. **Three left, not five**:
        // `fprintf` and `vfprintf` are bound as of M3's gate -- phase 3b built the stream layer
        // they named as missing, and Task 4 bound the formatting onto it.
        "vasprintf",
        "sscanf",
        "fscanf",
        // phase 2: the one guest-memory call whose guarantee cannot be met
        "mlock",
        // phase 3c: the signal family, which needs delivery that does not exist
        "sigaction",
        "raise",
        "pthread_sigmask",
        // phase 3e: the two that need something no OS would supply
        "longjmp",
    ];
    // **Eight, and the four that left in M6 are the point of this milestone.** `socket`,
    // `getaddrinfo` and `freeaddrinfo` were the last of phase 3d's network refusals; D30 withdrew
    // Global Constraint 8 and each of them answers now. `eventfd` had already started answering in
    // M6 and stayed in this list because it *happens* to refuse in the bare fixture below -- which
    // is the membership-adjusted-to-keep-a-total-right failure this test's own documentation
    // complains about, found while removing the other three.
    //
    // The category is "refuses whatever it is passed". All four of them refuse **only** when the
    // embedding has supplied nothing -- no filesystem root, no network policy -- which is a
    // conditional refusal and belongs on the other side of the line, exactly as `getauxval`'s and
    // `sched_getcpu`'s do. `the_network_group_answers_once_the_embedding_has_said_what_it_may
    // _reach` is where their new contract is asserted, and it is asserted by calling them.
    assert_eq!(refusals.len(), 8);
    for symbol in refusals {
        let error = refusal_of(&f, symbol, |asm| {
            for register in 0..6 {
                asm.mov(register, 0);
            }
        });
        assert!(
            matches!(error, AbiError::Refused { .. }),
            "`{symbol}` must refuse by name, and it produced {error:?}"
        );
        assert_eq!(error.symbol(), Some(symbol), "{error:?}");
        assert_eq!(error.guest_address(), Some(f.thunk(symbol)), "{error:?}");
        // **And it still refuses when it is given something plausible.** Zeroed arguments alone
        // would let a symbol that merely rejects nulls sit in a list whose category is
        // "refuses whatever it is passed" -- which is exactly how six symbols stayed here after
        // they had started answering.
        let with_arguments = refusal_of(&f, symbol, |asm| {
            asm.mov(0, (f.guest.data + 0x600) as u64);
            for register in 1..6 {
                asm.mov(register, 1);
            }
        });
        assert!(
            matches!(with_arguments, AbiError::Refused { .. }),
            "`{symbol}` must refuse whatever it is passed, and with arguments it produced \
             {with_arguments:?}"
        );
    }

    // **The six that changed category in M3's gate**, asserted as answers rather than trusted to
    // the comment above. Each is called with the argument it answers for, and each must come back
    // without a refusal -- which is what stops this list and the one above from drifting apart.
    let _active = f.bionic.activate().expect("a thread block");
    f.bionic.set_memory_budget(1 << 31);
    //
    // `dlsym`, `dlclose`, `fprintf` and `vfprintf` are not here because each needs a value only
    // another call can produce -- a handle, or a registered stream -- and each has a test of its
    // own that asserts it answers: `the_dl_family_answers_for_the_libraries_this_layer_supplies`
    // and `fprintf_and_vfprintf_write_through_a_real_stream`.
    let answers: [Answer<'_>; 5] = [
        ("sysconf", &|asm: &mut Asm| { asm.mov(0, 0x27); }),
        ("sysinfo", &|asm: &mut Asm| { asm.mov(0, (f.guest.data + 0x600) as u64); }),
        ("prctl", &|asm: &mut Asm| {
            asm.mov(0, 42); // PR_GET_THP_DISABLE
            asm.mov(1, 0);
        }),
        ("syscall", &|asm: &mut Asm| { asm.mov(0, 178); }), // gettid
        ("dlopen", &|asm: &mut Asm| {
            asm.mov(0, 0); // the global scope
            asm.mov(1, 2);
        }),
    ];
    for (symbol, setup) in answers {
        let entry = call_one(&f, symbol, |asm| setup(asm));
        let mut cpu = f.guest.thread(&f.boundary);
        match f.boundary.run(&mut cpu, entry, BUDGET) {
            Ok(_) => {}
            Err(AbiError::Refused { symbol: named, .. }) if named == symbol => {
                panic!("`{symbol}` is counted as an answer and refused the argument it answers for")
            }
            Err(other) => panic!("`{symbol}`: {other:?}"),
        }
    }

    // The three terminations, which are reported rather than performed.
    for symbol in ["abort", "__stack_chk_fail", "_exit"] {
        let error = refusal_of(&f, symbol, |asm| {
            asm.mov(0, 0);
        });
        assert!(
            !matches!(error, AbiError::Refused { .. } | AbiError::Unbound { .. }),
            "`{symbol}` reports a guest termination, which is neither a refusal nor a gap: \
             {error:?}"
        );
    }

    // **The split is over the 188 the static closure predicted**, so the thirteen symbols M3's and
    // M4's gates found outside it — and M5 decoded out of the binary — are subtracted rather than
    // folded in: they are not part of
    // what Task 1 predicted, and counting them here would make the total right for the wrong
    // reason -- the exact failure shape this project has made five times.
    let bound = Bionic::bound_symbols().count() - BEYOND_THE_PREDICTION.len();
    assert_eq!(bound, 168);
    let answered = bound - refusals.len() - 3;
    assert_eq!(answered, 157, "157 answer, 8 refuse by name, 3 report a termination");
    assert_eq!(
        answered + refusals.len() + 3 + omni_android::bionic::DATA_OBJECTS.len()
            + omni_android::bionic::ABSENT_SYMBOLS.len(),
        188,
        "every one of the statically-reachable imports is accounted for"
    );
}

// =========================================== M5: the park witness, for §8 row 14's cond-wait
// =========================================== M6: pthread_cond_timedwait's absolute deadline

/// **A deadline already in the past returns `ETIMEDOUT` with the mutex relocked, and returns at
/// once.**
///
/// The whole of what this handler adds over `pthread_cond_wait` is turning an *absolute*
/// `timespec` into a relative wait, and this is the arithmetic's edge. Three things are asserted
/// separately because each fails differently:
///
/// * the **return code** is `ETIMEDOUT` -- a sign error that made the past look like the future
///   would hang here instead, which the harness budget turns into a failure rather than a pass;
/// * the mutex is **held again** afterwards, proved by the guest unlocking it and getting 0. A
///   handler that returned early on a past deadline without going through the two-phase wait
///   would leave it locked, and POSIX requires the relock on every exit path;
/// * the call **did not sleep**, bounded generously at one second, because the same sign error in
///   the other direction produces a correct `ETIMEDOUT` after a wait nobody asked for.
#[test]
fn a_past_absolute_deadline_times_out_at_once_with_the_mutex_relocked() {
    let _guard = serialized();
    let f = fixture();
    let cond = f.guest.data + 0x300;
    let mutex = f.guest.data + 0x400;
    let abstime = f.guest.data + 0x500;
    f.guest.write_bytes(cond, &[0u8; 48]);
    f.guest.write_bytes(mutex, &[0u8; 40]);
    // The epoch: as far in the past as a CLOCK_REALTIME `timespec` goes.
    f.guest.write_u64(abstime, 0);
    f.guest.write_u64(abstime + 8, 0);

    let init_cond = f.thunk("pthread_cond_init");
    let init_mutex = f.thunk("pthread_mutex_init");
    let lock = f.thunk("pthread_mutex_lock");
    let unlock = f.thunk("pthread_mutex_unlock");
    let timedwait = f.thunk("pthread_cond_timedwait");

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(0, cond as u64);
    asm.mov(1, 0);
    asm.bl(init_cond);
    asm.mov(0, mutex as u64);
    asm.mov(1, 0);
    asm.bl(init_mutex);
    asm.mov(0, mutex as u64);
    asm.bl(lock);
    asm.push(str_imm(0, 22, 0));
    asm.mov(0, cond as u64);
    asm.mov(1, mutex as u64);
    asm.mov(2, abstime as u64);
    asm.bl(timedwait);
    asm.push(str_imm(0, 22, 8));
    // If the relock did not happen this unlock answers non-zero, which is the assertion below.
    asm.mov(0, mutex as u64);
    asm.bl(unlock);
    asm.push(str_imm(0, 22, 16));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let started = std::time::Instant::now();
    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");
    let elapsed = started.elapsed();

    assert_eq!(f.guest.read_u64(f.guest.data), 0, "pthread_mutex_lock");
    assert_eq!(
        f.guest.read_u64(f.guest.data + 8) as i32,
        110,
        "ETIMEDOUT, which is what the guest's own `cmp w0, #0x6e` at 0x0285f7f0 tests for"
    );
    assert_eq!(
        f.guest.read_u64(f.guest.data + 16),
        0,
        "the mutex must be held again on the timeout path, so the guest can unlock it"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "a deadline in the past is not a wait: {elapsed:?}"
    );
}

/// **A `timespec` with an out-of-range `tv_nsec` is `EINVAL`, not a wait.**
///
/// bionic validates it, and the alternative here would be `Duration::new` panicking on a
/// nanosecond field above a billion or a negative one silently becoming zero -- a wait for a time
/// the caller never expressed.
#[test]
fn an_out_of_range_tv_nsec_is_einval() {
    let _guard = serialized();
    let f = fixture();
    let cond = f.guest.data + 0x300;
    let mutex = f.guest.data + 0x400;
    let abstime = f.guest.data + 0x500;
    f.guest.write_bytes(cond, &[0u8; 48]);
    f.guest.write_bytes(mutex, &[0u8; 40]);

    for nanos in [1_000_000_000u64, u64::MAX] {
        f.guest.write_u64(abstime, 0);
        f.guest.write_u64(abstime + 8, nanos);
        let returned = value_of(&f, "pthread_cond_timedwait", |asm| {
            asm.mov(0, cond as u64);
            asm.mov(1, mutex as u64);
            asm.mov(2, abstime as u64);
        });
        assert_eq!(returned as i32, 22, "EINVAL for tv_nsec = {nanos}");
    }
}

/// **An absolute deadline further out than the layer's cap is refused by name**, the same cap
/// `nanosleep`, `poll`, `select` and `ALooper_pollOnce` state.
///
/// The number is derived from the clock rather than written down, so the test cannot drift from
/// the cap: `MAX_SLEEP_SECONDS` past now is inside it and twice that is not.
#[test]
fn an_absolute_deadline_past_the_cap_is_refused_and_names_the_cap() {
    let _guard = serialized();
    let f = fixture();
    let cond = f.guest.data + 0x300;
    let mutex = f.guest.data + 0x400;
    let abstime = f.guest.data + 0x500;
    f.guest.write_bytes(cond, &[0u8; 48]);
    f.guest.write_bytes(mutex, &[0u8; 40]);
    let now = omni_platform::clock::realtime_now().as_secs();
    f.guest.write_u64(abstime, now + 2 * omni_android::bionic::MAX_SLEEP_SECONDS);
    f.guest.write_u64(abstime + 8, 0);

    let error = refusal_of(&f, "pthread_cond_timedwait", |asm| {
        asm.mov(0, cond as u64);
        asm.mov(1, mutex as u64);
        asm.mov(2, abstime as u64);
    });
    let text = error.to_string();
    assert!(text.contains("pthread_cond_timedwait"), "{text}");
    assert!(
        text.contains(&format!("{} seconds", omni_android::bionic::MAX_SLEEP_SECONDS)),
        "the refusal must name the cap it applied: {text}"
    );
    assert!(text.contains("CLOCK_REALTIME"), "and the clock it measured against: {text}");
}



/// **A guest thread blocked in `pthread_cond_wait` is visible from outside while it is blocked.**
///
/// `jni-surface.md` §8.1's fifth failure mode, as a measurement. §8 row 14 has
/// `GameActivity_onCreate` blocking here until the game thread signals `app->running`, and that
/// file says a deadlock there is indistinguishable from a hang. It is indistinguishable from
/// outside; `Bionic::parked` is the inside, and this is what asserts it works **before** M5's gate
/// needs it.
///
/// **A detector rather than a watch** (`VERIFICATION.md` entry 11): the assertion is that the list
/// is non-empty *while a thread is genuinely parked* and empty again *after it is released*, which
/// a counter that only rose under load could not distinguish. The peak is read too, and it is
/// labelled a watch where it is defined.
#[test]
fn a_thread_blocked_in_pthread_cond_wait_is_visible_while_it_is_blocked() {
    let _guard = serialized();
    let f = fixture_with_threads(4);

    let cond = f.guest.data + 0x400;
    let mutex = f.guest.data + 0x480;
    let handle = f.guest.data + 0x500;
    let entered = f.guest.data + 0x508;
    f.guest.write_u64(entered, 0);

    // The waiter: lock, say it is here, wait, unlock, return.
    let waiter = {
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_lock"));
        asm.mov(9, entered as u64);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
        asm.mov(0, cond as u64);
        asm.mov(1, mutex as u64);
        asm.bl(f.thunk("pthread_cond_wait"));
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_unlock"));
        asm.mov(0, 0);
        asm.push(ret(21));
        f.guest.load(asm.words());
        entry
    };
    // Every program is assembled before any guest thread runs: `Guest::load` reprotects the whole
    // code region, and doing that under a running guest thread faults it.
    let create = {
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, cond as u64);
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_cond_init"));
        asm.mov(0, mutex as u64);
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_mutex_init"));
        asm.mov(0, handle as u64);
        asm.mov(1, 0);
        asm.mov(2, waiter as u64);
        asm.mov(3, 0);
        asm.bl(f.thunk("pthread_create"));
        asm.mov(22, f.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        f.guest.load(asm.words());
        entry
    };
    let release = {
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, cond as u64);
        asm.bl(f.thunk("pthread_cond_broadcast"));
        asm.mov(22, handle as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.mov(22, f.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        f.guest.load(asm.words());
        entry
    };

    assert!(f.bionic.parked().is_empty(), "nothing is parked before anything runs");
    assert_eq!(f.bionic.parked_peak(), 0);

    // **A fresh context per program.** The first program's `BL`s leave `X30` pointing into the
    // middle of itself, and a second program's `MOV X21, X30` would save that and `RET X21` into
    // it -- a loop, not a return, which shows up as the whole budget being spent.
    // `tests/thread_memory.rs` records the same trap.
    {
        let _active = f.bionic.activate().expect("a thread block");
        let exit = run_program(&f, create).expect("the create completes");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    }
    assert_eq!(f.guest.read_u64(f.guest.data), 0, "pthread_create must succeed");

    // **Wait for the witness rather than for a duration.** A sleep long enough to be reliable is
    // `VERIFICATION.md` entry 6's shape; a bounded poll on the thing under test is not.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let observed = loop {
        let parked = f.bionic.parked();
        if !parked.is_empty() {
            break parked;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no thread appeared in the park witness in 30 seconds; the waiter reported entering: \
             {}",
            f.guest.read_u64(entered)
        );
        std::thread::yield_now();
    };

    assert_eq!(observed.len(), 1, "exactly one thread is waiting: {observed:?}");
    let held = &observed[0];
    assert_eq!(held.symbol, "pthread_cond_wait", "the symbol it is inside");
    assert_eq!(held.cond, cond as u64, "the condition variable it is on");
    assert_eq!(held.mutex, mutex as u64, "and the mutex it released to wait");
    assert!(!held.thread.is_none(), "a real guest thread, not the reserved none-value");
    assert_eq!(f.bionic.parked_peak(), 1);

    {
        let _active = f.bionic.activate().expect("a thread block");
        let exit = run_program(&f, release).expect("the broadcast and join complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    }
    assert_eq!(f.guest.read_u64(f.guest.data), 0, "pthread_join must succeed");
    assert!(
        f.bionic.parked().is_empty(),
        "the guard must remove the entry on the way out: a stale one makes a run that finished \
         look like the deadlock this exists to find"
    );
    assert_eq!(f.bionic.parked_peak(), 1, "the peak survives the thread that made it");
}

// =================================================================== adapter review finding M1
//
// `read`, `pread` and `__write_chk` consumed from the descriptor **before** validating the guest
// buffer, so a half-mapped buffer kept some bytes behind a reported failure. D22 wrote the rule
// down for `arc4random_buf`; phase 3b did not carry it across. Three detectors follow, and every
// one of them needs a **half-mapped** buffer: valid for its first part and not for the rest. A
// wholly invalid buffer refuses in both versions and proves nothing, which is why the fixture
// below *asserts* the cliff rather than assuming it.

/// A one-page mapping with free address space immediately after it, filled with `0xAA`.
///
/// Returns the base and the page size. A **failure** rather than a skip if the shape cannot be
/// built: `VERIFICATION.md` entry 4 is about a test that early-returned when its fixture was
/// missing and still reported `ok`.
fn island_with_a_cliff(f: &Fixture) -> (omni_cpu::GuestAddr, usize) {
    use omni_mem::{CommitPolicy, Placement, Protection};
    let page = f.guest.space.page_size();
    let island = f
        .guest
        .space
        .map_anonymous(
            Placement::Fixed(f.guest.unmapped & !(page - 1)),
            page,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("an island mapping");
    assert!(f.guest.space.region_at(island).is_some(), "the island itself must be mapped");
    assert!(
        f.guest.space.region_at(island + page).is_none(),
        "these tests need free address space immediately after the island. Without the cliff \
         there is no half-mapped buffer, every refusal below would be a refusal of a wholly bad \
         pointer, and the test would pass for a reason that has nothing to do with M1"
    );
    f.guest.write_bytes(island, &vec![0xAAu8; page]);
    (island, page)
}

/// **M1: a `read` into a half-mapped buffer takes nothing out of the descriptor.**
///
/// The buffer's first sixteen bytes are writable and the rest of it is off the end of the
/// mapping. Before the fix the loop read twenty-four bytes from the descriptor and *then* tried
/// to place them: the placing access refused, the guest was told the call failed, and the bytes
/// were gone. Guest memory looks identical either way — `write_bytes` checks before it copies —
/// so **the descriptor is the only thing that can discriminate**, and it is what is asserted.
///
/// A pipe and a regular file are both exercised, because the fix treats them the same and the
/// reason it can is that the check happens before either is touched.
#[test]
fn a_read_into_a_half_mapped_buffer_consumes_nothing_from_the_descriptor() {
    let _guard = serialized();
    let (f, scratch) = rooted("m1-read");
    let (island, page) = island_with_a_cliff(&f);
    // Sixteen writable bytes, then the cliff. A twenty-four byte read straddles it.
    let straddling = island + page - 16;

    // --- A pipe. Destructive: there is nothing to seek back to, so a lost byte is lost.
    let fs = f.bionic.filesystem().expect("a root");
    let (read_fd, write_fd) = pipe_through_guest(&f);
    const SENT: &[u8] = b"012345678901234567890123";
    assert_eq!(fs.write(write_fd, SENT).expect("twenty-four bytes into the pipe"), 24);

    let error = refusal_of(&f, "read", |asm| {
        asm.mov(0, read_fd as u64);
        asm.mov(1, straddling as u64);
        asm.mov(2, 24);
    });
    assert_eq!(error.symbol(), Some("read"), "{error:?}");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    assert!(error.guest_address().is_some(), "{error}");

    let mut back = [0u8; 24];
    assert_eq!(fs.read(read_fd, &mut back).expect("the pipe still holds its bytes"), 24);
    assert_eq!(&back, SENT, "a refused `read` must consume nothing from the pipe");
    assert_eq!(
        read_guest(&f, straddling, 16),
        vec![0xAAu8; 16],
        "and it must place nothing in the part of the buffer that was writable"
    );

    // --- A regular file. Recoverable by seeking, and deliberately not treated differently: the
    // assertion is that the offset never moved, not that something put it back.
    std::fs::write(scratch.path("m1"), SENT).expect("a file");
    let fd = open_through_guest(&f, "/m1", O_RDONLY);
    assert!(fd >= 0, "open failed with {fd}");
    let error = refusal_of(&f, "read", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, straddling as u64);
        asm.mov(2, 24);
    });
    assert_eq!(error.symbol(), Some("read"), "{error:?}");
    let mut back = [0u8; 8];
    assert_eq!(fs.read(fd, &mut back).expect("the file is readable"), 8);
    assert_eq!(&back, b"01234567", "the refused `read` must not have moved the file offset");

    // --- `pread`, which names an offset rather than moving one. It loses the least of the three
    // and is held to the same rule, because the rule is about the order of the two steps.
    let error = refusal_of(&f, "pread", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, straddling as u64);
        asm.mov(2, 24);
        asm.mov(3, 0);
    });
    assert_eq!(error.symbol(), Some("pread"), "{error:?}");
    assert_eq!(read_guest(&f, straddling, 16), vec![0xAAu8; 16], "`pread` places nothing either");

    // --- **The over-correction validating first invites**, asserted so that a later "check it
    // always" cannot land unnoticed. A zero-length transfer at a null pointer is legal C, the
    // seam is never reached, and there is nothing to protect; a check applied unconditionally
    // would refuse a correct program. This is the direction-B half of the finding.
    assert_eq!(
        value_of(&f, "read", |asm| {
            asm.mov(0, read_fd as u64);
            asm.mov(1, 0);
            asm.mov(2, 0);
        }) as i64,
        0,
        "`read(fd, NULL, 0)` is zero, not a refusal"
    );
    assert_eq!(
        value_of(&f, "__write_chk", |asm| {
            asm.mov(0, write_fd as u64);
            asm.mov(1, 0);
            asm.mov(2, 0);
            asm.mov(3, 0);
        }) as i64,
        0,
        "and `__write_chk(fd, NULL, 0, 0)` is zero too"
    );
}

/// **M1, the source side: a `__write_chk` out of a half-mapped buffer puts nothing into the
/// descriptor.**
///
/// The count has to **cross a chunk boundary** for this to be a test of anything. The loop moves
/// [`IO_BLOCK`](omni_platform::fs::IO_BLOCK) at a time and reads each chunk out of guest memory
/// in one access, so for `count <= IO_BLOCK` the source was already validated before any host
/// write and the defect cannot show. It shows at the *second* chunk, which is the one the first
/// chunk's host write precedes — so the buffer is one page long and the count is two.
#[test]
fn a_write_chk_from_a_half_mapped_buffer_puts_nothing_into_the_descriptor() {
    let _guard = serialized();
    let (f, scratch) = rooted("m1-write");
    let (island, page) = island_with_a_cliff(&f);
    let io_block = omni_platform::fs::IO_BLOCK;
    assert_eq!(
        page, io_block,
        "this test's arithmetic needs a page to be exactly one IO_BLOCK; on a host where it is \
         not, the second chunk would still be inside the island and nothing would be tested"
    );
    let count = (io_block * 2) as u64;

    let fd = open_through_guest(&f, "/m1w", O_WRONLY | O_CREAT);
    assert!(fd >= 0, "open failed with {fd}");
    let error = refusal_of(&f, "__write_chk", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, island as u64);
        asm.mov(2, count);
        asm.mov(3, count);
    });
    assert_eq!(error.symbol(), Some("__write_chk"), "{error:?}");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    assert_eq!(
        std::fs::metadata(scratch.path("m1w")).expect("the file exists").len(),
        0,
        "a refused `__write_chk` must not have put its first chunk into the file"
    );

    // `write` shares the loop, so it shares the rule; asserted rather than assumed, because a fix
    // applied to one of the two callers is exactly the shape a review finds next.
    let fd = open_through_guest(&f, "/m1w2", O_WRONLY | O_CREAT);
    assert!(fd >= 0, "open failed with {fd}");
    let error = refusal_of(&f, "write", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, island as u64);
        asm.mov(2, count);
    });
    assert_eq!(error.symbol(), Some("write"), "{error:?}");
    assert_eq!(
        std::fs::metadata(scratch.path("m1w2")).expect("the file exists").len(),
        0,
        "and neither must a refused `write`"
    );

    // **The source is admitted for reading, not for writing.** A guest writing its own `.rodata`
    // to a descriptor is an ordinary correct program, and admitting the source as writable -- the
    // over-correction a copy of the destination rule produces -- would refuse it. Asserted here
    // because the two calls differ by one boolean and nothing else would notice.
    let fd = open_through_guest(&f, "/m1w3", O_WRONLY | O_CREAT);
    assert!(fd >= 0, "open failed with {fd}");
    let wrote = value_of(&f, "__write_chk", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, f.guest.readonly as u64);
        asm.mov(2, 16);
        asm.mov(3, 16);
    }) as i64;
    assert_eq!(wrote, 16, "sixteen bytes of read-only guest memory are a legal source");
    assert_eq!(
        std::fs::metadata(scratch.path("m1w3")).expect("the file exists").len(),
        16
    );
}

/// **M1's shape in a third place: a `readdir` that cannot place its entry consumes no entry.**
///
/// `Filesystem::readdir` advances the stream, and there is no `rewinddir` and no `seekdir` here,
/// so an entry that is consumed and then cannot be written is an entry the guest can never
/// obtain — a directory silently one file short.
///
/// The destination is the adapter's own arena slot rather than a pointer the guest chose, so the
/// only way to reach the check is for the guest to reprotect the arena under itself. It can:
/// `mprotect` is bound and does not exclude the arena. `VERIFICATION.md` entry 12 — a branch no
/// input can take is not a check — so the input is constructed here rather than argued about.
#[test]
fn a_readdir_that_cannot_write_its_entry_consumes_no_entry() {
    use omni_mem::Protection;
    let _guard = serialized();
    let (f, scratch) = rooted("m1-readdir");
    std::fs::create_dir(scratch.path("d")).expect("a directory");
    for name in ["alpha", "beta", "gamma"] {
        std::fs::write(scratch.path(&format!("d/{name}")), b"x").expect("a file");
    }
    let path = f.cstring(f.guest.data + 0x100, b"/d");
    let dirp = value_of(&f, "opendir", |asm| {
        asm.mov(0, path as u64);
    });
    assert_ne!(dirp, 0, "opendir returned NULL");

    let page = f.guest.space.page_size();
    let slot_page = (dirp as omni_cpu::GuestAddr) & !(page - 1);
    assert_ne!(
        slot_page,
        f.bionic.arena() & !(page - 1),
        "the page being made read-only must not be the one holding thread block zero, or this \
         test would be about `errno` rather than about `readdir`"
    );
    f.guest.space.protect(slot_page, page, Protection::Read).expect("the slot page read-only");
    let error = refusal_of(&f, "readdir", |asm| {
        asm.mov(0, dirp);
    });
    assert_eq!(error.symbol(), Some("readdir"), "{error:?}");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    f.guest.space.protect(slot_page, page, Protection::ReadWrite).expect("writable again");

    // Membership against the directory's real contents. A count would see this defect too — it
    // loses exactly one entry — but a count cannot say *which* one went missing, and entry 1 is
    // about exactly that difference.
    let mut names = Vec::new();
    for _ in 0..16 {
        let entry = value_of(&f, "readdir", |asm| {
            asm.mov(0, dirp);
        });
        if entry == 0 {
            break;
        }
        names.push(
            String::from_utf8(f.read_cstring(entry as omni_cpu::GuestAddr + 19))
                .expect("a UTF-8 name"),
        );
    }
    names.sort();
    assert_eq!(
        names,
        vec![
            ".".to_string(),
            "..".to_string(),
            "alpha".to_string(),
            "beta".to_string(),
            "gamma".to_string(),
        ],
        "a refused `readdir` must not have advanced the stream"
    );
    assert_eq!(value_of(&f, "closedir", |asm| { asm.mov(0, dirp); }) as i64, 0);
}

// ============================================== M1's shape in the `FILE *` layer (the stream side)
//
// `fgets`, `fread` and `fwrite` reached the descriptor **before** the guest's buffer had been
// validated, exactly as `read`, `pread` and `__write_chk` did. The loops are in
// `omni_bionic::stdio`, whose entire guest-memory vocabulary is `GuestMemory::read`/`write` — no
// way to ask whether a range is mapped without writing to it, and no refusal channel — so the
// admission lives in the adapter, where `GuestMem::checked_ptr` already is. Same reasoning, same
// place, as the zero-byte-write contract.
//
// Every detector below needs a **half-mapped** buffer and reaches the descriptor through a
// **pipe**, because the return value is a refusal in both versions: only the descriptor can tell
// a fix from the defect. `island_with_a_cliff` above *asserts* its cliff rather than skipping.

/// A stream over a descriptor the guest already holds, through real guest code.
fn fdopen_through_guest(f: &Fixture, fd: i32, mode: &[u8]) -> u64 {
    let at = f.cstring(f.guest.data + 0xc0, mode);
    let stream = value_of(f, "fdopen", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, at as u64);
    });
    assert_ne!(stream, 0, "fdopen returned NULL for fd {fd}");
    stream
}

/// Call `symbol`, then read this thread's `errno` back through `__errno`.
///
/// The cell is seeded with **`EDOM`** first, which nothing in the stream layer can produce, so an
/// assertion below distinguishes *set to this* from *left alone* and from *cleared*. Asserting
/// against zero would pass for a layer that cleared `errno`, which POSIX forbids.
fn value_and_errno(f: &Fixture, symbol: &str, setup: impl FnOnce(&mut Asm)) -> (u64, i32) {
    let thunk = f.thunk(symbol);
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.bl(f.thunk("__errno"));
    asm.mov(1, 33); // EDOM, the sentinel.
    asm.push(str_w(1, 0, 0));
    setup(&mut asm);
    asm.bl(thunk);
    asm.mov(22, f.guest.data as u64);
    asm.push(str_imm(0, 22, 0));
    asm.bl(f.thunk("__errno"));
    asm.push(ldr_w(1, 0, 0));
    asm.push(str_imm(1, 22, 8));
    asm.push(ret(21));
    f.guest.load(asm.words());
    let mut cpu = f.guest.thread(&f.boundary);
    let exit = f.run(&mut cpu, entry).expect("the run must complete");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    (f.guest.read_u64(f.guest.data), f.guest.read_u64(f.guest.data + 8) as i32)
}

/// **A `fread` into a half-mapped buffer takes nothing out of the descriptor.**
///
/// The defect is invisible below one [`TRANSFER_CHUNK`](omni_bionic::stdio::TRANSFER_CHUNK), for
/// the reason the `__write_chk` test already documents: the loop moves a chunk at a time and
/// places each one in a single access, so for `total <= TRANSFER_CHUNK` the placing access failed
/// before the descriptor was read a second time. It shows at the **second** chunk — the one the
/// first chunk's descriptor read precedes — so the buffer is one page and the request is two.
///
/// The discriminating assertion is on the **pipe**, not on guest memory. A pipe read is
/// destructive and there is nothing to seek back to; unfixed, the first 4096 bytes are placed in
/// the island, the next arrive and cannot be placed, and every one of them is gone behind a
/// return value that says the call failed.
#[test]
fn a_fread_into_a_half_mapped_buffer_consumes_nothing_from_the_descriptor() {
    let _guard = serialized();
    let (f, scratch) = rooted("m1-fread");
    let (island, page) = island_with_a_cliff(&f);
    assert_eq!(
        page,
        omni_bionic::stdio::TRANSFER_CHUNK,
        "this test's arithmetic needs a page to be exactly one TRANSFER_CHUNK; on a host where \
         it is not, the second chunk would still be inside the island and nothing would be tested"
    );

    // --- A pipe. The bytes it gives up cannot be recovered.
    let fs = f.bionic.filesystem().expect("a root");
    let (read_fd, write_fd) = pipe_through_guest(&f);
    let sent: Vec<u8> = (0..page + 16).map(|i| (i % 251) as u8).collect();
    assert_eq!(fs.write(write_fd, &sent).expect("the pipe takes it all"), sent.len());
    let stream = fdopen_through_guest(&f, read_fd, b"r");

    let error = refusal_of(&f, "fread", |asm| {
        asm.mov(0, island as u64);
        asm.mov(1, 1);
        asm.mov(2, 2 * page as u64);
        asm.mov(3, stream);
    });
    assert_eq!(error.symbol(), Some("fread"), "{error:?}");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    assert!(error.guest_address().is_some(), "{error}");

    let mut back = vec![0u8; sent.len()];
    let got =
        fs.read(read_fd, &mut back).expect("a refused fread leaves the pipe holding its bytes");
    assert_eq!(got, sent.len(), "a refused `fread` must consume nothing from the pipe");
    assert_eq!(back, sent, "and the bytes must be the ones that were sent, in order");
    assert_eq!(
        read_guest(&f, island, 64),
        vec![0xAAu8; 64],
        "and it must place nothing in the part of the buffer that WAS writable -- unfixed, the \
         first chunk lands here behind a call the guest was told had failed"
    );

    // --- A regular file. Recoverable by seeking, and deliberately held to the same rule: the
    // assertion is that the offset never moved, not that something put it back.
    std::fs::write(scratch.path("f"), &sent).expect("a file");
    let fd = open_through_guest(&f, "/f", O_RDONLY);
    assert!(fd >= 0, "open failed with {fd}");
    let stream = fdopen_through_guest(&f, fd, b"rb");
    let error = refusal_of(&f, "fread", |asm| {
        asm.mov(0, island as u64);
        asm.mov(1, 1);
        asm.mov(2, 2 * page as u64);
        asm.mov(3, stream);
    });
    assert_eq!(error.symbol(), Some("fread"), "{error:?}");
    let mut head = [0u8; 8];
    assert_eq!(fs.read(fd, &mut head).expect("the file is readable"), 8);
    assert_eq!(&head, &sent[..8], "the refused `fread` must not have moved the file offset");
}

/// **A `fgets` into a half-mapped buffer takes nothing out of the descriptor.**
///
/// `fgets` reads one byte per `read(2)` — deliberately, so that it stops *on* its newline and
/// leaves the next byte for the next call — and accumulates them host-side until the newline or
/// the capacity. It is the whole accumulated line that is then placed, so a destination whose
/// first sixteen bytes are writable swallows a forty-one byte line one byte at a time and loses
/// every one of them at the single placing access.
///
/// Sixteen writable bytes and a `size` of 64: the admission is `size`, because C17 7.21.7.2p2
/// gives `fgets` an array of `size` characters to write `size - 1` characters and a terminator
/// into.
#[test]
fn an_fgets_into_a_half_mapped_buffer_consumes_nothing_from_the_descriptor() {
    let _guard = serialized();
    let (f, _scratch) = rooted("m1-fgets");
    let (island, page) = island_with_a_cliff(&f);
    // Sixteen writable bytes, then the cliff.
    let straddling = island + page - 16;

    let fs = f.bionic.filesystem().expect("a root");
    let (read_fd, write_fd) = pipe_through_guest(&f);
    const LINE: &[u8] = b"0123456789012345678901234567890123456789\n";
    assert_eq!(LINE.len(), 41, "forty bytes and a newline: past the sixteen that are writable");
    assert_eq!(fs.write(write_fd, LINE).expect("the line goes in"), LINE.len());
    let stream = fdopen_through_guest(&f, read_fd, b"r");

    let error = refusal_of(&f, "fgets", |asm| {
        asm.mov(0, straddling as u64);
        asm.mov(1, 64);
        asm.mov(2, stream);
    });
    assert_eq!(error.symbol(), Some("fgets"), "{error:?}");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");

    let mut back = [0u8; 41];
    assert_eq!(
        fs.read(read_fd, &mut back).expect("a refused fgets leaves the pipe holding its line"),
        41,
        "a refused `fgets` must consume nothing from the pipe -- unfixed it eats the whole line \
         one byte at a time and then finds it cannot place it"
    );
    assert_eq!(&back, LINE, "and the line must be intact and in order");
    assert_eq!(
        read_guest(&f, straddling, 16),
        vec![0xAAu8; 16],
        "and nothing is placed in the part of the buffer that was writable"
    );

    // **The admission is `size`, not `size - 1`, and this is the input that tells them apart.**
    // Sixteen bytes are writable. A `size` of 17 describes an array of seventeen characters, and
    // C17 7.21.7.2p2 lets `fgets` write sixteen of them plus a terminator -- so the seventeenth
    // byte is one a correct call reaches, and a check that admitted `size - 1` would let this
    // through, swallow sixteen bytes and lose them at the terminator. The companion assertion --
    // that a `size` of exactly 16 here SUCCEEDS -- is in
    // `the_stream_admissions_refuse_nothing_that_touches_neither_buffer_nor_descriptor`, and
    // neither half means anything without the other.
    assert_eq!(fs.write(write_fd, LINE).expect("the line goes back in"), LINE.len());
    let error = refusal_of(&f, "fgets", |asm| {
        asm.mov(0, straddling as u64);
        asm.mov(1, 17);
        asm.mov(2, stream);
    });
    assert_eq!(error.symbol(), Some("fgets"), "{error:?}");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    let mut back = [0u8; 41];
    assert_eq!(
        fs.read(read_fd, &mut back).expect("the pipe still holds its line"),
        41,
        "one byte past the mapping is still past the mapping. A `size - 1` admission would \
         have taken sixteen bytes out of the pipe and then failed at the terminator"
    );
    assert_eq!(&back, LINE);
}

/// **A `fwrite` out of a half-mapped buffer puts nothing into the descriptor.**
///
/// The source side, and it is an instance rather than the mirror image that does not apply:
/// `transfer_out` reads one chunk out of guest memory, hands it to the descriptor, and only then
/// looks at the next. A byte in a **pipe** cannot be taken back out, and for the glue's command
/// pipe half a message is a command.
///
/// Like the `__write_chk` detector this mirrors, the count has to cross a chunk boundary: for
/// `total <= TRANSFER_CHUNK` the single `read_all` already failed before any host write.
#[test]
fn an_fwrite_from_a_half_mapped_buffer_puts_nothing_into_the_descriptor() {
    let _guard = serialized();
    let (f, scratch) = rooted("m1-fwrite");
    let (island, page) = island_with_a_cliff(&f);
    assert_eq!(page, omni_bionic::stdio::TRANSFER_CHUNK, "a page must be exactly one chunk here");

    let path = f.cstring(f.guest.data + 0x100, b"/w");
    let mode = f.cstring(f.guest.data + 0x140, b"wb");
    let stream = value_of(&f, "fopen", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, mode as u64);
    });
    assert_ne!(stream, 0, "fopen returned NULL");

    let error = refusal_of(&f, "fwrite", |asm| {
        asm.mov(0, island as u64);
        asm.mov(1, 1);
        asm.mov(2, 2 * page as u64);
        asm.mov(3, stream);
    });
    assert_eq!(error.symbol(), Some("fwrite"), "{error:?}");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    assert_eq!(
        std::fs::metadata(scratch.path("w")).expect("the file exists").len(),
        0,
        "a refused `fwrite` must not have put its first chunk into the descriptor"
    );
}

/// **And the calls the admission must NOT refuse**, which is the half of this that over-corrects.
///
/// `docs/VERIFICATION.md` entry 12 and the mutation table's `order-B1` are both about this: a
/// check applied unconditionally reads as stricter and is wrong. Four shapes, each asserted by
/// name rather than assumed:
///
/// * a **zero-length** transfer at a wholly unmapped pointer. C17 7.21.8.1p3 leaves the stream
///   unchanged for `size` or `nmemb` of zero, and `read(fd, NULL, 0)` is legal C.
/// * an **overflowing `size * nmemb`**, which is `EINVAL` and zero items, not a refusal about a
///   pointer that was never looked at. A refusal here would replace a defined answer.
/// * `fgets` with a **non-positive `size`**, which C17 7.21.7.2 does not define and which this
///   layer answers by touching neither the buffer nor the descriptor.
/// * an **exactly-fitting** buffer: `size` bytes admitted, not `size + 1`, and a full chunk
///   admitted, not a chunk plus one. Without these a check that over-reached by a byte would
///   still pass every refusing test above.
#[test]
fn the_stream_admissions_refuse_nothing_that_touches_neither_buffer_nor_descriptor() {
    let _guard = serialized();
    let (f, _scratch) = rooted("m1-stream-b");
    let (island, page) = island_with_a_cliff(&f);
    // Past the cliff: no mapping at all, so any admission of a non-zero length refuses here.
    let nowhere = island + page;

    let fs = f.bionic.filesystem().expect("a root");
    let (read_fd, write_fd) = pipe_through_guest(&f);
    let sent: Vec<u8> = (0..page).map(|i| (i % 251) as u8).collect();
    assert_eq!(fs.write(write_fd, &sent).expect("the pipe takes it all"), page);
    let stream = fdopen_through_guest(&f, read_fd, b"r");

    // --- Zero-length, at a pointer that is not a pointer.
    let (items, errno) = value_and_errno(&f, "fread", |asm| {
        asm.mov(0, nowhere as u64);
        asm.mov(1, 0);
        asm.mov(2, 4096);
        asm.mov(3, stream);
    });
    assert_eq!(items, 0, "zero items");
    assert_eq!(errno, 33, "and the stream is untouched, so `errno` keeps the EDOM sentinel");

    let (items, errno) = value_and_errno(&f, "fread", |asm| {
        asm.mov(0, nowhere as u64);
        asm.mov(1, 4096);
        asm.mov(2, 0);
        asm.mov(3, stream);
    });
    assert_eq!((items, errno), (0, 33), "`nmemb == 0` is the same answer as `size == 0`");

    // --- The unrepresentable product: EINVAL, which is an answer about the arguments and holds
    // whatever guest memory looks like. Admitting a wrapped or saturated product here would turn
    // it into a refusal about the pointer.
    let (items, errno) = value_and_errno(&f, "fread", |asm| {
        asm.mov(0, nowhere as u64);
        asm.mov(1, 1u64 << 32);
        asm.mov(2, 1u64 << 32);
        asm.mov(3, stream);
    });
    assert_eq!(items, 0, "zero items for a product that is not representable");
    assert_eq!(errno, 22, "EINVAL, and not the EDOM sentinel: the answer must still be given");

    // --- `fgets` with a size C does not define.
    for size in [0u64, 0xffff_ffff_ffff_ffff] {
        let (returned, errno) = value_and_errno(&f, "fgets", |asm| {
            asm.mov(0, nowhere as u64);
            asm.mov(1, size);
            asm.mov(2, stream);
        });
        assert_eq!(returned, 0, "NULL for a size of {size}");
        assert_eq!(errno, 33, "and no errno: C17 7.21.7.2 defines no error for it");
    }

    // --- Exactly-fitting destinations. A `fgets` of the last sixteen bytes of the island writes
    // fifteen characters and a terminator at most, and sixteen is what is admitted.
    let (f2, scratch2) = rooted("m1-stream-b2");
    let (island2, page2) = island_with_a_cliff(&f2);
    let fs2 = f2.bionic.filesystem().expect("a root");
    let (read2, write2) = pipe_through_guest(&f2);
    assert_eq!(fs2.write(write2, b"abc\n").expect("four bytes"), 4);
    let stream2 = fdopen_through_guest(&f2, read2, b"r");
    let edge = island2 + page2 - 16;
    assert_eq!(
        value_of(&f2, "fgets", |asm| {
            asm.mov(0, edge as u64);
            asm.mov(1, 16);
            asm.mov(2, stream2);
        }),
        edge as u64,
        "a `size` that exactly fills the mapping must be admitted, not refused: admitting \
         `size + 1` would look stricter and reject a correct call"
    );
    assert_eq!(f2.read_cstring(edge), b"abc\n", "and the line arrives, newline kept");

    // A whole-chunk `fread` into a whole-page island, and a whole-chunk `fwrite` out of it.
    let sent2: Vec<u8> = (0..page2).map(|i| (i % 241) as u8).collect();
    let mut offered = 0;
    while offered < sent2.len() {
        offered += fs2.write(write2, &sent2[offered..]).expect("the pipe takes it");
    }
    assert_eq!(
        value_of(&f2, "fread", |asm| {
            asm.mov(0, island2 as u64);
            asm.mov(1, 1);
            asm.mov(2, page2 as u64);
            asm.mov(3, stream2);
        }),
        page2 as u64,
        "a request that exactly fills the mapping must be admitted"
    );
    assert_eq!(read_guest(&f2, island2, page2), sent2, "and every byte arrives");

    let out = f2.cstring(f2.guest.data + 0x100, b"/o");
    let wmode = f2.cstring(f2.guest.data + 0x140, b"wb");
    let sink = value_of(&f2, "fopen", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, wmode as u64);
    });
    assert_ne!(sink, 0, "fopen returned NULL");
    assert_eq!(
        value_of(&f2, "fwrite", |asm| {
            asm.mov(0, island2 as u64);
            asm.mov(1, 1);
            asm.mov(2, page2 as u64);
            asm.mov(3, sink);
        }),
        page2 as u64,
        "a source that exactly fills the mapping must be admitted, and admitted READABLE: a \
         source demanded writable would refuse a guest writing out of its own .rodata"
    );
    assert_eq!(value_of(&f2, "fclose", |asm| { asm.mov(0, sink); }) as i64, 0);
    assert_eq!(std::fs::read(scratch2.path("o")).expect("the host file"), sent2);

    // --- And the source really is admitted READABLE rather than writable. Asserted over a
    // mapping that is genuinely read-only, because over a read-write one the two are the same
    // check and the test would pass for a reason that has nothing to do with the rule.
    f2.guest
        .space
        .protect(island2, page2, omni_mem::Protection::Read)
        .expect("the island read-only");
    let ro = f2.cstring(f2.guest.data + 0x180, b"/ro");
    let sink = value_of(&f2, "fopen", |asm| {
        asm.mov(0, ro as u64);
        asm.mov(1, wmode as u64);
    });
    assert_ne!(sink, 0, "fopen returned NULL");
    assert_eq!(
        value_of(&f2, "fwrite", |asm| {
            asm.mov(0, island2 as u64);
            asm.mov(1, 1);
            asm.mov(2, page2 as u64);
            asm.mov(3, sink);
        }),
        page2 as u64,
        "a guest writing out of its own .rodata is ordinary, and demanding the \
         destination-side access for a source reads as stricter while being wrong"
    );
    assert_eq!(value_of(&f2, "fclose", |asm| { asm.mov(0, sink); }) as i64, 0);
    assert_eq!(std::fs::read(scratch2.path("ro")).expect("the host file"), sent2);
    f2.guest
        .space
        .protect(island2, page2, omni_mem::Protection::ReadWrite)
        .expect("writable again");
}

// =========================================== M6: the two symbols a guest WORKER THREAD died on
//
// Both were found by `guest_thread_failures` rather than by a scripted downcall stopping, which
// is the first time that has been the discovery route. `BEYOND_THE_PREDICTION` carries the two
// failure records verbatim.

/// **A guest thread is told where its own stack really is.**
///
/// Four numbers, and the test asserts every one of them against a value this test *chose* rather
/// than against whatever the handler produced: the size is set on the thread host here, the
/// guard is the page size, the detach state follows from creating with a `NULL` attribute
/// object, and the base is pinned by a relation rather than by a constant, because its value is
/// whatever `map_anonymous` returned.
///
/// The relation is the point and it is what a constant could not do. The thread reads **its own
/// `SP`** as its first instruction and the assertion is that `SP` lies inside the range the
/// handler reported — so a base that named the mapping's true bottom (the `PROT_NONE` guard
/// page) rather than the first usable byte, or a size that folded the guard in, fails here. That
/// is the whole of what a caller does with this function: decide whether a pointer is on its own
/// stack, or how much of it is left.
///
/// And the thread **writes to the base it was told about** before returning. A host-side read
/// cannot check that afterwards — the runner unmaps the stack before the join returns — but the
/// store itself is the check: a base inside the guard is `PROT_NONE`, so the store faults, the
/// thread dies, and `pthread_join` refuses instead of answering 0. Three assertions below fail
/// together in that case, which is why `guest_thread_failures` is asserted empty by name.
#[test]
fn pthread_getattr_np_reports_the_stack_the_runtime_gave_the_thread() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&f.guest.backend) as _;
    // **Not the layer's default.** A test that asserted `DEFAULT_GUEST_STACK_BYTES` would pass
    // against a handler that reported the policy number instead of the mapping, which is exactly
    // the plausible wrong answer here: the two are equal for a default-attribute thread.
    const STACK: usize = 256 * 1024;
    f.bionic
        .set_thread_host(ThreadHost::new(backend).with_limit(2).with_default_stack(STACK))
        .expect("a thread host");
    let page = f.guest.space.page_size();
    let out = f.guest.data + 0x800;
    let attr = f.guest.data + 0xA00;
    let seen = f.guest.data + 0xB00;
    // Poisoned, so every field asserted below had to be written by this call rather than found
    // already holding the value that was wanted.
    f.guest.write_bytes(attr, &[0xAB; 56]);
    f.guest.write_u64(seen, 0);
    f.guest.write_u64(seen + 8, 0);

    let start = start_routine(&f, |asm| {
        asm.push(mov_reg(19, 30));
        asm.mov(20, seen as u64);
        // `ADD X9, SP, #0` is `MOV X9, SP`. First instruction of the thread, before a call can
        // have moved it.
        asm.push(add_imm(9, 31, 0));
        asm.push(str_imm(9, 20, 0));
        asm.bl(f.thunk("pthread_self"));
        asm.mov(1, attr as u64);
        asm.bl(f.thunk("pthread_getattr_np"));
        asm.push(str_imm(0, 20, 8));
        // Write to the lowest byte it was told it may touch.
        asm.mov(9, attr as u64);
        asm.push(ldr_imm(10, 9, 24));
        asm.mov(11, 0x5A5A);
        asm.push(str_imm(11, 10, 0));
        asm.mov(0, 0);
        asm.push(mov_reg(30, 19));
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 24));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0, "pthread_create");
    assert_eq!(f.guest.read_u64(out + 24), 0, "pthread_join: the thread returned, so it did not \
        fault writing to the base it was given");
    assert_eq!(f.bionic.guest_thread_failures(), Vec::new(), "no thread failed");
    assert_eq!(f.guest.read_u64(seen + 8), 0, "pthread_getattr_np answers 0 for its own thread");

    let detach = f.guest.read_u64(attr) & 0xFFFF_FFFF;
    let size = f.guest.read_u64(attr + 8);
    let guard = f.guest.read_u64(attr + 16);
    let base = f.guest.read_u64(attr + 24);
    assert_eq!(detach, 0, "created with a NULL attr, so PTHREAD_CREATE_JOINABLE");
    assert_eq!(size, STACK as u64, "the stack this instance was configured to give a thread");
    assert_eq!(guard, page as u64, "a NULL attr gets this layer's own one-page guard");
    assert_ne!(base, 0, "a base of zero is the `attr_init` encoding for \"system default\"");
    assert_eq!(base % page as u64, 0, "the mapping and its guard are both page-aligned");

    // **The relation, and then the tighter fact separately** (`docs/VERIFICATION.md` entry 10):
    // the first is what every caller of this function relies on, the second is what this layer
    // happens to do today and would have to be restated if it ever pushed an initial frame.
    let sp = f.guest.read_u64(seen);
    assert!(
        base < sp && sp <= base + size,
        "the thread's own SP {sp:#x} must be inside the stack it was told it has \
         ({base:#x}..{:#x})",
        base + size
    );
    assert_eq!(sp, base + size, "and a thread starts at the very top of its stack");
}

/// **A detached thread's attr says DETACHED, and a guard size of zero is reported as a value.**
///
/// Two facts the joinable case cannot show. The detach state is read out of the instance's own
/// record on every call rather than copied when the thread was created — a copy would answer
/// JOINABLE for a thread that had since been detached, which is the answer that makes a caller
/// decide to join something nobody may join. And the guard here is **0** rather than a page,
/// because `pthread_attr_init` zeroes the guard field and `pthread_create` honours it: only a
/// `NULL` attribute object means "this layer's default", which is the `Some(0) => 0` arm there.
///
/// Nobody joins a detached thread, so there is no call to wait on. The host polls the thread's
/// own done-word under a deadline that **fails** rather than skips, which is
/// `docs/VERIFICATION.md` entries 4 and 6 together: a fixed sleep would be a guess about a
/// machine, and a missing fixture that reports `ok` is not a test.
#[test]
fn a_detached_threads_attr_says_detached_and_a_zero_guard_is_reported_as_zero() {
    let _guard = serialized();
    let f = fixture_with_threads(2);
    let out = f.guest.data + 0x800;
    let attr = f.guest.data + 0xA00;
    let live = f.guest.data + 0xA80;
    let done = f.guest.data + 0xB80;
    f.guest.write_bytes(live, &[0xCD; 56]);
    f.guest.write_u64(done, 0);
    f.guest.write_u64(done + 8, 0);

    let start = start_routine(&f, |asm| {
        asm.push(mov_reg(19, 30));
        asm.mov(20, done as u64);
        asm.bl(f.thunk("pthread_self"));
        asm.mov(1, live as u64);
        asm.bl(f.thunk("pthread_getattr_np"));
        asm.push(str_imm(0, 20, 8));
        // Written last, so that the host polling on it sees everything above.
        asm.mov(9, 1);
        asm.push(str_imm(9, 20, 0));
        asm.mov(0, 0);
        asm.push(mov_reg(30, 19));
    });

    let entry = program(&f, |asm| {
        asm.mov(0, attr as u64);
        asm.bl(f.thunk("pthread_attr_init"));
        asm.mov(0, attr as u64);
        asm.mov(1, 1); // PTHREAD_CREATE_DETACHED
        asm.bl(f.thunk("pthread_attr_setdetachstate"));
        create_call(&f, asm, out, attr as u64, start, 0);
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0, "pthread_create");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while f.guest.read_u64(done) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the detached thread never reached its done-word; failures: {:?}",
            f.bionic.guest_thread_failures()
        );
        std::thread::yield_now();
    }
    assert_eq!(f.guest.read_u64(done + 8), 0, "pthread_getattr_np answered 0");
    assert_eq!(f.guest.read_u64(live) & 0xFFFF_FFFF, 1, "PTHREAD_CREATE_DETACHED");
    assert_eq!(f.guest.read_u64(live + 16), 0, "an attr that asked for no guard is given none");
    assert_ne!(f.guest.read_u64(live + 24), 0, "and it still knows where the stack is");
    assert_ne!(f.guest.read_u64(live + 8), 0, "and how big it is");
}

/// **The three questions it refuses to answer, and the one POSIX error it does answer.**
///
/// Each of the three would have a believable wrong answer, and the middle one is the dangerous
/// case: reporting the *calling* thread's stack for somebody else's `pthread_t` hands out a
/// mapping that is real, readable and 256 KiB long, so nothing the caller can check would catch
/// it. A refusal naming the symbol and the thunk address is the only other honest answer.
#[test]
fn pthread_getattr_np_refuses_what_it_cannot_know_and_is_esrch_for_a_thread_that_is_not_there() {
    let _guard = serialized();
    let f = fixture_with_threads(2);
    let attr = f.guest.data + 0xA00;

    // 1. A `pthread_t` nothing answers to. **ESRCH is an answer, not a stop**: it is POSIX's own
    //    error for this function and guest code has a branch for it, and this instance genuinely
    //    knows the id is not one of its own -- the created-thread registry and the arena's thread
    //    table between them hold every identity it has ever handed out.
    let code = value_of(&f, "pthread_getattr_np", |asm| {
        asm.mov(0, 0xDEAD_BEEF);
        asm.mov(1, attr as u64);
    });
    assert_eq!(code, 3, "ESRCH for a thread this instance never handed out");

    // 2. This thread, which attached to the instance rather than being created by it -- the
    //    thread the initializers and every JNI downcall run on. The embedding gave it its stack
    //    and set `SP` itself; nothing in `Bionic` was told where that mapping is.
    let error = refusal_of(&f, "pthread_getattr_np", |asm| {
        asm.bl(f.thunk("pthread_self"));
        asm.mov(1, attr as u64);
    });
    assert_eq!(error.symbol(), Some("pthread_getattr_np"));
    assert_eq!(error.guest_address(), Some(f.thunk("pthread_getattr_np")));
    let text = error.to_string();
    assert!(
        text.contains("/proc/self/maps"),
        "the refusal must name what bionic reads to answer this and this runtime does not \
         have: {text}"
    );

    // 3. A thread this instance *did* start, asked about by a different thread. The record of
    //    where a thread's stack is lives on the thread itself, so this one is not a gap in the
    //    question but a gap in what is kept -- and the refusal says which change would lift it.
    let out = f.guest.data + 0x800;
    let start = start_routine(&f, |asm| {
        asm.mov(0, 0);
    });
    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, attr as u64);
        asm.bl(f.thunk("pthread_getattr_np"));
    });
    let error = run_program(&f, entry).expect_err("a refusal, not an answer");
    assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    assert_eq!(error.symbol(), Some("pthread_getattr_np"));
    let text = error.to_string();
    assert!(
        text.contains("not the thread asking"),
        "the refusal must say which thread it cannot answer for: {text}"
    );
}

/// **`pthread_mutex_trylock` takes a free mutex, answers `EBUSY` for a held one, and re-enters a
/// recursive one it already owns.**
///
/// The three branches, and the types are chosen so that **no defect in reach can hang this
/// test**. The held case uses an `ERRORCHECK` mutex rather than a `NORMAL` one for exactly that
/// reason: `pthread_mutex_lock` on an `ERRORCHECK` mutex the caller holds returns `EDEADLK`
/// immediately, so a `trylock` wired to `lock` fails this assertion with 35 where 16 was wanted.
/// Over a `NORMAL` mutex the same defect would **block for ever** inside the handler, where the
/// run budget cannot see it -- and a test that deadlocks under the defect it exists to detect
/// reports nothing at all (`docs/VERIFICATION.md` entry 8, and the `threads-B1` story beside
/// `detaching_twice_is_einval_and_joining_a_detached_thread_is_einval`).
///
/// The recursive case is the one that needs the **owner table**: a recursive mutex held by the
/// caller is a successful re-entry and one held by anybody else is `EBUSY`, and the two are the
/// same lock word. A binding that passed no owner table would answer `EBUSY` to a thread
/// re-entering its own mutex, which is a `std::recursive_mutex::try_lock` failing for no reason.
#[test]
fn trylock_takes_a_free_mutex_answers_ebusy_for_a_held_one_and_re_enters_a_recursive_one() {
    let _guard = serialized();
    let f = fixture();
    let attr = f.guest.data + 0x300;
    let mutex = f.guest.data + 0x200;
    let out = f.guest.data + 0x400;
    f.guest.write_bytes(mutex, &[0u8; 40]);
    f.guest.write_bytes(attr, &[0u8; 8]);

    let entry = program(&f, |asm| {
        asm.mov(23, out as u64);
        // --- An ERRORCHECK mutex: held is EBUSY, even by the thread holding it.
        asm.mov(0, attr as u64);
        asm.bl(f.thunk("pthread_mutexattr_init"));
        asm.mov(0, attr as u64);
        asm.mov(1, 2); // PTHREAD_MUTEX_ERRORCHECK
        asm.bl(f.thunk("pthread_mutexattr_settype"));
        asm.mov(0, mutex as u64);
        asm.mov(1, attr as u64);
        asm.bl(f.thunk("pthread_mutex_init"));
        asm.push(str_imm(0, 23, 0));
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_trylock"));
        asm.push(str_imm(0, 23, 8));
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_trylock"));
        asm.push(str_imm(0, 23, 16));
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_unlock"));
        asm.push(str_imm(0, 23, 24));
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_trylock"));
        asm.push(str_imm(0, 23, 32));
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_unlock"));

        // --- A RECURSIVE mutex: the owner re-enters it and the count is real.
        asm.mov(0, attr as u64);
        asm.mov(1, 1); // PTHREAD_MUTEX_RECURSIVE
        asm.bl(f.thunk("pthread_mutexattr_settype"));
        asm.mov(0, mutex as u64);
        asm.mov(1, attr as u64);
        asm.bl(f.thunk("pthread_mutex_init"));
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_trylock"));
        asm.push(str_imm(0, 23, 40));
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_trylock"));
        asm.push(str_imm(0, 23, 48));
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_unlock"));
        asm.push(str_imm(0, 23, 56));
        // Still held once, so a third party would still see EBUSY -- asserted through the
        // *second* unlock succeeding rather than by claiming it.
        asm.mov(0, mutex as u64);
        asm.bl(f.thunk("pthread_mutex_unlock"));
        asm.push(str_imm(0, 23, 64));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));

    assert_eq!(f.guest.read_u64(out), 0, "pthread_mutex_init (ERRORCHECK)");
    assert_eq!(f.guest.read_u64(out + 8), 0, "a free mutex is taken, and trylock answers 0");
    assert_eq!(
        f.guest.read_u64(out + 16),
        16,
        "a held mutex is EBUSY -- 35 here would be `lock`'s EDEADLK, which is the defect this \
         type was chosen to make visible rather than fatal"
    );
    assert_eq!(f.guest.read_u64(out + 24), 0, "pthread_mutex_unlock");
    assert_eq!(f.guest.read_u64(out + 32), 0, "and the unlock really released it");

    assert_eq!(f.guest.read_u64(out + 40), 0, "a free RECURSIVE mutex is taken");
    assert_eq!(f.guest.read_u64(out + 48), 0, "and its owner re-enters it rather than EBUSY");
    assert_eq!(f.guest.read_u64(out + 56), 0, "the first unlock of two");
    assert_eq!(f.guest.read_u64(out + 64), 0, "the second, which is what makes the count real");

    // **A watch, not a detector** (`docs/VERIFICATION.md` entry 11): it rises whenever this
    // layer blocks and would stay at zero under most ways of getting `trylock` wrong. What it
    // does say is the one thing the return codes cannot -- that nothing slept.
    let (waits, _) = f.bionic.futex().activity();
    assert_eq!(waits, 0, "a trylock must never enter the futex");
}
