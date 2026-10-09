//! **Measurement levers switched while an instance runs**, so an A/B is taken inside one live
//! session -- the same world, the same players, the same host load -- phase against phase, rather
//! than across boots minutes apart (docs/NIGHT-2026-10-02.md, `tools/perf_live.ps1`).
//!
//! `OMNI_LEVER_FILE=<path>`: every host process of the instance reads the file four times a second;
//! when its text changes, each `name=value` line is applied and said on stderr (`[lever]`). Levers:
//!
//! - `jit_fp=<hex mask>`: dynarmic's unsafe floating-point flags
//!   (`omni_cpu::dynarmic::set_live_fp_optimizations`), then every process's translations are
//!   dropped so what runs next is translated with them. 0 is the accurate default.
//! - `jit_getset=0|1`: dynarmic's `GetSetElimination` in the form precise at every guest memory
//!   access (patch 0037, `omni_cpu::dynarmic::set_precise_get_set`), then every process's
//!   translations are dropped. 1 is the default; 0 is upstream's behaviour under the memory-abort
//!   check (every guest register read a load from `JitState`, every write a store).
//! - `jit_fpxmm=0|1`: scalar floating-point operands kept in XMM registers rather than copied
//!   through a general register and back (patch 0039, `omni_cpu::dynarmic::set_scalar_fp_in_xmm`;
//!   bit-identical values, ~2x on dependent `FADD`/`FMUL` chains), then every process's
//!   translations are dropped. On by default in omni-linux (`process::scalar_fp_in_xmm_default`); `OMNI_JIT_SCALAR_FP_XMM=0` turns it off from the start.
//! - `jit_tbiand=0|1`: Top Byte Ignore's mask on every direct guest access as one `and` against a
//!   constant rather than a `shl`/`shr` pair (patch 0040, `omni_cpu::dynarmic::
//!   set_fastmem_mask_by_and`; the same address), then every process's translations are dropped.
//!   Off by default; `OMNI_JIT_TBI_AND=1` from the start.
//! - `jit_fastdisp=0|1`: the return-stack buffer's and fast-dispatch table's hit paths inside each
//!   `RET`/`BR`/`BLR` block (patch 0042, `omni_cpu::dynarmic::set_fast_dispatch_inline`): same
//!   lookups and checks, the target from a register and a host indirect jump per site; ~20% on
//!   call/return chains. Every process's translations are dropped. Off by default;
//!   `OMNI_JIT_FASTDISP=1` from the start.
//! - `jit_tbi=0|1`: 0 takes Top Byte Ignore's mask off the direct path (patches 0040/0041,
//!   `omni_cpu::dynarmic::set_tbi_unmasked`): an untagged access is D4's identity; a tagged one is
//!   a host fault served by the slow path (~2.4 us), and its instruction then learns the mask
//!   (translated again, masked) -- scudo's `0x02` chunk-header tag comes from a few dozen
//!   instructions in `libc.so`, so a process pays ~280 faults once (`[tbi]` every 30 s counts them
//!   and the learned instructions). 1, the default, puts the mask back everywhere. Every process's
//!   translations are dropped. (`OMNI_JIT_TBI=0` does the same from the start:
//!   `crate::process::tbi_direct_mask`.)
//!
//! - `compose_fast=0|1`: the composer's fast path (`crate::hal::compose::FAST`: the same pixels
//!   in fewer passes, its buffers kept from frame to frame). On by default.
//! - `compose_zero=0|1`: the composer's layers read where they are in their gralloc regions and
//!   composed straight into the framebuffer's next frame, by the run path
//!   (`crate::hal::compose::ZERO`): two to three fewer 5.6 MB copies a frame. Off by default.
//! - `present_bgra=0|1`: the frame composed as BGRA, and the Win32 window takes it shared rather
//!   than swizzling a copy (`crate::hal::compose::BGRA_OUT`). Off by default.
//! - `present_gpu=0|1`: the display window presented through a Vulkan swapchain (a copy into
//!   host-visible memory, the scaling a GPU blit) instead of GDI's `StretchDIBits`
//!   (`crate::gpu::window_present`). Off by default.
//! - `release_wait=spin|poll` (and `release_poll=<us>`, which also means `poll`): how the host waits
//!   for its own fences -- the release worker's copy, a sync-file export -- the driver's wait (the
//!   default) or asked every `<us>` (100 by default) with a sleep between
//!   (`crate::gpu::native::RELEASE_WAIT_US`; `event` measured not available).
//! - `gralloc_direct=0|1`: an app's released frame copied by the GPU straight into its gralloc
//!   region (imported as Vulkan memory) instead of into a staging buffer the release worker then
//!   copies on the CPU (`crate::gpu::native::DIRECT`). Needs devices made with
//!   `OMNI_GRALLOC_DIRECT=ready` (or `=1`, on from the start). Off by default.
//! - `fence_poll=<microseconds>`: a guest `vkWaitForFences` polled with that period instead of the
//!   host driver's own (spinning, on NVIDIA) wait (`crate::gpu::FENCE_POLL_US`). 0 is the driver's.
//! - `vsync_hz=<n>`: the composer's vsync rate, 1..=1000 (`crate::hal::composer::VSYNC_PERIOD_NS`;
//!   60 by default, `OMNI_VSYNC_HZ` from boot). To find out whether vsync pacing is the ceiling.
//! - `vsync_pace=0|1`: vsync paced to absolute deadlines (1, the default) or the old
//!   `sleep(period)` loop that drifted to ~58.8 Hz (0) (`crate::hal::composer::VSYNC_PACE`).
//! - `composer_skip_validate=0|1`: a frame the composer composes is presented at
//!   `presentOrValidateDisplay` (1; one `executeCommands` a frame instead of two) or at the
//!   separate `presentDisplay` (0, the default) (`crate::hal::composer::SKIP_VALIDATE`).
//! - `composer_fences=0|1|2`: no fences (0, the default), a present fence (1), present and
//!   release fences (2) from the composer (`crate::hal::composer::FENCES`).
//! - `poll_keyed=0|1`: waits on descriptors that had no key (constant-readiness files, netlink
//!   and other self-answered sockets, unix `accept`, host sockets' waits, `/dev/fuse`, `epoll_ctl`)
//!   are woken by their own changes only (1), not by every change in the host process (0, the
//!   default) (`crate::poll::KEYED`; `OMNI_POLL_STATS` counts what is left).
//! - `remote_direct=0|1`: the system's host process reads and writes an app's guest memory itself
//!   (1, the default) rather than over the app thread's connection (0; `OMNI_REMOTE_DIRECT=0`)
//!   (`crate::remote::DIRECT`; `OMNI_REMOTE_STATS` counts both).
//! - `binder_host_pool=0|1`: a host service's binder calls run on kept, reused threads (1) or on a
//!   new thread each (0, the default; `OMNI_BINDER_HOST_POOL`) (`crate::binder::HOST_POOL`).
//!
//! - `timer_ms=<n>`: this host process holds a host timer resolution of `n` ms
//!   (`omni_platform::clock::TimerResolution`, `timeBeginPeriod`); 0 gives it back. On Windows every
//!   timed wait -- a guest `futex` with a timeout, `nanosleep`, a condition variable's `wait_for` --
//!   is otherwise rounded up to the ~15.6 ms scheduler tick.
//! - `qos=high|auto`: `high` opts the host process out of Windows' power throttling (EcoQoS) and of
//!   its rule that a windowless process's timer resolution is not honoured
//!   (`omni_platform::clock::allow_power_throttling`); `auto` gives the choice back to the host.
//! - `poll_slice_ms=<ms>`: the longest a poll-family wait sleeps before looking again by itself
//!   (`crate::poll::SLICE_MS`, 1000 by default); a posted signal wakes the waiters directly.
//! - `futex_herd=0|1`: 1 makes every futex wake unpark every task waiting in the process (the old
//!   one-condition-variable behaviour, `crate::futex::HERD`), to A/B the per-task wake against it.
//! - `vk_fast=0|1`: the Vulkan forwarding's fast path (`crate::gpu::FAST`: no allocation per
//!   command, no write of a zero result). Off by default; `OMNI_VK_FAST=1` from the start.
//! - `vk_handles=0|1`: dispatchable handles from a per-thread cache (`crate::gpu::HANDLE_CACHE`).
//!   Off by default; `OMNI_VK_HANDLES=1`.
//! - `vk_inline=0|1`: the guest's Vulkan driver sends each unbatched command with its arguments
//!   inline (`crate::gpu::INLINE`), from its next config refresh (at most every 250 ms, at a
//!   `vkBeginCommandBuffer`). Off by default; `OMNI_VK_INLINE=1`.
//! - `vk_batch=0|1`: the guest's Vulkan driver batches the commands that only record into a command
//!   buffer (`crate::gpu::BATCH`), from a command buffer's `vkBeginCommandBuffer` after its next
//!   config refresh. Independent of the others. Off by default; `OMNI_VK_BATCH=1` from the start.
//!
//! - `zero_reclaim=<seconds>`: sweep every guest process's resident pages of zeros out of the
//!   working set that often (`crate::zero_reclaim`; RAM, not commit). 0, the default, stops it.
//! - `madv_free=0|1`: a guest `madvise(MADV_FREE)` carried out as `MADV_DONTNEED` -- the range
//!   decommitted at once, which Linux's "old contents or zeros" allows -- rather than ignored
//!   (`crate::mm::MADV_FREE_DISCARDS`). Off by default.
//!
//! - `read_no_commit=0|1`: the kernel's reads of guest memory read a lazy mapping's uncommitted
//!   pages as zeros instead of committing them (`crate::guest::READ_NO_COMMIT`; on by default).
//! - `binder_spawn=kernel|eager`: when a guest process is asked for another binder looper -- the
//!   kernel's rule (no other looper waits idle) or the eager one this driver had (the default).
//!   Threads already spawned stay; see `crate::binder::spawn` (also `OMNI_BINDER_MAX_LOOPERS`).
//!
//! `OMNI_JIT_UNSAFE_FP=<hex mask>` sets the same flags from the start, without a file;
//! `OMNI_ZERO_RECLAIM=<seconds>` and `OMNI_MADV_FREE=1` the last two.
use std::path::PathBuf;
use std::time::Duration;

/// `release_wait=poll`'s period when `release_poll` has not set one: 100 us (measured: +0.31 ms of
/// latency a wait, `crate::gpu::native::RELEASE_WAIT_US`).
const RELEASE_POLL_DEFAULT_US: u32 = 100;

/// How often the file is read.
pub const POLL: Duration = Duration::from_millis(250);

fn parse_hex(v: &str) -> Option<u32> {
    u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok()
}

/// Apply one `name=value` line; what was done, for the log, or `None` for a line not understood.
pub fn apply(line: &str) -> Option<String> {
    let (name, value) = line.split_once('=')?;
    match name.trim() {
        "jit_fp" => {
            let kept = omni_cpu::dynarmic::set_live_fp_optimizations(parse_hex(value)?);
            let live = crate::process::all_live();
            for p in &live {
                p.trim_code();
            }
            Some(format!("jit_fp={kept:#x}: {} processes' translations dropped", live.len()))
        }
        "jit_fpxmm" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            let kept = omni_cpu::dynarmic::set_scalar_fp_in_xmm(on);
            let live = crate::process::all_live();
            for p in &live {
                p.trim_code();
            }
            Some(format!("jit_fpxmm={}: {} processes' translations dropped", u8::from(kept), live.len()))
        }
        "jit_fastdisp" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            let kept = omni_cpu::dynarmic::set_fast_dispatch_inline(on);
            let live = crate::process::all_live();
            for p in &live {
                p.trim_code();
            }
            Some(format!("jit_fastdisp={}: {} processes' translations dropped", u8::from(kept), live.len()))
        }
        "jit_tbi" => {
            // 1 is the mask on the direct path (the default); 0 takes it off, live.
            let masked = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            let unmasked = omni_cpu::dynarmic::set_tbi_unmasked(!masked);
            let live = crate::process::all_live();
            for p in &live {
                p.trim_code();
            }
            Some(format!(
                "jit_tbi={}: {} processes' translations dropped ({} tagged accesses served by the slow path so far)",
                u8::from(!unmasked),
                live.len(),
                omni_cpu::dynarmic::tagged_accesses()
            ))
        }
        "jit_tbiand" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            let kept = omni_cpu::dynarmic::set_fastmem_mask_by_and(on);
            let live = crate::process::all_live();
            for p in &live {
                p.trim_code();
            }
            Some(format!("jit_tbiand={}: {} processes' translations dropped", u8::from(kept), live.len()))
        }
        "jit_getset" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            let kept = omni_cpu::dynarmic::set_precise_get_set(on);
            let live = crate::process::all_live();
            for p in &live {
                p.trim_code();
            }
            Some(format!("jit_getset={}: {} processes' translations dropped", u8::from(kept), live.len()))
        }
        "compose_fast" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::hal::compose::FAST.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("compose_fast={}", u8::from(on)))
        }
        "compose_zero" | "present_bgra" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::hal::compose::levers_from_env();
            let lever = if name.trim() == "compose_zero" { &crate::hal::compose::ZERO } else { &crate::hal::compose::BGRA_OUT };
            lever.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("{}={}", name.trim(), u8::from(on)))
        }
        "vsync_hz" => {
            let hz: u32 = value.trim().parse().ok()?;
            let period = crate::hal::composer::set_vsync_hz(hz)?;
            Some(format!("vsync_hz={hz}: period {period} ns"))
        }
        "vsync_pace" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::hal::composer::VSYNC_PACE.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("vsync_pace={}: {}", u8::from(on), if on { "deadlines" } else { "the old sleep(period) loop" }))
        }
        "composer_skip_validate" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::hal::composer::SKIP_VALIDATE.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("composer_skip_validate={}: {}", u8::from(on), if on { "a frame of the composer's is presented at presentOrValidate" } else { "presentOrValidate answers Validated, presentDisplay presents" }))
        }
        "composer_fences" => {
            let n: u8 = value.trim().parse().ok().filter(|n| *n <= 2)?;
            crate::hal::composer::FENCES.store(n, std::sync::atomic::Ordering::Relaxed);
            Some(format!("composer_fences={n}: {}", ["no fences", "a present fence", "present and release fences"][usize::from(n)]))
        }
        "poll_slice_ms" => {
            let ms: u64 = value.trim().parse().ok()?;
            crate::poll::SLICE_MS.store(ms.max(1), std::sync::atomic::Ordering::Relaxed);
            Some(format!("poll_slice_ms={}: a poll-family wait looks again by itself every {} ms", ms.max(1), ms.max(1)))
        }
        "remote_direct" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::remote::DIRECT.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("remote_direct={}: apps' memory {}", u8::from(on), if on { "read and written directly, the connection where that cannot be" } else { "asked for over each thread's connection" }))
        }
        "poll_keyed" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::poll::KEYED.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("poll_keyed={}: {}", u8::from(on), if on { "fewer waits on anything" } else { "unkeyed descriptors wait on anything" }))
        }
        "binder_host_pool" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::binder::HOST_POOL.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("binder_host_pool={}: host services' calls run on {}", u8::from(on), if on { "kept threads" } else { "a new thread each" }))
        }
        "vk_fast" | "vk_batch" | "vk_handles" | "vk_inline" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            match name.trim() {
                "vk_fast" => crate::gpu::set_fast(on),
                "vk_batch" => crate::gpu::set_batch(on),
                "vk_handles" => crate::gpu::set_handle_cache(on),
                _ => crate::gpu::set_inline(on),
            }
            Some(format!("{}={}", name.trim(), u8::from(on)))
        }
        "zero_reclaim" => {
            let seconds: u64 = value.trim().parse().ok()?;
            crate::zero_reclaim::set_period(seconds);
            Some(format!("zero_reclaim={seconds}: {}", if seconds == 0 { "no sweeps" } else { "zero pages swept out of the working set" }))
        }
        "read_no_commit" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::guest::READ_NO_COMMIT.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("read_no_commit={}", u8::from(on)))
        }
        "binder_spawn" => {
            let kernel = match value.trim() {
                "kernel" => true,
                "eager" => false,
                _ => return None,
            };
            crate::binder::spawn::KERNEL_RULE.store(kernel, std::sync::atomic::Ordering::Relaxed);
            Some(format!("binder_spawn={}: a looper is asked for {}", value.trim(), if kernel { "only when none waits idle" } else { "whenever one takes work" }))
        }
        "madv_free" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::mm::MADV_FREE_DISCARDS.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("madv_free={}: MADV_FREE {}", u8::from(on), if on { "discards the range" } else { "is a hint" }))
        }
        "release_wait" => {
            use std::sync::atomic::Ordering::Relaxed;
            let w = &crate::gpu::native::RELEASE_WAIT_US;
            match value.trim() {
                "spin" => w.store(0, Relaxed),
                "poll" => {
                    if w.load(Relaxed) == 0 {
                        w.store(RELEASE_POLL_DEFAULT_US, Relaxed);
                    }
                }
                "event" => return Some("release_wait=event: not available (an exported fence's Win32 handle is no event on this driver; measured): unchanged".into()),
                _ => return None,
            }
            let us = w.load(Relaxed);
            Some(if us == 0 { "release_wait=spin: the host driver's own fence wait".into() } else { format!("release_wait=poll: the fence asked every {us} us") })
        }
        "release_poll" => {
            let us: u32 = value.trim().parse().ok().filter(|&us| us > 0)?;
            crate::gpu::native::RELEASE_WAIT_US.store(us, std::sync::atomic::Ordering::Relaxed);
            Some(format!("release_poll={us}: release_wait=poll, the fence asked every {us} us"))
        }
        "gralloc_direct" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            let _ = crate::gpu::native::direct_wanted();
            crate::gpu::native::DIRECT.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("gralloc_direct={}", u8::from(on)))
        }
        "present_gpu" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            let _ = crate::gpu::window_present::on();
            crate::gpu::window_present::ON.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("present_gpu={}", u8::from(on)))
        }
        "fence_poll" => {
            let us: u32 = value.trim().parse().ok()?;
            crate::gpu::FENCE_POLL_US.store(us, std::sync::atomic::Ordering::Relaxed);
            Some(format!("fence_poll={us}: a guest vkWaitForFences {}", if us == 0 { "is the host driver's wait" } else { "is polled" }))
        }
        "timer_ms" => {
            let ms: u64 = value.trim().parse().ok()?;
            static HELD: std::sync::Mutex<Option<omni_platform::clock::TimerResolution>> = std::sync::Mutex::new(None);
            let mut held = HELD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *held = None;
            if ms == 0 {
                return Some("timer_ms=0: the host's default timer resolution".into());
            }
            match omni_platform::clock::TimerResolution::raise(std::time::Duration::from_millis(ms)) {
                Ok(r) => {
                    *held = Some(r);
                    Some(format!("timer_ms={ms}: held"))
                }
                Err(e) => Some(format!("timer_ms={ms}: {e}")),
            }
        }
        "qos" => {
            let allow = match value.trim() {
                "high" => false,
                "auto" => true,
                _ => return None,
            };
            let ok = omni_platform::clock::allow_power_throttling(allow);
            Some(format!("qos={}: {}", value.trim(), if ok { "set" } else { "refused by the host" }))
        }
        "futex_herd" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::futex::HERD.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("futex_herd={}", u8::from(on)))
        }
        _ => None,
    }
}

/// Start the lever reader for this host process (once), and apply `OMNI_JIT_UNSAFE_FP`.
pub fn start() {
    crate::mm::madv_free_from_env();
    crate::binder::spawn::from_env();
    crate::guest::read_no_commit_from_env();
    if let Some(mask) = std::env::var("OMNI_JIT_UNSAFE_FP").ok().as_deref().and_then(parse_hex) {
        let kept = omni_cpu::dynarmic::set_live_fp_optimizations(mask);
        eprintln!("[lever] OMNI_JIT_UNSAFE_FP: jit_fp={kept:#x}");
    }
    if let Ok(v) = std::env::var("OMNI_POLL_KEYED") {
        let on = v.trim() != "0";
        crate::poll::KEYED.store(on, std::sync::atomic::Ordering::Relaxed);
        eprintln!("[lever] OMNI_POLL_KEYED: poll_keyed={}", u8::from(on));
    }
    if let Ok(v) = std::env::var("OMNI_REMOTE_DIRECT") {
        let on = v.trim() != "0";
        crate::remote::DIRECT.store(on, std::sync::atomic::Ordering::Relaxed);
        eprintln!("[lever] OMNI_REMOTE_DIRECT: remote_direct={}", u8::from(on));
    }
    let Some(path) = std::env::var_os("OMNI_LEVER_FILE").map(PathBuf::from) else { return };
    let _ = std::thread::Builder::new().name("omni-lever".into()).spawn(move || {
        let mut seen = String::new();
        loop {
            std::thread::sleep(POLL);
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text == seen {
                continue;
            }
            seen = text;
            for line in seen.lines().filter(|l| !l.trim().is_empty()) {
                match apply(line) {
                    Some(done) => eprintln!("[lever] pid {}: {done}", std::process::id()),
                    None => eprintln!("[lever] not understood: {line:?}"),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_masks_parse_with_or_without_the_prefix() {
        assert_eq!(parse_hex("0x40000"), Some(0x40000));
        assert_eq!(parse_hex("f0000\n"), Some(0xf0000));
        assert_eq!(parse_hex("zz"), None);
    }

    #[test]
    fn a_line_without_a_known_lever_is_not_understood() {
        assert_eq!(apply("nope=1"), None);
        assert_eq!(apply("no equals sign"), None);
    }

    #[test]
    fn the_compose_lever_switches_the_fast_path() {
        use std::sync::atomic::Ordering;
        apply("compose_fast=0").expect("understood");
        assert!(!crate::hal::compose::FAST.load(Ordering::Relaxed));
        apply("compose_fast=1").expect("understood");
        assert!(crate::hal::compose::FAST.load(Ordering::Relaxed));
        assert_eq!(apply("compose_fast=yes"), None);
    }

    #[test]
    fn the_zero_copy_and_bgra_levers_switch() {
        use std::sync::atomic::Ordering;
        use crate::hal::compose::{BGRA_OUT, ZERO};
        assert_eq!(apply("compose_zero=1").as_deref(), Some("compose_zero=1"));
        assert!(ZERO.load(Ordering::Relaxed));
        assert_eq!(apply("present_bgra=1").as_deref(), Some("present_bgra=1"));
        assert!(BGRA_OUT.load(Ordering::Relaxed));
        apply("compose_zero=0").expect("understood");
        apply("present_bgra=0").expect("understood");
        assert!(!ZERO.load(Ordering::Relaxed) && !BGRA_OUT.load(Ordering::Relaxed));
        assert_eq!(apply("compose_zero=on"), None);
        assert_eq!(apply("present_gpu=1").as_deref(), Some("present_gpu=1"));
        assert!(crate::gpu::window_present::on());
        apply("present_gpu=0").expect("understood");
        assert!(!crate::gpu::window_present::on());
        assert_eq!(apply("gralloc_direct=1").as_deref(), Some("gralloc_direct=1"));
        assert!(crate::gpu::native::DIRECT.load(Ordering::Relaxed));
        apply("gralloc_direct=0").expect("understood");
        assert!(!crate::gpu::native::DIRECT.load(Ordering::Relaxed));
        let w = &crate::gpu::native::RELEASE_WAIT_US;
        apply("release_wait=poll").expect("understood");
        assert_eq!(w.load(Ordering::Relaxed), 100);
        apply("release_poll=250").expect("understood");
        assert_eq!(w.load(Ordering::Relaxed), 250);
        assert!(apply("release_wait=event").expect("answered").contains("not available"));
        assert_eq!(w.load(Ordering::Relaxed), 250, "event changes nothing");
        apply("release_wait=spin").expect("understood");
        assert_eq!(w.load(Ordering::Relaxed), 0);
        assert_eq!(apply("release_poll=0"), None);
        assert_eq!(apply("release_wait=sleep"), None);
    }

    #[test]
    fn the_fence_poll_lever_sets_the_period() {
        use std::sync::atomic::Ordering;
        assert!(apply("fence_poll=250").expect("understood").starts_with("fence_poll=250"));
        assert_eq!(crate::gpu::FENCE_POLL_US.load(Ordering::Relaxed), 250);
        assert_eq!(apply("fence_poll=x"), None);
        apply("fence_poll=0").expect("understood");
        assert_eq!(crate::gpu::FENCE_POLL_US.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_vsync_levers_set_the_rate_and_the_pacing() {
        use std::sync::atomic::Ordering;
        assert!(apply("vsync_hz=30").expect("understood").starts_with("vsync_hz=30: period 33333333"));
        assert_eq!(apply("vsync_hz=0"), None);
        assert_eq!(apply("vsync_hz=fast"), None);
        assert!(apply("vsync_hz=60").expect("understood").starts_with("vsync_hz=60: period 16666667"));
        apply("vsync_pace=0").expect("understood");
        assert!(!crate::hal::composer::VSYNC_PACE.load(Ordering::Relaxed));
        apply("vsync_pace=1").expect("understood");
        assert!(crate::hal::composer::VSYNC_PACE.load(Ordering::Relaxed));
    }

    /// Values out of range are refused (switched values: `hal::composer`'s own test, as the
    /// levers are process-wide).
    #[test]
    fn the_composer_levers_refuse_what_they_do_not_know() {
        assert_eq!(apply("composer_fences=3"), None);
        assert_eq!(apply("composer_fences=x"), None);
        assert_eq!(apply("composer_skip_validate=yes"), None);
    }

    #[test]
    fn the_binder_pool_lever_switches_the_pool() {
        use std::sync::atomic::Ordering;
        let was = crate::binder::HOST_POOL.load(Ordering::Relaxed);
        apply("binder_host_pool=1").expect("understood");
        assert!(crate::binder::HOST_POOL.load(Ordering::Relaxed));
        apply("binder_host_pool=0").expect("understood");
        assert!(!crate::binder::HOST_POOL.load(Ordering::Relaxed));
        assert_eq!(apply("binder_host_pool=2"), None);
        crate::binder::HOST_POOL.store(was, Ordering::Relaxed);
    }

    #[test]
    fn the_vulkan_levers_switch_the_fast_path_and_batching() {
        use std::sync::atomic::Ordering;
        assert_eq!(apply("vk_fast=1").as_deref(), Some("vk_fast=1"));
        assert!(crate::gpu::FAST.load(Ordering::Relaxed));
        assert_eq!(apply("vk_batch=1").as_deref(), Some("vk_batch=1"));
        assert!(crate::gpu::BATCH.load(Ordering::Relaxed));
        assert_eq!(apply("vk_fast=on"), None);
        assert_eq!(apply("vk_handles=1").as_deref(), Some("vk_handles=1"));
        assert!(crate::gpu::HANDLE_CACHE.load(Ordering::Relaxed));
        assert_eq!(apply("vk_inline=1").as_deref(), Some("vk_inline=1"));
        assert!(crate::gpu::INLINE.load(Ordering::Relaxed));
        for lever in ["vk_fast=0", "vk_batch=0", "vk_handles=0", "vk_inline=0"] {
            apply(lever).expect("understood");
        }
        assert!(!crate::gpu::FAST.load(Ordering::Relaxed));
        assert!(!crate::gpu::BATCH.load(Ordering::Relaxed));
        assert!(!crate::gpu::HANDLE_CACHE.load(Ordering::Relaxed));
        assert!(!crate::gpu::INLINE.load(Ordering::Relaxed));
    }

    #[test]
    fn the_getset_lever_switches_the_precise_pass() {
        let want = cfg!(target_arch = "x86_64");
        let done = apply("jit_getset=0").expect("understood");
        assert!(done.starts_with("jit_getset=0"), "{done}");
        assert!(!omni_cpu::dynarmic::precise_get_set());
        let done = apply("jit_getset=1").expect("understood");
        assert!(done.starts_with(if want { "jit_getset=1" } else { "jit_getset=0" }), "{done}");
        assert_eq!(omni_cpu::dynarmic::precise_get_set(), want);
        assert_eq!(apply("jit_getset=on"), None);
    }

    #[test]
    fn the_fastdisp_lever_is_understood() {
        let want = cfg!(target_arch = "x86_64");
        let done = apply("jit_fastdisp=1").expect("understood");
        assert!(done.starts_with(if want { "jit_fastdisp=1" } else { "jit_fastdisp=0" }), "{done}");
        assert!(apply("jit_fastdisp=0").expect("understood").starts_with("jit_fastdisp=0"));
        assert_eq!(apply("jit_fastdisp=2"), None);
    }

    #[test]
    fn the_tbi_lever_takes_the_mask_off_and_puts_it_back() {
        let want = cfg!(target_arch = "x86_64");
        let done = apply("jit_tbi=0").expect("understood");
        assert!(done.starts_with(if want { "jit_tbi=0" } else { "jit_tbi=1" }), "{done}");
        assert!(apply("jit_tbi=1").expect("understood").starts_with("jit_tbi=1"));
        assert_eq!(apply("jit_tbi=off"), None);
    }

    #[test]
    fn the_tbiand_lever_is_understood() {
        let want = cfg!(target_arch = "x86_64");
        let done = apply("jit_tbiand=1").expect("understood");
        assert!(done.starts_with(if want { "jit_tbiand=1" } else { "jit_tbiand=0" }), "{done}");
        assert!(apply("jit_tbiand=0").expect("understood").starts_with("jit_tbiand=0"));
        assert_eq!(apply("jit_tbiand=2"), None);
    }

    #[test]
    fn the_fpxmm_lever_switches_scalar_fp_in_xmm() {
        let want = cfg!(target_arch = "x86_64");
        let was = omni_cpu::dynarmic::scalar_fp_in_xmm();
        let done = apply("jit_fpxmm=1").expect("understood");
        assert!(done.starts_with(if want { "jit_fpxmm=1" } else { "jit_fpxmm=0" }), "{done}");
        assert_eq!(omni_cpu::dynarmic::scalar_fp_in_xmm(), want);
        let done = apply("jit_fpxmm=0").expect("understood");
        assert!(done.starts_with("jit_fpxmm=0"), "{done}");
        assert!(!omni_cpu::dynarmic::scalar_fp_in_xmm());
        assert_eq!(apply("jit_fpxmm=yes"), None);
        omni_cpu::dynarmic::set_scalar_fp_in_xmm(was);
    }

    #[test]
    fn the_memory_levers_switch() {
        use std::sync::atomic::Ordering;
        assert!(apply("zero_reclaim=30").expect("understood").starts_with("zero_reclaim=30"));
        assert_eq!(crate::zero_reclaim::PERIOD_S.load(Ordering::Relaxed), 30);
        apply("zero_reclaim=0").expect("understood");
        assert_eq!(crate::zero_reclaim::PERIOD_S.load(Ordering::Relaxed), 0);
        assert_eq!(apply("zero_reclaim=soon"), None);
        apply("madv_free=1").expect("understood");
        assert!(crate::mm::MADV_FREE_DISCARDS.load(Ordering::Relaxed));
        apply("madv_free=0").expect("understood");
        assert!(!crate::mm::MADV_FREE_DISCARDS.load(Ordering::Relaxed));
        assert_eq!(apply("madv_free=yes"), None);
        apply("binder_spawn=kernel").expect("understood");
        assert!(crate::binder::spawn::KERNEL_RULE.load(Ordering::Relaxed));
        apply("binder_spawn=eager").expect("understood");
        assert!(!crate::binder::spawn::KERNEL_RULE.load(Ordering::Relaxed));
        assert_eq!(apply("binder_spawn=1"), None);
    }

    #[test]
    fn the_fp_lever_keeps_only_the_unsafe_fp_bits() {
        let done = apply("jit_fp=0xffffffff").expect("understood");
        let want = if cfg!(target_arch = "x86_64") { "jit_fp=0xf0000" } else { "jit_fp=0x0" };
        assert!(done.starts_with(want), "{done}");
        apply("jit_fp=0").expect("understood");
    }
}
