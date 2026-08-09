# Per-instance footprint — state, evidence, and what is left

Measured 2026-08-05 on the arm64 base (LineageOS 23.2, Apple Silicon / HVF)
with the real Roblox APK. Every number here came from a live instance; none
is an estimate. Full narrative in `CHANGELOG.md`.

## Where the targets stand

| target | status |
|---|---|
| 2–3 playable instances per workstation | **met** |
| 50+ farming instances | **met on a 64 GB host** (~71) — zram is baked and verified on production |
| ~400 MB RAM per instance | **not achievable** — see below |

The arm base is at **version 3**: `persist.sys.zram_enabled=1` is baked into
`/product/etc/build.prop`, so every production instance boots with zram and
the balloon caps it at 896 MB by itself. Verified on a genuinely non-rooted
instance (`uid=shell`, no `su`): `SwapTotal` 995 MB, Roblox running, **zero
kills** — a cap that without zram killed the game outright.

### Why ~400 MB per instance cannot be reached

Roblox with its engine active is **~680 MB resident, by itself**. That is
already 1.7x the target before Android exists underneath it. The squeezed
Android adds ~620 MB, so a joined instance needs ~1.3 GB live, and the safe
host cap is 1024 MB with zram.

This is a property of the game, not of QEMU or Android. Every layer beneath
it has been tested:

| lever | result |
|---|---|
| QEMU device set | duplicate xHCI + unused virtio-serial removed |
| virtio-balloon + free-page-reporting | host RSS tracks live set, not `-m` |
| **zram (lz4, 2.97x)** | **cap 1536 → 1024 MB — the biggest single win** |
| package trim (34 pkgs) | −70 MB |
| tiny display (480x270 @ 80 dpi) | applied |
| Roblox client settings (FPS cap 5) | **CPU 36% → 18.8%**; memory unchanged |
| `ro.config.low_ram=true` | **bricks the guest** (RescueParty → recovery) |
| removing SystemUI | **bricks the guest** |
| lmkd / dexopt / bg-limit props | boot fine, save nothing |

The two "bricks the guest" rows were each isolated and reproduced; they are
recorded in `lean.py` so nobody has to re-learn them.

## The one step left: zram on production

zram is **not missing** from the base — it is switched off behind one
property. Read off a live instance:

```
/vendor/etc/fstab.virtio    /dev/block/zram0 none swap defaults zramsize=50%
/vendor/etc/init/zram.rc    on early-init -> modprobe zram.ko
                            on init       -> comp_algorithm = lz4
                            on property:persist.sys.zram_enabled=1 -> swapon_all
```

Verified: `setprop persist.sys.zram_enabled 1` → init runs `swapon_all` →
`SwapTotal` 0 → 470980 kB. It is a `persist.*` property so it is settable at
runtime, but SELinux denies uid `shell`, so production needs it baked.

### Already done — how it was baked

`base_arm_system_zram.qcow2` is a **~1 MB COW overlay** of
`base_arm_system.qcow2` (untouched), carrying one changed file. The config's
`bases.arm.system` points at it; revert by pointing it back.

`omnidroid enable-zram-base` does the same edit via a qcow2 round trip, which needs
~3.3 GiB of scratch (`--scratch-dir` can put that on another volume). With no
scratch space at all, the method actually used here works instead and costs
about a megabyte:

```sh
# 1. Thin overlay of the base's system image — a few hundred KB
qemu-img create -f qcow2 -b base_arm_system.qcow2 -F qcow2 base_arm_system_zram.qcow2

# 2. Boot the ROOTED dev system with that overlay attached as an extra disk,
#    so the guest's own kernel does the ext4 write. (virt's pcie.0 does not
#    support hotplug, so attach it at boot, not via device_add.)
#      -device virtio-blk-pci,drive=vdd
#      -drive file=base_arm_system_zram.qcow2,if=none,id=vdd,cache=writeback

# 3. In the guest, as root: super is the GPT partition at sector 526336, and
#    `product` sits 18874368 bytes into it. Do NOT pass a sizelimit — the
#    ext4 is a few blocks larger than the liblp record, and the kernel then
#    refuses with "bad geometry: block count N exceeds size of device".
losetup -f --show -o 18874368 /dev/block/vdd2      # toybox uses -S, not --sizelimit
mount -t ext4 -o rw /dev/block/loopN /mnt/omniedit
#    merge (do not blindly append), then restore mode 600, owner 0:0; the
#    SELinux context is inherited correctly from the existing file
printf '\npersist.sys.zram_enabled=1\n' >> /mnt/omniedit/etc/build.prop
sync; umount /mnt/omniedit
```

Acceptance test on a production instance:

```sh
omnidroid start <name> --mode farming
adb -s 127.0.0.1:<adb_port> shell grep SwapTotal /proc/meminfo   # must be > 0
```

Note `getprop persist.sys.zram_enabled` reads EMPTY from uid `shell` — the
same SELinux restriction that blocks writing it also blocks reading it. Use
`SwapTotal`, not `getprop`, to check.

**Today, without freeing space:** on the rooted dev base the engine already
does this per boot (`enable_zram()`), so dev instances get the 1024 MB cap now.

The scratch requirement is ~3.3 GiB, not the 6 GiB a flat constant used to
claim, and it does not have to be on the same disk as the images
(`--scratch-dir`) — it holds only temporary state. Reasons: peak usage is the raw export plus a THIN overlay (only the clusters
that differ from the original are stored, which for a one-property build.prop
edit is a handful), so the output is hundreds of KB rather than another full
2.3 GiB image. Same shape the project already uses — `base_arm_system.qcow2`
is itself a 7.5 MiB overlay on the shared base.

## zstd was tried and is WORSE — do not retry on one vCPU

`/vendor/etc/init/zram.rc` selects **lz4**. zstd compresses better (~4x vs
~3x), and "quality, speed doesn't matter" appears to license the trade, so it
was baked in and measured. Result:

| | lz4 | zstd |
|---|---|---|
| 896 MB cap, game running | 0 kills, sustained 3+ min | **3 mem-pressure kills, game dead** |
| boot time | 0.8 min | 5+ min (timed out `omnidroid start`) |

zstd wins on compression RATIO and loses on THROUGHPUT. On one vCPU it is
reclaim LATENCY that decides whether an instance survives pressure: the guest
cannot free pages fast enough, lmkd sees sustained pressure, and the game is
what it kills. "Speed doesn't matter" applies to the game's frame rate, not
to how fast the kernel can reclaim memory.

Reverted. Worth retrying only with more vCPUs per instance, which trades away
the thing farming mode exists to conserve.

## Tuning density vs staying up

The default caps favour STAYING UP, because the stated requirement is "the
instances must be on" — an OOM-killed game is an instance that is off.

| cap | zram | measured on a real instance, game running |
|---|---|---|
| 1536 MB | no | **default without zram.** 336 MB headroom |
| 1280 MB | no | holds, 0 kills, 117 MB headroom — tight |
| 1024 MB | no | **kills the game** (`mem-pressure-event`) |
| 1024 MB | yes | holds comfortably, ~300 MB headroom |
| **896 MB** | yes | **default with zram.** ~200 MB RSS, ~590 MB swapped, 0 kills, 85–131 MB headroom, sustained 3+ min |
| 768 MB | yes | holds, 0 kills, but only 34 MB headroom |
| 640 MB | yes | **kills the game** (2 mem-pressure kills) |

896 rather than 768 because "only the instances must be on" is the stated
requirement and 34 MB of headroom is luck, not margin. `--balloon` is now
honoured exactly if you want a different point on this ladder — it used to be
silently overridden by the zram cap.

1280-without-zram became possible only after the 34-package trim landed; the
original 1536 was measured before it. Take it with `--balloon 1280` if you
want ~17% more instances and can tolerate the margin. Every number above was
measured against the game on its login screen, not joined to a place, so a
joined instance has less headroom than these suggest.

## Capacity arithmetic

Planning is against the balloon **cap**, never observed RSS — an idle fleet
flatters the number, and `omnidroid measure` enforces this.

| host | instances at 896 MB (2 GiB reserved) |
|---|---|
| 16 GB workstation | ~16 |
| 64 GB server | **~71** |
| 128 GB server | ~144 |

50+ needs a 64 GB host. It is not reachable on a 16 GB workstation at any
per-instance size, because 50 × 400 MB = 20 GB exceeds the machine.

## Two things still unverified, and why

- **KSM dedup.** 50 guests from one base hold overwhelmingly identical
  pages; KSM collapses them, so a large fleet costs less than the sum of its
  instances. `omnidroid measure` reports `ksm_merged_mb` per instance and
  fleet-wide `saved_mb`. Linux-only — unverifiable on macOS. Turn it on with
  `omnidroid ksm --on`.
- **A genuinely joined instance.** Without a live Roblox cookie the game sits
  on its login screen; the kiosk's Lock Task whitelist is `[com.omni.kiosk]`
  until a session is delivered. All game numbers here are foreground-with-
  engine-up, which is the right proxy, but a joined instance has not been
  measured.

## Correction (2026-08-06): the "flagged, black-screening" APK was not the APK

Earlier notes here and in the B2 plan recorded that the pre-installed Roblox
"is flagged and black-screens", and told the reader to sideload a bootstrap
APK to measure anything. That diagnosis was wrong.

The black screen was `farming.build_client_settings_script` creating
`/data/data/com.roblox.client/files` as **root:root** (its `mkdir -p` ran as
root and only the leaf `ClientSettings/` was chowned back). The game could not
write its own `files/` dir, failed to initialise, and left the foreground. With
the ownership fixed, the SAME baked APK logs in and renders its home screen,
and `topResumedActivity` is `com.roblox.client/.ActivityNativeMain`.

It does still show Roblox's own "your version is out of date" dialog — a real
but ordinary update, handled by `omnidroid bake-data-game <newer.apk>` without
rebuilding any base. See the 2026-08-06 CHANGELOG entries.

## Platform note

balloon and free-page-reporting decommit for real on Linux/KVM. On macOS/HVF
QEMU's `madvise` is advisory — a balloon inflate left host RSS high and
rising. The 50+ story is a Linux number; macOS runs the 2–3 playable
instances. `omnidroid measure` prints which regime it is in.
