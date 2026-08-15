# Custom loading screen (boot animation)

Goal: both bases boot with the Omni loading screen and **never** show a Bliss or
LineageOS logo.

| Base | Custom loading screen? |
|---|---|
| `x86` (Bliss) | **Yes** — baked into `/system/media/bootanimation.zip` when base_x86 v2 was built (`configs/paths.json` → `bases.x86.changelog["2"]`). |
| `arm` (LineageOS 23.2) | **Built + boot-verified** (`omnidroid brand-base` → `base_arm_branded.qcow2`). **Not yet swapped into the live base** — see "Remaining step". |
| `dev` (arm + devkit) | **DONE + boot-verified, live.** `base_arm_devsystem.qcow2` was branded in place (`.bak` kept). |

A dev boot now goes **black → Omni loading animation → Android**: no LineageOS
logo, no boot menu, no countdown, no kernel log, no GRUB chatter.

> **`dev` does NOT inherit `arm`'s branding — it needs its own `brand-base` run.**
> Both bases name the same `base_disk` (`base_arm.qcow2`), which makes it look
> like branding the base covers both. It does not:
>
> | image | size | backing |
> |---|---|---|
> | `base_arm_system.qcow2` (prod) | 7 MiB | → `base_arm.qcow2` (thin COW, inherits) |
> | `base_arm_devsystem.qcow2` (dev) | 1.08 GiB | **none — standalone** |
>
> `build-dev-base --patch-boot` builds the dev system through a raw export →
> re-import cycle, which FLATTENS it: it carries its own copy of every partition
> and shadows the shared base entirely. `brand-base` detects this
> (`_brand_target` checks for a backing file) and brands the standalone image
> directly. Rebasing a dev overlay onto a branded base is a silent no-op.

## How it works: `omnidroid brand-base`

```bash
omnidroid brand-base                          # -> base_arm_branded.qcow2 (safe, new file)
omnidroid brand-base --animation my.zip       # bake different art
omnidroid brand-base --in-place               # overwrite the base, keeping a .bak
```

Build-machine command (like `build-dev-base`). Needs **e2fsprogs** (`brew install
e2fsprogs` / `apt install e2fsprogs`) and ~6 GiB scratch.

Verified facts it is built on — all measured on this base, not assumed:

- LineageOS 23's `BootAnimation.cpp` searches only:
  `/apex/com.android.bootanimation/etc/…` → `/product/media/bootanimation.zip` →
  `/oem/media/…` → `/system/media/…`.
  **`/data/local/bootanimation.zip` (`USER_BOOTANIMATION_FILE`) does not exist
  anymore.** It is what every guide online still recommends; it was removed from
  LineageOS years ago. Pushing there over adb is a silent no-op on this base — do
  not "fix" this by going back to it.
- This base *has* `/product/media/bootanimation.zip` (the stock LineageOS one:
  361 STORED entries, `600 200 60`, 3 parts, 1,034,095 bytes).
- `product` is a **single linear extent** inside the `super` dynamic partition
  (vda2) at `+0x11300000`, a plain ext4 (1471 MiB, no verity/shared-blocks). The
  disk has **no `vbmeta` partition**, so AVB does not reject the edit.
- No mounting is involved (macOS cannot mount ext4; mounting would need root):
  `debugfs` edits the filesystem in place via its `?offset=` syntax, addressed
  straight into the raw disk export.

The **SELinux label is load-bearing**: the replacement is written back with
`security.selinux = u:object_r:system_file:s0` and mode 0644 root:root. `bootanim`
reads the file as a confined domain, so an unlabelled replacement is unreadable
and the screen simply stays black. `brand-base` verifies the label, re-reads the
file out of the image, and runs `e2fsck -fn` before emitting anything.

## Boot-verified

Booted on a real account (`qemu-img rebase -u` onto the branded disk, HVF):

| t | what is on screen |
|---|---|
| 0 – ~7.9 s | **kernel console text** (see the open gap below) |
| ~8.0 s | SurfaceFlinger starts and takes the framebuffer |
| **~8.1 s onward** | **the Omni loading animation renders** |

Proven with a deliberately unmistakable test animation first (full-screen magenta
+ a moving green bar: 50 frames of it captured on screen from t=8.14 s), so
"black screen" could never be mistaken for success. The shipped art was then
verified the same way.

> The first attempt looked like a failure — a black screen. The cause was that
> `assets/loading/bootanimation.zip` was a **near-black placeholder** from
> `tools/gen_placeholder_frames.ps1` (mean RGB 0.6/0.9/1.1). The mechanism had
> been working the whole time; there was simply nothing to see. The asset is now
> a real, brand-neutral animation (dark field + orbiting arc, 60 frames @30fps).
> **Replace it with real art** via `--animation`; it is deliberately plain, not a
> design proposal.

Note the animation is invisible to the auto-screenshot recorder *by design*: it
is a spinner, and the change detector is tuned to ignore exactly that (see
`manager/capture.py`). That is correct behaviour, not a missed frame.

## Remaining step (needs a decision)

`base_arm.qcow2` is **untouched**; the branded image is a separate file. Swapping
it is deliberately not automatic:

- it is a 2.1 GiB shared base that every existing account's `system.qcow2` is
  COW-backed by, and
- it lives in Syncthing-managed `~/Desktop/OmniImages`, so overwriting it costs a
  full re-sync.

The edit itself is safe for existing accounts — `/product` is mounted read-only,
so no overlay has ever written those blocks, and they read the new content
through. When you want it live:

```bash
omnidroid brand-base --in-place        # keeps base_arm.qcow2.bak
```

## Silent boot on arm — also done

The arm base used to render **scrolling kernel/init log text on the framebuffer**
for the first ~8 s of every boot. That defeats "no vendor logo" harder than a
logo does.

x86 solves it on the QEMU command line; arm cannot, because it boots UEFI → GRUB
→ kernel from *inside* the image, so there is no `-append` to add. `brand-base`
therefore patches the kernel cmdline in **grub.cfg on the ESP (vda1, FAT32)**,
which macOS mounts natively via `hdiutil` — no root, no mtools:

```
linux ${boot_partition}/kernel … ${kernel_cmdline_dynamic} $@ quiet loglevel=0 vt.global_cursor_default=0
```

Only the **normal-boot** line is touched; the recovery line stays verbose, since
a silent recovery is a debugging own-goal. Skip it with `--no-silent-boot`.

### The boot MENU — the part you only see by looking

Quieting the kernel is not enough. **Before the kernel loads at all, GRUB draws a
themed menu**: the LineageOS logo, four entries (`LineageOS 23.2` / `Recovery` /
`Settings` / `Advanced options`), and **"Booting in 10 seconds"**. That is both a
vendor logo *and* a menu — the two things this product must never show — and it
accounted for a flat 10-second gap in every boot timeline.

No amount of `quiet` touches it, because it is GRUB's own UI. `brand-base` forces
GRUB straight through:

```
set timeout=0
set timeout_style=hidden
```

`set default=` is untouched, so a misc-triggered recovery boot still selects the
recovery entry — it just does not wait. With the menu hidden the gfxmenu theme
never renders, so the logo never appears. The `echo 'Loading kernel...'` lines in
`boot_android` are dropped too (recovery keeps its echoes).

### Measured on real boots

| | frames showing a logo / menu / console text |
|---|---|
| stock arm base | ~119 (scrolling init log) + a 10 s LineageOS menu |
| after `brand-base` | **0 during boot** |

Boot is now: black → **Omni loading animation** (t≈8.6 s on dev) → Android.
Two residual `error: serial port 'auto' isn't found` lines from a GRUB hook are
the only text left, and they no longer render before the animation.

> This is a macOS-only step today (`hdiutil`). On another build host it is
> skipped with a warning rather than failing the command; the animation still
> gets baked. A Linux build host needs mtools or a loop mount here.

## Silent launch (separate, and already done)

No terminal/console window ever appears on the host, on either base:

- QEMU is `-display none` **always**; the VNC server is an attach point, never a
  window (`qemu_command` / `qemu_command_arm`).
- The host spawns it detached — `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` on
  Windows, `start_new_session=True` elsewhere (`spawn_qemu`).
- omni-executor spawns helpers with `CREATE_NO_WINDOW`.
