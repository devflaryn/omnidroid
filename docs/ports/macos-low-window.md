# macOS: ART's low 4 GiB as a based window (D41)

The real-AOSP path (`omni-linux`, D39) runs ART, and ART keeps its heap and boot image below 4 GiB:
object references are 32 bits wide and are decompressed by zero-extension, and the boot image goes
near `ART_BASE_ADDRESS` (0x7000_0000). Windows and Linux reserve the guest space at `0x1000_0000`
(`omni_linux::process::reserve_space`), and because a guest address is a host address (D4), the
guest's low 4 GiB *are* the host's.

## Why macOS cannot do that

MEASURED on the M1 (macOS 27.0, `xnu-13432`), with a probe binary:

| attempt | result |
|---|---|
| `mmap(0x1000_0000, 1 MiB, MAP_FIXED)` in a default binary | `MAP_FAILED` |
| `mach_vm_allocate(0x2000_0000, VM_FLAGS_FIXED)` | `KERN_INVALID_ADDRESS` |
| link with `-Wl,-pagezero_size,0x4000` | the process is killed at exec (137, `SIGKILL`) |

An arm64 Mach-O must have a *hard* page zero of at least 4 GiB (`load_machfile` refuses anything
less with `LOAD_BADMACHO`), and the loader raises the map's minimum address to the end of
`__PAGEZERO` (`vm_map_raise_min_offset`). Nothing can be mapped below 4 GiB by any means a user
process has. So on macOS D4's identity cannot hold for the guest's low 4 GiB, and the three ART gates
(`b_low_base`, `b_hello_dex`, `c2_apk_in_app_process`) were ignored there.

## The approach: identity above 4 GiB, a based window below

The guest space keeps its guest layout exactly as on the other hosts -- `[0x1000_0000,
0x1000_0000 + 64 GiB)` -- but on macOS its low part is **backed somewhere else in the host**:

```
guest address g                         host address
[0x1000_0000, 4 GiB - page)   ->   W + g      (W: a 4 GiB-aligned host reservation, anywhere)
[4 GiB - page, 4 GiB)         ->   (a host-owned guard page: no mapping may straddle the seam)
[4 GiB, end)                  ->   g          (identity, reserved around what the host holds)
```

`host(g) = g + (g < 4 GiB ? W : 0)`, and back: `guest(h) = h - W` for `h` in the window.

**Why not base the whole space** (`fastmem_pointer = W` for everything, the usual dynarmic setup)?
Because the host is handed guest pointers it dereferences in place: the paravirtual GPU forwards
the guest's Vulkan structs to the host driver where they lie (`gpu/mod.rs`), and D4's
"never hand the guest a host pointer" runs both ways. Everything the host reads raw is
allocated by `mmap` without an address, and the Linux personality already places such mappings
above 4 GiB, as Linux's top-down `mmap_base` does (`mm.rs`; `b_low_base` asserts it). So the window
holds only what ART asks for by address -- the Java heap, the boot image, its `low_4gb`
allocations -- and nothing the host reads raw ends up there.

### Where the translation lives

Every host path to guest memory on the AOSP side already goes through one of a few functions, so
the translation is made in those and nowhere else:

| layer | what changes |
|---|---|
| `omni-platform` | `vm::lowest_mappable_address()`: 4 GiB on macOS (the hard page zero), else the first page. The one fact the rest decides on. |
| `omni-mem` `GuestSpace` | `GuestSpaceConfig::low_window`: the part of the space below 4 GiB is reserved wherever the host chooses (4 GiB-aligned) and addressed as `W + g`. `ptr`, every `vm::` call the space makes, and `write_forced` translate; `host_to_guest` maps a host address back. Placements without an address never land in the window (the cursor starts above it). Off by default, and the translation is `+ 0` when off. |
| `omni-mem` pager | a fault's host address is translated to the guest's before the region lookup, so demand commit works in the window. |
| `omni-cpu` | dynarmic is configured with `fastmem_pointer = W` and the new `fastmem_low_window` flag; the slow-path callbacks (data, code fetch, exclusives) go through `GuestSpace::ptr`. The D4 check accepts "identity above 4 GiB, based below" only together with the flag. |
| dynarmic (patch 0030, arm64) | `FastmemEmitVAddrLookup` and `EmitExclusiveHostAddress` add the base only below 4 GiB: `tst addr, #0xffffffff00000000; csel base, Xfastmem, xzr, eq; ldr [base, addr]` -- two instructions per guest access, no branch, no callback. Off unless asked for; the x64 backend refuses the flag. |
| `omni-linux` | `reserve_space` asks for the window when `lowest_mappable_address() > 0x1000_0000`. The guest's fault address (`siginfo.si_addr`) is always the guest's. |

### What it costs, and what it does not change

* Windows and Linux: nothing. The window is off, `host(g)` is `g`, the dynarmic flag is unset and
  the emitted code is byte for byte what it was.
* macOS: two ALU instructions per guest memory access on the fast path (the M1 issues them in the
  same cycle as the address computation it already does for Top Byte Ignore). Measured in
  `macos.md` once in-world.
* One address space per host process gets the window, as on the other hosts one gets the low range
  (a second ART in the system's host process still has none -- the `svc` note in HANDOFF).
* The seam: a guest mapping cannot straddle 4 GiB, because its two halves would be at unrelated host
  addresses. A host-owned guard page at `4 GiB - page` makes such a request fail as any occupied
  range does; ART never asks for one (its `low_4gb` maps end at or below 4 GiB).

### Proof

* `omni-mem/tests/low_window.rs`: a space with the window: guest addresses below 4 GiB, host
  addresses of the window above it; a byte written through `ptr` is read back at `W + g`;
  demand commit through the pager in the window; unhinted placements land above 4 GiB; a mapping
  across the seam is refused.
* `omni-cpu` (arm64): guest code loads and stores, and an exclusive pair, at a low guest address
  reach `W + g` on the fast path with zero slow-path entries.
* `omni-linux/tests/b_low_base.rs`, un-ignored on macOS: the space starts below 4 GiB, and a real
  process's `/proc/self/maps` shows nothing unhinted there.
* `omni-linux/tests/b_hello_dex.rs` and `c2_apk_in_app_process.rs`, un-ignored on macOS: real ART
  maps its heap and boot image in the window and runs dex.
