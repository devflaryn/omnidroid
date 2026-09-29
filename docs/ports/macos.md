# macOS

Host: Apple M1, 16 GB, 16 KiB pages (`ssh berat@192.168.0.24`, checkout
`~/Desktop/omnidroid-unified`; in a non-interactive shell run `. ~/.cargo/env` first). Nothing here
is claimed for Intel Macs. Topic files: `macos-cpu.md` (dynarmic's arm64 backend),
`macos-memory.md`, `macos-window.md` (window, surface, audio), `macos-hvf.md` (the native backend,
D34).

## What runs (measured in PS99, place 8737899170, `omnidroid play`)

| run | commit | result |
|---|---|---|
| m8, fresh storage | `82f7f8f` | 50 fps, join to `onGameLoaded` 19 s, 2.8 GiB, gate passed |
| m10, 30 min | `562b80d` | 55.8 fps median, 2.9 GiB private flat, 2.8 cores, clean close; two threads lost at +490 s to a DNS failure the unix seam could not classify (fixed since: `getaddrinfo` returns `EAI_*`) |
| m11, 30 min | `7a56fdb` | 46.6 fps, then a worker died at +1325 s: a call landed in the translation of another function (stale code after a mid-run cache clear, arm64 only); m7 and m9 failed on the same worker kind |
| m12, 30 min, `OMNI_JIT_EXCLUSIVE_MONITOR=global` | `7a56fdb` | clean, 39.0 fps, 2.8 GiB, 3.0 cores, gate passed |

Memory is `phys_footprint` (there is no commit charge here). At the landing screen, before any
world, one instance is 811-841 MiB steady with a 1.0-1.1 GiB boot peak (`macos-memory.md`).

## Build from a fresh Mac

```sh
xcode-select --install                                      # clang, mig, the SDK
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
brew install cmake ninja molten-vk vulkan-loader            # optional: vulkan-tools
cp Roblox-2.738.1397.apk ~/Desktop/omnidroid-unified/        # a regular file or a hard link
cargo build --release -p omnidroid
tools/play.sh --cookie <file> --place <id>                  # or target/release/omnidroid play
python3 tools/footprint_mac.py --match '[d]eps/gameactivity' # memory from outside, optional
```

* **dynarmic's CMake build is per machine**, not in the checkout: with no
  `OMNIDROID_DYNARMIC_BUILD_DIR`, `dynarmic-sys/build.rs` builds in
  `~/Library/Caches/omnidroid/dynarmic/<target>-<profile>-<hash of the source path>` (it survives
  `cargo clean`, and the checkout's path may hold a space). The arm64 backend runs the arm64 guest:
  dynarmic translates A64 to A64 (no x64 path is involved), with fastmem, the low window (D41) and
  per-thread caches.
* **The APK must not be a symbolic link**: the gate hard-links it into the guest's root, and the
  guest filesystem refuses a link to a symlink.
* Off Windows, `dynarmic-sys/build.rs` passes `DYNARMIC_USE_BUNDLED_EXTERNALS=ON`; without it CMake
  links Homebrew's `fmt`, which the pin never named.
* Storage: `~/Library/Application Support/Omnidroid/data`, accounts beside it. Never run
  `osascript` over ssh (it raises a permission dialog on the owner's screen).

## Virtual memory

MEASURED with a probe, then pinned by `vm_footprint_macos`:

| operation | `phys_footprint` |
|---|---|
| reserve 16 GiB `PROT_NONE` | +0.00 MiB |
| commit (`mprotect` RW) 256 MiB | +0.00 MiB |
| touch every page of it | +256.1 MiB |
| decommit (a fresh `MAP_FIXED` mapping) | -256.1 MiB; re-committed pages read 0 |
| read a 64 MiB read-only file view | +0.00 MiB (clean file pages are the file cache's) |
| `MADV_FREE_REUSABLE` (not used) | drops, but **contents kept**: cannot give zero-on-recommit |
| `MADV_DONTNEED` (not used) | no measurable effect |

* Pages are 16 KiB and the guest is told so (`AT_PAGESZ`), as a 16 KiB Android 15 device says.
  `process_commit_charge` answers `phys_footprint`, which moves on touch, not on commit. The rest
  (`PROT_EXEC` file views, the refusal registry) is in `vm/macos.rs`'s module docs.
* A guest `madvise` of a 4 KiB range is 4 KiB-exact: `GuestSpace::discard` decommits whole host
  pages and zeroes partial ones (before `7a56fdb`, unaligned ranges were refused with `EINVAL`).

## The ELF loader on 16 KiB pages

* Relro is sealed as bionic seals it, the end rounded up: at 16 KiB that seals the page holding
  `libroblox.so`'s `.got`; `.data` starts on the next page (`loader_m1`).
* A library aligned below the host page is refused by name (`AlignBelowPageSize`), as on a 16 KiB
  device: of the APK's arm64 libraries only `libzstd-jni-1.5.7-6.so` (`p_align` 0x1000). Only
  `libroblox.so` is loaded, so nothing depends on it.

## The Linux personality (`omni-linux`, D39)

A1 passes here (`a1-mac`): the real AOSP 15 `toybox echo hello` prints `hello` and exits 0 through
the real `linker64` and `libc.so`; the only refusal is liblog's `socket` to logd, as on Windows.

* **The page is the host's.** `Mm` takes its page from the guest space and `AT_PAGESZ` says 16 KiB,
  as a 16 KiB Android 15 device does, so every `mmap`/`mprotect`/`munmap` the guest makes is whole
  host pages. Every ELF in the arm64 sysroot has `p_align` 0x4000 (1,590 checked), so nothing needs
  a private copy for alignment. A program or interpreter whose `PT_LOAD` offset and address
  disagree within the page is refused at exec (`EINVAL`), as a 16 KiB kernel refuses it; libraries
  are `linker64`'s to judge.
* **Top Byte Ignore**: dynarmic's arm64 backend already masks 56 mirrored bits (one `ubfx` before
  the fastmem access), so no patch; `tbi.rs` passes with zero slow-path entries. XNU itself enables
  TBI for user space (a native load through `ptr | 0x02 << 56` reaches `ptr`), so *without* the
  option a tagged guest pointer does not fault here either: `vm::host_ignores_top_byte`.
* The sysroot is not in git: copy `sysroot/aosp-35` from another machine and check it with
  `python3 tools/make_sysroot.py --verify sysroot/aosp-35` -- or make it here (done on the M1,
  2026-09-29: manifest sha256 `5b586655...`, the pinned one). `brew install e2fsprogs erofs-utils`
  for `debugfs`/`fsck.erofs`, and give the tool a **case-sensitive** scratch volume: it `rdump`s the
  ext4 images into `$TMPDIR`, and the image has paths that differ only by case (APFS is
  case-insensitive by default):

  ```sh
  curl -LO https://dl.google.com/android/repository/sys-img/android/arm64-v8a-35_r02.zip
  hdiutil create -size 20g -fs "Case-sensitive APFS" -volname omnics -type SPARSEBUNDLE cs.sparsebundle
  hdiutil attach cs.sparsebundle -mountpoint ./cs -nobrowse && mkdir cs/tmp
  export PATH=/opt/homebrew/opt/e2fsprogs/sbin:/opt/homebrew/opt/erofs-utils/bin:$PATH TMPDIR=$PWD/cs/tmp
  python3 tools/make_sysroot.py --zip arm64-v8a-35_r02.zip --out cs/aosp-35
  python3 tools/make_sysroot.py --zip arm64-v8a-35_r02.zip --out cs/aosp-35 --meta
  cp -R cs/aosp-35 sysroot/ && python3 tools/make_sysroot.py --verify sysroot/aosp-35
  ```

## The real-AOSP path (`omni-linux`) on macOS

`tools/aosp_play.sh --apk <apk> --cookie <file> --place <id>` (the shell spelling of
`aosp_play.ps1`). What it took, beyond the Linux personality that already ran here:

* **ART's low 4 GiB** (D41, `macos-low-window.md`): nothing maps below 4 GiB on macOS, so a guest
  space's part below 4 GiB is a based window and dynarmic adds the base there only (patch 0030).
  Every guest address space gets its own window, so a second ART in one host process works here.
* **Implicit null checks**: the Linux personality's CPU contexts serve a fault the guest meant once
  (`recompile_on_declined_fault` off); arm64 otherwise moved each such load to the callback path
  for good and the D4 invariant killed the process.
* **The paravirtual GPU on MoltenVK**: the host loader from the platform's candidates (dyld does
  not search `/opt/homebrew/lib`), and `VK_EXT_queue_family_foreign` emulated (ANGLE's
  hardware-buffer images need it; MoltenVK has none). `tests/d3a_gpu.rs` 3/3.

## What differs on this host

| area | macOS |
|---|---|
| guest faults | thread-level Mach exception ports, taken before dynarmic's task port; 43.5 us per resolved fault with a decommit (n = 2,000), against 2.05 us for Windows' VEH |
| CPU/JIT | dynarmic's **arm64** backend with patches 0002-0016, 0020, 0021 (`macos-cpu.md`, `patches/README.md`). **Per-thread translation caches**: the shared cache (D38) is x64-only. `INTERRUPTIBLE` is `ALL_SAFE` less `FAST_DISPATCH`. Code cache is `MAP_JIT`, W^X per thread |
| window | AppKit; the process's main thread is handed to AppKit before `main` (`window/macos/main_thread.rs`) |
| GPU | Vulkan through MoltenVK (Homebrew loader, portability enumeration); the engine accepts the M1 as an integrated GPU. No GLES host |
| audio | Core Audio `DefaultOutput` unit, 48 kHz stereo float, 512-frame period |
| web view | `WKWebView` on the AppKit thread (`webview_live` 10/10) |
| network | `net/macos.rs`: a refused connect polls `POLLHUP` alone, `SO_NOSIGPIPE` on every socket, path MTU is one don't-fragment bit |
| timers | a 1 ms `nanosleep` takes 1.49 ms (coalescing); a `THREAD_TIME_CONSTRAINT` thread gets 1.01 ms. Nothing acts on it yet |
| profiling | `sampler/macos.rs` (Mach `thread_suspend`); `OMNI_MEM_REPORT` prints the guest rows only (the host census refuses on macOS) |

## A native CPU backend: measured, not adopted (D34)

`omni-cpu`'s `native-hvf` feature runs the guest at EL0 under Hypervisor.framework: 5.7x
dynarmic's compute on real engine code, but 1.6 us per import crossing against 23.7 ns. Off by
default; evidence and how to run its tests: `macos-hvf.md`.

## Open

* **Stale translated code on arm64** (m11): fixed in this tree by patch 0031 (a mid-run clear
  forgets the return-stack buffer; `omni-cpu/tests/cache_clear_rsb.rs` fails without it). Branch
  `arm64-clear-audit`'s patch 0023 addressed the same defect and is not merged; reconcile the two
  if it is.
* Captured pointer motion is accelerated; the physical wheel's sign under natural scrolling is not
  measured (`macos-window.md`). The census behind `OMNI_MEM_REPORT`'s host rows is not written.

## Merge notes

* `arm64-clear-audit` (patch 0023; `OMNI_JIT_CODE_CACHE_MB` and full-cache clears on the PERF
  line) is not in `unified`; see Open.
* `tools/mutate_mac/android.py` (`mac-and-` rows) covers shared `omni-android` behaviour the Mac
  exposed: e.g. `VK_SUBOPTIMAL_KHR` from acquire, which Android never answers, killed the render
  thread at MoltenVK's first resize.
