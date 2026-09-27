# D2: graphics buffers -- a host allocator, a vendor mapper, one `shm` region per buffer

Status: design, 2026-09-27. Follows D1 (host binder services). Part of sub-project D
(`2026-09-27-display-and-host-hals-design.md`).

## What the image expects, measured

- The image's vendor partition declares **HIDL gralloc 3** (`android.hardware.graphics.gralloc3.ranchu.xml`):
  `allocator@3.0::IAllocator/default` over hwbinder and a passthrough `mapper@3.0`, whose one
  implementation (`mapper@3.0-impl-ranchu.so`) opens goldfish's address-space device in its
  constructor. Nothing here provides that device, so the image's gralloc cannot run.
- Android 15's libui (`frameworks/native/libs/ui/Gralloc5.cpp`) tries **gralloc 5 first**: when
  `android.hardware.graphics.allocator.IAllocator/default` is declared in VINTF
  (`AServiceManager_isDeclared`), it waits for that AIDL service, requires interface version >= 2,
  asks `getIMapperLibrarySuffix()`, and loads `/vendor/lib64/hw/mapper.<suffix>.so` into the
  **sphal** linker namespace (`android_load_sphal_library`; linkerconfig's sphal namespace searches
  `/vendor/${LIB}/hw`). That library exports `AIMapper_loadIMapper` (the stable-C IMapper,
  `hardware/interfaces/graphics/mapper/stable-c/.../IMapper.h`). Every buffer a process allocates
  through libui (`GraphicBuffer`, `AHardwareBuffer`, SurfaceFlinger's, an app's) goes this way.

## Decisions

1. **Gralloc 5.** omnidroid serves the AIDL `IAllocator` (V2) from the host, on the D1 endpoint, and
   supplies the in-process half, `mapper.omni.so`, as a vendor library. The mapper cannot be a host
   service: IMapper is by definition a library loaded into each process that uses buffers. This is
   the route libui takes on every current device; nothing about any app is involved.

2. **omnidroid's device partition overlay.** omnidroid is this device's vendor: its HAL
   declarations and in-process HAL libraries are files added over the pinned image, never edits of
   the image. They live in `crates/omni-linux/device/` at their guest paths, are compiled into the
   crate (`include_bytes!`), and `Sysroot` shows them as sysroot files (stat, open, `mmap`,
   directory listings), backed by content-addressed host files it writes once. The pinned image and
   its manifest hash are unchanged. D2 adds:
   - `/vendor/etc/vintf/manifest/omni-graphics.xml`: `android.hardware.graphics.allocator`
     version 2, `IAllocator/default` (AIDL).
   - `/vendor/lib64/hw/mapper.omni.so`: built from `device/src/mapper.c` with the NDK (r28c,
     `aarch64-linux-android35-clang`), committed with its build line and sha256, as the test
     fixtures are. It links only libc and liblog.
   The ranchu HIDL declaration stays; libui never reaches it once gralloc 5 loads, and its service
   is not started.

3. **One `crate::shm` region per buffer.** The host allocator makes an `Shm` per buffer and sends
   it as the handle's one file descriptor. Page 0 of the region is a **metadata page** (the
   mutable metadata: dataspace, blend mode, crop, name, HDR blobs), shared by every process that
   maps the buffer because it is the same memory; pixels start at offset 4096. The immutable
   description rides in the handle's ints, so a process can validate a handle before mapping it:

   | int | meaning |
   |---|---|
   | 0 | magic `'OMGB'` (0x42474d4f) |
   | 1 | layout version (1) |
   | 2, 3 | width, height |
   | 4 | layer count |
   | 5 | pixel format (`PixelFormat`) |
   | 6, 7 | usage, low and high 32 bits |
   | 8 | stride, in pixels |
   | 9, 10 | buffer id, low and high 32 bits (host-assigned, unique for the broker's life) |
   | 11, 12 | pixel bytes, low and high 32 bits |
   | 13 | offset of the pixels in the region (4096) |

   `mapper.c` and `hal/gralloc.rs` both define this table; each cites the other.

4. **Formats.** Single-plane formats only: `RGBA_8888`, `RGBX_8888`, `BGRA_8888`, `RGB_888`,
   `RGB_565`, `RGBA_FP16`, `RGBA_1010102`, `R_8`, and `BLOB` (width = bytes, height 1). Stride is the
   width rounded up to 16 pixels (BLOB: the width). Anything else -- YUV formats, protected content,
   more than one layer -- `isSupported` answers false and `allocate2` raises
   `AllocationError.UNSUPPORTED`; the planes of YUV come when a caller needs them, not before.

5. **The broker's host services see objects, in both directions.** A host handler receives a
   `HostCall` (code, data, the file descriptors and binder handles it carried, the sender's pid and
   euid) and returns a `HostReply` (data plus the file descriptors to attach, by offset). Incoming
   binder objects are translated for `HOST` as for any receiver (a handle in the host's table), so
   D3's composer can take IComposerCallback binders; incoming fds arrive as `Arc<OpenFile>`. D1's
   bytes-only `create_host_service` stays as a wrapper.

6. **HAL binders are VINTF-stable.** `add_service` takes the stability it writes after the binder:
   `SYSTEM` for D1's echo, `VINTF` (`0b111111`) for a HAL, so `servicemanager` checks the VINTF
   declaration and system and vendor clients alike accept it.

7. **What the allocator answers** (`android.hardware.graphics.allocator.IAllocator`, V2, hash
   `9499fec09c544e9de5be3c87125721600f8ade66`): `allocate` (1) raises `UNSUPPORTED` (it takes a
   mapper@4 descriptor, which gralloc 5 never uses); `allocate2` (2), `isSupported` (3),
   `getIMapperLibrarySuffix` (4) = `"omni"`; `getInterfaceVersion` (`0xFFFFFF`) = 2,
   `getInterfaceHash` (`0xFFFFFE`). A parcel it cannot read is `EX_ILLEGAL_ARGUMENT`, never a panic
   (Global Constraint 11). Exact parcel bytes are checked against what the guest's libbinder writes,
   captured in the gate, not guessed.

8. **The mapper** implements every AIMapper v5 entry: import (validate, clone, `mmap` the whole
   region `MAP_SHARED`), free, transport size, lock (returns the pixel address; the region is
   coherent, so lock, unlock, flush and reread have nothing to copy -- an acquire fence is waited on
   with `poll`), the 23 standard metadata types (get; set for the mutable ones) in the stable-C
   encoding of `IMapperMetadataTypes.h`, the supported-types list, dump and dump-all, and a zero
   reserved region.

## The gate (`tests/d2_gralloc.rs`)

Start `linkerconfig` and the real `servicemanager`; register the host allocator. Run the NDK
fixture `gralloc` (the real `libnativewindow` -> libui -> gralloc 5 path): it allocates a 64x32
`RGBA_8888` `AHardwareBuffer` with CPU read/write usage, locks it, writes a pattern, unlocks, prints
its id and stride, then locks it for reading and checks the pattern. The host then finds that id in
its allocator and reads the pattern out of the buffer's `Shm` at the pixel offset: the guest wrote
it through its mapping, the host read it through its own. Failing first: no allocator is declared,
so libui has no gralloc and `AHardwareBuffer_allocate` fails.

Unit tests (`tests/binder_host.rs`): a host service replies with a file descriptor that the guest
receives as a new descriptor on the same `Shm`; a guest sends a file descriptor and a binder to a
host service, which receives the `OpenFile` and a handle.

## Next

D3 uses this: SurfaceFlinger's and the composer's buffers are these regions, and the host composer
reads a client target's pixels straight out of its `Shm`.
