//! **`OMNI_GLES_TIMING=1`: where the GL time of a frame goes, on the CPU and on the GPU.**
//!
//! A diagnostic, off by default, for one question: when the engine's render thread waits on the
//! GPU (`glClientWaitSync`, `eglSwapBuffers`), is that the card or this layer? It adds three
//! readings to the census, printed every few seconds by an embedding that asks
//! ([`Gles::timing_window`](super::Gles::timing_window)) and once more in
//! [`Gles::report`](super::Gles::report):
//!
//! * **CPU**: the wall time of every guest GL/EGL call, by name, from the slot's entry to its
//!   return -- the host's work and this layer's -- with the calls this layer made to the host
//!   beyond the guest's own (the per-mapping context and binding queries) and the bytes the mapping
//!   shadows copied each way.
//! * **Waits**: every `glClientWaitSync` by its flags, timeout and answer, with the **age** of the
//!   fence it waited on -- how many `glFenceSync`s the engine had made since that one -- which is
//!   how early the engine waits.
//! * **GPU**: a `GL_TIMESTAMP_EXT` query (`GL_EXT_disjoint_timer_query`) as the last command before
//!   each `eglSwapBuffers` and as the first after it. The GPU writes each when it reaches it, so per
//!   frame: the **span** (first command to last, on the GPU), the **gap** to the next frame's first
//!   (the swap's own GPU work, other clients such as the compositor, and idle), and the **lag** --
//!   how far behind the CPU the GPU finished the frame, from `glGetInteger64v(GL_TIMESTAMP_EXT)`
//!   read as the end query was issued. A lag near zero is a GPU that waited for the CPU; a lag of
//!   tens of milliseconds is a GPU the CPU is waiting for.
//!
//! **What it must not change.** The queries are this layer's own objects, made with the guest's
//! context current and never visible to the guest (`glGenQueries` answers names no later guest
//! `glGenQueries` can be given). Every call it makes is valid under `GL_EXT_disjoint_timer_query`,
//! which is checked in the host's extension string first, so no GL error is raised that the
//! guest's `glGetError` could read; without the extension the GPU part is off and says why. A
//! result is read only once `GL_QUERY_RESULT_AVAILABLE` says so, so nothing here waits on the GPU
//! -- the availability check may flush the command stream, which the swap has just done anyway.
//! The engine makes no queries of its own on this path (the census: no `glBeginQuery`,
//! `glQueryCounterEXT` or `glGetQueryObject*` call in 30 minutes, l10).

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::host::HostProc;
use super::{signature, Call, Gles, Signature, SHAPES};

/// `GL_TIMESTAMP_EXT`.
pub const TIMESTAMP: u32 = 0x8E28;
/// `GL_QUERY_RESULT`.
pub const QUERY_RESULT: u32 = 0x8866;
/// `GL_QUERY_RESULT_AVAILABLE`.
pub const QUERY_RESULT_AVAILABLE: u32 = 0x8867;
/// The extension the GPU readings need.
pub const TIMER_EXTENSION: &str = "GL_EXT_disjoint_timer_query";

/// Below this, a draw's index or attribute pointer is an offset into a bound buffer.
pub const CLIENT_POINTER_FLOOR: u64 = 1 << 32;

/// Query names made at a time.
const QUERY_BATCH: usize = 16;
/// Frames whose results may be outstanding before the GPU part gives up (a driver that never
/// answers availability must not grow this without bound).
const MAX_PENDING: usize = 32;
/// Distinct `glClientWaitSync` shapes kept.
const MAX_WAIT_KEYS: usize = 64;
/// Recent fences remembered for their age.
const MAX_FENCES: usize = 64;

/// Whether `OMNI_GLES_TIMING` asks for timing: set, and not empty, `0` or `off`.
#[must_use]
pub fn asked(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let v = v.trim();
        !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("off")
    })
}

/// One `glClientWaitSync` shape: flags, timeout, answer, and the fence's age in fences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WaitKey {
    /// The guest's flags.
    pub flags: u32,
    /// The guest's timeout, in nanoseconds.
    pub timeout: u64,
    /// The host's answer (`GL_ALREADY_SIGNALED` 0x911A .. `GL_WAIT_FAILED` 0x911D).
    pub answer: u32,
    /// How many `glFenceSync`s were made after the one waited on (0: the newest); `u32::MAX` when
    /// the fence is older than this layer remembers.
    pub age: u32,
}

/// How many waits of one shape, and how long they took.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaitStat {
    /// Waits.
    pub count: u64,
    /// Their total wall time.
    pub nanos: u64,
    /// The longest.
    pub max_nanos: u64,
}

/// One frame as the GPU saw it, in the GPU's nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuFrame {
    /// The first command after the previous swap.
    pub begin: u64,
    /// The last command before this frame's swap.
    pub end: u64,
    /// `end` minus the GPU's clock when the CPU issued it: how far behind the GPU finished.
    pub lag: i64,
    /// This frame's begin minus the previous frame's end, when the previous frame was timed.
    pub gap: Option<u64>,
    /// This frame's begin minus the previous frame's begin.
    pub interval: Option<u64>,
}

impl GpuFrame {
    /// The frame's span on the GPU.
    #[must_use]
    pub fn span(&self) -> u64 {
        self.end.saturating_sub(self.begin)
    }
}

/// One stretch of a frame between two draw-framebuffer binds, as the GPU timeline is divided: its
/// place in the frame, the framebuffer drawn to, and the last viewport set in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pass {
    /// 0 for the stretch before the frame's first bind, then 1, 2, ...
    pub ordinal: u16,
    /// The draw framebuffer bound (0: the window's).
    pub framebuffer: u32,
    /// The last `glViewport` width and height in the stretch (or before it).
    pub viewport: (u32, u32),
}

#[derive(Debug, Clone)]
struct Pending {
    begin: u32,
    end: u32,
    gpu_at_end: u64,
    /// A timestamp at each draw-framebuffer bind, and the pass that bind ended.
    marks: Vec<(u32, Pass)>,
    /// The pass the swap ended.
    last_pass: Pass,
}

/// The queries of one context.
#[derive(Debug)]
struct Ring {
    context: u64,
    free: Vec<u32>,
    open_begin: Option<u32>,
    open_marks: Vec<(u32, Pass)>,
    pass: Pass,
    pending: VecDeque<Pending>,
    last: Option<(u64, u64)>,
}

/// Most pass marks one frame gets (a frame with more binds folds the rest into its last pass).
const MAX_MARKS: usize = 32;

#[derive(Debug, Clone, Copy)]
struct Procs {
    query_counter: (HostProc, &'static Signature),
    result_u64: (HostProc, &'static Signature),
}

#[derive(Debug, Default)]
struct Snapshot {
    at: Option<Instant>,
    presents: u64,
    per_entry: Vec<(u64, u64, u64)>,
    bytes: (u64, u64),
}

#[derive(Debug)]
struct State {
    /// `None` until the first swap decides; `Err` once the GPU part is off, saying why.
    gpu: Option<Result<Procs, String>>,
    ring: Option<Ring>,
    frames: Vec<GpuFrame>,
    frames_total: u64,
    swap_nanos: Vec<u64>,
    fences: VecDeque<(u64, u64)>,
    fence_seq: u64,
    waits: BTreeMap<WaitKey, WaitStat>,
    waits_total: BTreeMap<WaitKey, WaitStat>,
    waits_dropped: u64,
    observed: BTreeMap<String, u64>,
    /// Draws and vertex-array pointers by name and by where their data is: `true` for client
    /// memory (a pointer), `false` for a bound buffer (an offset) -- (calls, nanos, elements).
    draws: BTreeMap<(&'static str, bool), (u64, u64, u64)>,
    /// GPU time per pass, per frame, this window.
    passes: BTreeMap<Pass, Vec<u64>>,
    last: Snapshot,
}

/// The timing census of one [`Gles`]. See the module documentation.
#[derive(Debug)]
pub struct Timing {
    state: Mutex<State>,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                gpu: None,
                ring: None,
                frames: Vec::new(),
                frames_total: 0,
                swap_nanos: Vec::new(),
                fences: VecDeque::new(),
                fence_seq: 0,
                waits: BTreeMap::new(),
                waits_total: BTreeMap::new(),
                waits_dropped: 0,
                observed: BTreeMap::new(),
                draws: BTreeMap::new(),
                passes: BTreeMap::new(),
                last: Snapshot { at: Some(Instant::now()), ..Snapshot::default() },
            }),
        }
    }
}

/// Call a host function the registry has a signature for.
///
/// # Safety
///
/// `proc` must be the host's function for `sig.name` ([`HostProc::new`]'s contract).
unsafe fn call_proc(proc: HostProc, sig: &Signature, lanes: &[u64]) -> u64 {
    debug_assert_eq!(lanes.len(), sig.abi.len());
    // SAFETY: the caller's contract; the shape is the registry's for this very name.
    unsafe { (SHAPES[sig.shape as usize].call)(proc.address(), lanes) }
}

impl Timing {
    /// Record something the guest asked for, by a line of text (`eglSwapInterval(1) -> 1`).
    pub(super) fn observe(&self, what: String) {
        let mut state = self.state.lock();
        if state.observed.len() < 256 || state.observed.contains_key(&what) {
            *state.observed.entry(what).or_insert(0) += 1;
        }
    }

    /// A draw or vertex-array call: whether its data is client memory, and how many elements.
    ///
    /// Read from the arguments alone, with no GL query: an index or attribute "pointer" below
    /// [`CLIENT_POINTER_FLOOR`] is an offset into a bound buffer (no buffer this engine makes is
    /// 4 GiB), one above it a pointer into the guest's memory, which the driver reads at the draw.
    pub(super) fn call_made(&self, gles: &Gles, call: &Call, took: Duration) {
        let (name, lanes) = (call.name, &call.lanes);
        match name {
            // GL_FRAMEBUFFER, GL_DRAW_FRAMEBUFFER: a new draw target, so a pass boundary.
            "glBindFramebuffer" if matches!(lanes[0] as u32, 0x8D40 | 0x8CA9) => {
                return self.pass_boundary(gles, call, lanes[1] as u32);
            }
            "glViewport" => {
                let mut state = self.state.lock();
                if let Some(ring) = state.ring.as_mut() {
                    ring.pass.viewport = (lanes[2] as u32, lanes[3] as u32);
                }
                return;
            }
            _ => {}
        }
        let (client, elements) = match name {
            "glDrawElements" => (lanes[3] >= CLIENT_POINTER_FLOOR, lanes[1]),
            "glDrawElementsInstanced" | "glDrawElementsInstancedEXT" => {
                (lanes[3] >= CLIENT_POINTER_FLOOR, lanes[1] * lanes[4].max(1))
            }
            "glDrawArrays" => (false, lanes[2]),
            "glDrawArraysInstanced" | "glDrawArraysInstancedEXT" => (false, lanes[2] * lanes[3].max(1)),
            "glVertexAttribPointer" => (lanes[5] >= CLIENT_POINTER_FLOOR, 0),
            _ => return,
        };
        let nanos = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        let mut state = self.state.lock();
        let row = state.draws.entry((name, client)).or_default();
        row.0 += 1;
        row.1 += nanos;
        row.2 += elements;
    }

    /// A timestamp where one pass ends and the next begins.
    fn pass_boundary(&self, gles: &Gles, call: &Call, framebuffer: u32) {
        let mut state = self.state.lock();
        let Some(Ok(procs)) = state.gpu.as_ref().map(|g| g.as_ref().map(|p| *p)) else { return };
        let Some(ring) = state.ring.as_mut() else { return };
        if ring.open_begin.is_none() || ring.open_marks.len() >= MAX_MARKS {
            ring.pass.framebuffer = framebuffer;
            return;
        }
        // Only in the context the ring's queries belong to.
        match gles.host_call_quiet(call, "eglGetCurrentContext", &[]) {
            Ok(context) if context == ring.context => {}
            _ => return,
        }
        let Some(mark) = take_name(gles, call, ring) else { return };
        // SAFETY: the host's glQueryCounterEXT, resolved by that name.
        unsafe { call_proc(procs.query_counter.0, procs.query_counter.1, &[u64::from(mark), u64::from(TIMESTAMP)]) };
        let ended = ring.pass;
        ring.open_marks.push((mark, ended));
        ring.pass = Pass { ordinal: ended.ordinal + 1, framebuffer, viewport: ended.viewport };
    }

    /// A fence the guest made.
    pub(super) fn fence_made(&self, sync: u64) {
        let mut state = self.state.lock();
        state.fence_seq += 1;
        let seq = state.fence_seq;
        state.fences.push_back((sync, seq));
        if state.fences.len() > MAX_FENCES {
            state.fences.pop_front();
        }
    }

    /// A `glClientWaitSync` the guest made, and what it cost.
    pub(super) fn waited(&self, sync: u64, flags: u32, timeout: u64, answer: u32, took: Duration) {
        let mut state = self.state.lock();
        let age = state
            .fences
            .iter()
            .rev()
            .find(|(s, _)| *s == sync)
            .map_or(u32::MAX, |(_, seq)| u32::try_from(state.fence_seq - seq).unwrap_or(u32::MAX));
        let key = WaitKey { flags, timeout, answer, age };
        let nanos = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        let State { waits, waits_total, waits_dropped, .. } = &mut *state;
        for map in [waits, waits_total] {
            if map.len() >= MAX_WAIT_KEYS && !map.contains_key(&key) {
                *waits_dropped += 1;
                continue;
            }
            let stat = map.entry(key).or_default();
            stat.count += 1;
            stat.nanos += nanos;
            stat.max_nanos = stat.max_nanos.max(nanos);
        }
    }

    /// Every wait shape recorded this session.
    #[must_use]
    pub fn waits_so_far(&self) -> Vec<(WaitKey, WaitStat)> {
        self.state.lock().waits_total.iter().map(|(k, s)| (*k, *s)).collect()
    }

    /// Decide once whether the GPU part can run on this host, and resolve what it calls.
    fn gpu_procs(&self, gles: &Gles, call: &Call, state: &mut State) -> Option<Procs> {
        if state.gpu.is_none() {
            state.gpu = Some(resolve_procs(gles, call));
        }
        match &state.gpu {
            Some(Ok(procs)) => Some(*procs),
            _ => None,
        }
    }

    /// The last command before the guest's `eglSwapBuffers`: the end of the frame, on the GPU.
    pub(super) fn before_swap(&self, gles: &Gles, call: &Call) {
        let mut state = self.state.lock();
        let Some(procs) = self.gpu_procs(gles, call, &mut state) else { return };
        let Ok(context) = gles.host_call_quiet(call, "eglGetCurrentContext", &[]) else { return };
        if state.ring.as_ref().is_none_or(|ring| ring.context != context) {
            // A new context: the old one's query names are not this one's (query objects are not
            // shared), so they are abandoned rather than used or deleted from the wrong context.
            state.ring = Some(Ring {
                context,
                free: Vec::new(),
                open_begin: None,
                open_marks: Vec::new(),
                pass: Pass::default(),
                pending: VecDeque::new(),
                last: None,
            });
        }
        let ring = state.ring.as_mut().expect("just made");
        let marks = std::mem::take(&mut ring.open_marks);
        let last_pass = ring.pass;
        // The next frame starts in whatever the swap left bound, as its pass 0.
        ring.pass = Pass { ordinal: 0, ..last_pass };
        let Some(begin) = ring.open_begin.take() else {
            ring.free.extend(marks.iter().map(|(q, _)| *q));
            return;
        };
        let Some(end) = take_name(gles, call, ring) else { return };
        // SAFETY: the host's glQueryCounterEXT, resolved by that name.
        unsafe { call_proc(procs.query_counter.0, procs.query_counter.1, &[u64::from(end), u64::from(TIMESTAMP)]) };
        let mut now: i64 = 0;
        let _ = gles.host_call_quiet(call, "glGetInteger64v", &[u64::from(TIMESTAMP), &mut now as *mut i64 as u64]);
        ring.pending.push_back(Pending { begin, end, gpu_at_end: now as u64, marks, last_pass });
    }

    /// The first command after it, and the frames whose results have arrived.
    pub(super) fn after_swap(&self, gles: &Gles, call: &Call, swap: Duration) {
        let mut state = self.state.lock();
        state.swap_nanos.push(u64::try_from(swap.as_nanos()).unwrap_or(u64::MAX));
        let Some(procs) = self.gpu_procs(gles, call, &mut state) else { return };
        let State { ring, frames, frames_total, gpu, passes, .. } = &mut *state;
        let Some(ring) = ring.as_mut() else { return };
        if let Some(begin) = take_name(gles, call, ring) {
            // SAFETY: as in `before_swap`.
            unsafe { call_proc(procs.query_counter.0, procs.query_counter.1, &[u64::from(begin), u64::from(TIMESTAMP)]) };
            ring.open_begin = Some(begin);
        }
        while let Some(front) = ring.pending.front().cloned() {
            let mut available: u32 = 0;
            let _ = gles.host_call_quiet(
                call,
                "glGetQueryObjectuiv",
                &[u64::from(front.end), u64::from(QUERY_RESULT_AVAILABLE), &mut available as *mut u32 as u64],
            );
            if available == 0 {
                break;
            }
            let (mut begin, mut end) = (0u64, 0u64);
            for (name, out) in [(front.begin, &mut begin), (front.end, &mut end)] {
                // SAFETY: the host's glGetQueryObjectui64vEXT, resolved by that name; `out` is a
                // live u64. The begin query was issued before the end one, so it is available too.
                unsafe {
                    call_proc(
                        procs.result_u64.0,
                        procs.result_u64.1,
                        &[u64::from(name), u64::from(QUERY_RESULT), out as *mut u64 as u64],
                    )
                };
            }
            // Each pass: from the previous timestamp to its own mark (the last to the end query).
            let mut previous = begin;
            let mut stamps: Vec<(u64, Pass)> = Vec::with_capacity(front.marks.len() + 1);
            for (mark, pass) in &front.marks {
                let mut at = 0u64;
                // SAFETY: as above; issued between the begin and end queries, so available too.
                unsafe {
                    call_proc(
                        procs.result_u64.0,
                        procs.result_u64.1,
                        &[u64::from(*mark), u64::from(QUERY_RESULT), &mut at as *mut u64 as u64],
                    )
                };
                stamps.push((at, *pass));
            }
            stamps.push((end, front.last_pass));
            for (at, pass) in stamps {
                let entry = passes.entry(pass).or_default();
                if entry.len() < 100_000 {
                    entry.push(at.saturating_sub(previous));
                }
                previous = at;
            }
            ring.pending.pop_front();
            ring.free.extend([front.begin, front.end]);
            ring.free.extend(front.marks.iter().map(|(q, _)| *q));
            let frame = GpuFrame {
                begin,
                end,
                lag: end as i64 - front.gpu_at_end as i64,
                gap: ring.last.map(|(_, last_end)| begin.saturating_sub(last_end)),
                interval: ring.last.map(|(last_begin, _)| begin.saturating_sub(last_begin)),
            };
            ring.last = Some((begin, end));
            *frames_total += 1;
            if frames.len() < 100_000 {
                frames.push(frame);
            }
        }
        if ring.pending.len() > MAX_PENDING {
            *gpu = Some(Err(format!(
                "{} frames' timestamp queries never became available; the GPU part stopped",
                ring.pending.len()
            )));
        }
    }

    /// The readings since the previous call, as `GLES TIMING` / `GLES GPU` / `GLES WAITS` lines.
    pub(super) fn window(&self, gles: &Gles) -> String {
        let now = Instant::now();
        let per_entry = gles.entry_counters();
        let presents = gles.presents();
        let bytes = gles.shadow_bytes();
        let mut state = self.state.lock();
        let last = std::mem::take(&mut state.last);
        let wall = last.at.map_or(Duration::ZERO, |at| now.duration_since(at));
        let frames = presents.saturating_sub(last.presents);
        let mut out = String::new();
        let names = gles.entry_names();
        let mut rows: Vec<(&str, u64, u64, u64)> = Vec::new();
        for (index, (calls, nanos, extra)) in per_entry.iter().enumerate() {
            let (c0, n0, e0) = last.per_entry.get(index).copied().unwrap_or_default();
            let (dc, dn, de) = (calls - c0, nanos - n0, extra - e0);
            if dc + de > 0 {
                rows.push((names.get(index).copied().flatten().unwrap_or("?"), dc, dn, de));
            }
        }
        let per_frame = |v: f64| if frames > 0 { v / frames as f64 } else { v };
        let calls: u64 = rows.iter().map(|r| r.1).sum();
        let nanos: u64 = rows.iter().map(|r| r.2).sum();
        let mut extra: Vec<(&str, u64)> = rows.iter().filter(|r| r.3 > 0).map(|r| (r.0, r.3)).collect();
        extra.sort_by(|a, b| b.1.cmp(&a.1));
        rows.sort_by(|a, b| b.2.cmp(&a.2));
        let wall_ms = wall.as_secs_f64() * 1e3;
        out.push_str(&format!(
            "GLES TIMING: {:.1}s, {frames} frames ({:.1}/s, {:.1} ms each); GL calls/frame {:.0}, in GL \
             {:.1} ms/frame ({:.0}% of the wall); by time/frame: {}; this layer's own host calls/frame: \
             {}; shadow bytes/frame: {:.2} MiB in (driver -> guest), {:.2} MiB out (guest -> driver)\n",
            wall.as_secs_f64(),
            frames as f64 / wall.as_secs_f64().max(1e-9),
            if frames > 0 { wall_ms / frames as f64 } else { 0.0 },
            per_frame(calls as f64),
            per_frame(nanos as f64 / 1e6),
            100.0 * nanos as f64 / 1e6 / wall_ms.max(1e-9),
            rows.iter()
                .take(12)
                .map(|(name, c, n, _)| format!(
                    "{name} {:.2} ms x{:.0} ({:.1} us each)",
                    per_frame(*n as f64 / 1e6),
                    per_frame(*c as f64),
                    *n as f64 / 1e3 / (*c).max(1) as f64
                ))
                .collect::<Vec<_>>()
                .join(", "),
            if extra.is_empty() {
                "none".to_string()
            } else {
                extra.iter().map(|(n, e)| format!("{n} {:.1}", per_frame(*e as f64))).collect::<Vec<_>>().join(", ")
            },
            per_frame((bytes.0 - last.bytes.0) as f64 / 1048576.0),
            per_frame((bytes.1 - last.bytes.1) as f64 / 1048576.0),
        ));
        let frames_gpu = std::mem::take(&mut state.frames);
        let swaps = std::mem::take(&mut state.swap_nanos);
        out.push_str(&gpu_line(&state.gpu, &frames_gpu, &swaps));
        out.push('\n');
        out.push_str(&waits_line(&std::mem::take(&mut state.waits), frames));
        out.push('\n');
        out.push_str(&draws_line(&std::mem::take(&mut state.draws), frames));
        out.push('\n');
        out.push_str(&passes_line(&std::mem::take(&mut state.passes)));
        state.last = Snapshot { at: Some(now), presents, per_entry, bytes };
        out
    }

    /// The whole session's lines, for the final report.
    pub(super) fn totals(&self, gles: &Gles) -> String {
        let per_entry = gles.entry_counters();
        let names = gles.entry_names();
        let presents = gles.presents().max(1);
        let mut rows: Vec<(&str, u64, u64)> = per_entry
            .iter()
            .enumerate()
            .filter(|(_, (c, _, _))| *c > 0)
            .map(|(i, (c, n, _))| (names.get(i).copied().flatten().unwrap_or("?"), *c, *n))
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2));
        let state = self.state.lock();
        let mut out = format!(
            "GLES: timing (OMNI_GLES_TIMING), whole session, per present: {}\n",
            rows.iter()
                .take(20)
                .map(|(n, c, t)| format!("{n} {:.2} ms x{:.1}", *t as f64 / 1e6 / presents as f64, *c as f64 / presents as f64))
                .collect::<Vec<_>>()
                .join(", ")
        );
        out.push_str(&format!(
            "GLES: timing: {} frames timed on the GPU; {}\n",
            state.frames_total,
            match &state.gpu {
                None => "the GPU part never ran (no swap)".to_string(),
                Some(Ok(_)) => "GL_TIMESTAMP_EXT queries".to_string(),
                Some(Err(why)) => format!("GPU part off: {why}"),
            }
        ));
        out.push_str(&format!("GLES: timing, all session: {}\n", waits_line(&state.waits_total, presents)));
        out.push_str(&format!(
            "GLES: timing, observed: {}",
            if state.observed.is_empty() {
                "nothing".to_string()
            } else {
                state.observed.iter().map(|(w, n)| format!("{w} x{n}")).collect::<Vec<_>>().join(", ")
            }
        ));
        if state.waits_dropped > 0 {
            out.push_str(&format!(" ({} wait shapes not kept)", state.waits_dropped));
        }
        out
    }
}

fn resolve_procs(gles: &Gles, call: &Call) -> Result<Procs, String> {
    let extensions = gles
        .host_call_quiet(call, "glGetString", &[u64::from(super::gl::EXTENSIONS)])
        .map_err(|e| format!("glGetString(GL_EXTENSIONS) could not be asked: {e}"))?;
    if extensions == 0 {
        return Err("the host answered no extension string".to_string());
    }
    // SAFETY: a non-NULL glGetString result is a static NUL-terminated host string.
    let text = unsafe { std::ffi::CStr::from_ptr(extensions as usize as *const core::ffi::c_char) }
        .to_string_lossy()
        .into_owned();
    if !text.split_ascii_whitespace().any(|e| e == TIMER_EXTENSION) {
        return Err(format!("the host does not advertise {TIMER_EXTENSION}"));
    }
    let host = gles.require_host(call).map_err(|e| e.to_string())?;
    let get = |name: &'static str| -> Result<(HostProc, &'static Signature), String> {
        let sig = signature(name).ok_or_else(|| format!("no registry signature for {name}"))?;
        let proc = host
            .proc_address(name)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("the host has no {name}"))?;
        Ok((proc, sig))
    };
    Ok(Procs { query_counter: get("glQueryCounterEXT")?, result_u64: get("glGetQueryObjectui64vEXT")? })
}

fn take_name(gles: &Gles, call: &Call, ring: &mut Ring) -> Option<u32> {
    if ring.free.is_empty() {
        let mut names = [0u32; QUERY_BATCH];
        gles.host_call_quiet(call, "glGenQueries", &[QUERY_BATCH as u64, names.as_mut_ptr() as u64]).ok()?;
        ring.free.extend(names.iter().copied().filter(|&n| n != 0));
    }
    ring.free.pop()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let at = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[at.min(sorted.len() - 1)]
}

fn stats(mut v: Vec<f64>) -> String {
    if v.is_empty() {
        return "-".to_string();
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    format!("{:.1} ({:.1}..{:.1})", percentile(&v, 0.5), percentile(&v, 0.1), percentile(&v, 0.9))
}

fn gpu_line(gpu: &Option<Result<Procs, String>>, frames: &[GpuFrame], swaps: &[u64]) -> String {
    let swap = stats(swaps.iter().map(|n| *n as f64 / 1e6).collect());
    match gpu {
        Some(Err(why)) => return format!("GLES GPU: off -- {why}; eglSwapBuffers on the CPU ms median (p10..p90) {swap}"),
        None => return format!("GLES GPU: not started; eglSwapBuffers ms {swap}"),
        Some(Ok(_)) => {}
    }
    let spans: Vec<f64> = frames.iter().map(|f| f.span() as f64 / 1e6).collect();
    let gaps: Vec<f64> = frames.iter().filter_map(|f| f.gap).map(|g| g as f64 / 1e6).collect();
    let lags: Vec<f64> = frames.iter().map(|f| f.lag as f64 / 1e6).collect();
    let (busy, total): (u64, u64) = frames
        .iter()
        .filter_map(|f| f.interval.map(|i| (f.span().min(i), i)))
        .fold((0, 0), |(b, t), (s, i)| (b + s, t + i));
    format!(
        "GLES GPU: {} frames; ms median (p10..p90): span {} , gap to the next frame {} , lag at the \
         swap {} ; frame spans cover {:.0}% of the GPU time between frame starts; eglSwapBuffers on \
         the CPU {}",
        frames.len(),
        stats(spans),
        stats(gaps),
        stats(lags),
        if total > 0 { 100.0 * busy as f64 / total as f64 } else { 0.0 },
        swap
    )
}

fn passes_line(passes: &BTreeMap<Pass, Vec<u64>>) -> String {
    if passes.is_empty() {
        return "GLES PASSES: none timed".to_string();
    }
    format!(
        "GLES PASSES (GPU ms per frame, median, by the frame's draw-framebuffer binds): {}",
        passes
            .iter()
            .map(|(pass, times)| {
                let mut ms: Vec<f64> = times.iter().map(|t| *t as f64 / 1e6).collect();
                ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
                format!(
                    "#{} fb {} {}x{}: {:.2} (n {})",
                    pass.ordinal,
                    pass.framebuffer,
                    pass.viewport.0,
                    pass.viewport.1,
                    percentile(&ms, 0.5),
                    ms.len()
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn draws_line(draws: &BTreeMap<(&'static str, bool), (u64, u64, u64)>, frames: u64) -> String {
    let f = frames.max(1) as f64;
    format!(
        "GLES DRAWS: {}",
        if draws.is_empty() {
            "none".to_string()
        } else {
            draws
                .iter()
                .map(|((name, client), (calls, nanos, elements))| {
                    format!(
                        "{name} from {} x{:.0}/frame, {:.1} us each, {:.0} elements each",
                        if *client { "CLIENT memory" } else { "a buffer" },
                        *calls as f64 / f,
                        *nanos as f64 / 1e3 / (*calls).max(1) as f64,
                        *elements as f64 / (*calls).max(1) as f64
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        }
    )
}

fn waits_line(waits: &BTreeMap<WaitKey, WaitStat>, frames: u64) -> String {
    if waits.is_empty() {
        return "GLES WAITS: no glClientWaitSync".to_string();
    }
    let mut rows: Vec<(&WaitKey, &WaitStat)> = waits.iter().collect();
    rows.sort_by(|a, b| b.1.nanos.cmp(&a.1.nanos));
    format!(
        "GLES WAITS: glClientWaitSync by (flags, timeout, answer, fence age): {}",
        rows.iter()
            .take(8)
            .map(|(k, s)| format!(
                "({:#x}, {} ns, {}, age {}) x{} = {:.2} ms/frame, mean {:.2} ms, max {:.1} ms",
                k.flags,
                k.timeout,
                match k.answer {
                    0x911A => "ALREADY_SIGNALED",
                    0x911B => "TIMEOUT_EXPIRED",
                    0x911C => "CONDITION_SATISFIED",
                    0x911D => "WAIT_FAILED",
                    _ => "?",
                },
                if k.age == u32::MAX { "?".to_string() } else { k.age.to_string() },
                s.count,
                s.nanos as f64 / 1e6 / frames.max(1) as f64,
                s.nanos as f64 / 1e6 / s.count.max(1) as f64,
                s.max_nanos as f64 / 1e6
            ))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

/// The counters one slot keeps for the timing census.
#[derive(Debug, Default)]
pub(super) struct EntryTiming {
    /// Wall time of the guest's calls through this slot, when timing is on.
    pub(super) nanos: AtomicU64,
    /// Calls this layer made to the host function behind this slot beyond the guest's own.
    pub(super) extra: AtomicU64,
}

impl EntryTiming {
    pub(super) fn add(&self, took: Duration) {
        self.nanos.fetch_add(u64::try_from(took.as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed);
    }
}
