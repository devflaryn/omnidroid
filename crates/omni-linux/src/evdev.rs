//! **`/dev/input/event*`: input devices, as the Linux evdev driver presents them** -- what a
//! device's keyboard, mouse and touch screen are to Android. The real `InputReader` (its
//! `EventHub`, in system_server) scans `/dev/input`, asks each node what it is (`EVIOCGNAME`,
//! `EVIOCGID`, `EVIOCGBIT`, ...), classifies it (a keyboard, a cursor device) and reads
//! `struct input_event`s from it; everything above -- key layouts, the pointer, focus, dispatch
//! to the app -- is Android's own.
//!
//! The devices are made by the embedding ([`register`]) and fed by it ([`Device::send`]): on this
//! path the live display window (`crate::display_window`) turns the host window's keyboard and
//! mouse into a USB-style keyboard and a relative mouse. They exist in the host process that made
//! them -- the system's, where system_server's `InputReader` is -- and must be registered before
//! it scans (it also watches `/dev/input` with inotify, which reports nothing here).
//!
//! Each open is a client with its own queue, as the driver's are: an event sent reaches every
//! client, a client that falls [`QUEUE_MAX`] events behind loses its queue to one `SYN_DROPPED`,
//! and a read hands out whole 24-byte `struct input_event`s (arm64: a 16-byte `timeval`, type,
//! code, value), stamped with the instance's `CLOCK_MONOTONIC` -- the clock `EventHub` asks for
//! (`EVIOCSCLOCKID`) and compares against.
use std::collections::VecDeque;
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::{Mutex, RwLock};

use crate::errno::{Errno, SysResult, EAGAIN, EINVAL, ENODEV, ENOENT, ENOTTY};
use crate::fd::{FileKind, OpenFile};
use crate::process::{Process, Task};

/// Event types (`linux/input-event-codes.h`).
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
/// `SYN_REPORT`, `SYN_DROPPED`.
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;
/// Absolute axes.
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
/// Relative axes.
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
/// Mouse buttons.
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;
/// `MSC_SCAN`: the key's own scan code, sent before its `EV_KEY` as a USB keyboard does.
pub const MSC_SCAN: u16 = 0x04;
/// `BUS_USB`.
pub const BUS_USB: u16 = 0x03;

/// How many events a client may fall behind before its queue is dropped.
pub const QUEUE_MAX: usize = 4096;
/// `struct input_event` on arm64.
const EVENT_SIZE: usize = 24;

/// The bitmaps' sizes in bytes, as the driver rounds them (to whole `long`s).
const KEY_BYTES: usize = 96; // KEY_MAX 0x2ff
const SMALL_BYTES: usize = 8; // EV, REL, ABS, MSC, LED, SND, SW, PROP
const FF_BYTES: usize = 16; // FF_MAX 0x7f

/// What a device is: its identity and the codes it can report.
#[derive(Debug, Clone)]
pub struct Spec {
    pub name: String,
    /// `struct input_id`: bus, vendor, product, version.
    pub id: [u16; 4],
    pub keys: Vec<u16>,
    pub rels: Vec<u16>,
    pub abs: Vec<u16>,
    pub msc: Vec<u16>,
}

impl Spec {
    /// A full PC keyboard: every key code from `KEY_ESC` to `KEY_MICMUTE`.
    #[must_use]
    pub fn keyboard(name: &str) -> Self {
        Self { name: name.into(), id: [BUS_USB, 0, 0, 1], keys: (1..=248).collect(), rels: Vec::new(), abs: Vec::new(), msc: Vec::new() }
    }

    /// A five-button wheel mouse: relative X and Y, both wheels.
    #[must_use]
    pub fn mouse(name: &str) -> Self {
        Self {
            name: name.into(),
            id: [BUS_USB, 0, 0, 1],
            keys: vec![BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA],
            rels: vec![REL_X, REL_Y, REL_HWHEEL, REL_WHEEL],
            abs: Vec::new(),
            msc: Vec::new(),
        }
    }

    fn types(&self) -> Vec<u16> {
        let mut t = vec![EV_SYN];
        if !self.keys.is_empty() {
            t.push(EV_KEY);
        }
        if !self.rels.is_empty() {
            t.push(EV_REL);
        }
        if !self.abs.is_empty() {
            t.push(EV_ABS);
        }
        if !self.msc.is_empty() {
            t.push(EV_MSC);
        }
        t
    }
}

/// One input device, `/dev/input/event<number>`.
pub struct Device {
    pub number: usize,
    pub spec: Spec,
    clients: Mutex<Vec<Weak<Client>>>,
    /// Keys and buttons down now, for `EVIOCGKEY`.
    down: Mutex<[u8; KEY_BYTES]>,
}

/// One open of a device: its own queue.
pub struct Client {
    device: Arc<Device>,
    queue: Mutex<VecDeque<[u8; EVENT_SIZE]>>,
}

fn registry() -> &'static RwLock<Vec<Arc<Device>>> {
    static DEVICES: OnceLock<RwLock<Vec<Arc<Device>>>> = OnceLock::new();
    DEVICES.get_or_init(RwLock::default)
}

/// Make a device, `/dev/input/event<n>` for the next `n`.
pub fn register(spec: Spec) -> Arc<Device> {
    let mut devices = registry().write();
    let device = Arc::new(Device { number: devices.len(), spec, clients: Mutex::default(), down: Mutex::new([0; KEY_BYTES]) });
    devices.push(Arc::clone(&device));
    device
}

/// The device `/dev/input/event<n>` names, if there is one.
#[must_use]
pub fn device(n: usize) -> Option<Arc<Device>> {
    registry().read().get(n).cloned()
}

/// How many devices there are (`event0` .. `event<n-1>`).
#[must_use]
pub fn count() -> usize {
    registry().read().len()
}

/// The node name of device `n` (`event<n>`), and `n` back from one.
#[must_use]
pub fn node_number(name: &[u8]) -> Option<usize> {
    let n: usize = std::str::from_utf8(name.strip_prefix(b"event")?).ok()?.parse().ok()?;
    (n < count()).then_some(n)
}

impl Device {
    /// Open it: a new client, whose queue starts empty.
    #[must_use]
    pub fn open(self: &Arc<Self>) -> Arc<Client> {
        let client = Arc::new(Client { device: Arc::clone(self), queue: Mutex::new(VecDeque::new()) });
        self.clients.lock().push(Arc::downgrade(&client));
        client
    }

    /// Send `events` (type, code, value) as one packet: each stamped now, a `SYN_REPORT` after
    /// them, to every client; readers are woken.
    pub fn send(&self, events: &[(u16, u16, i32)]) {
        let now = crate::sys::monotonic();
        let (sec, usec) = (now.as_secs() as i64, i64::from(now.subsec_micros()));
        let encode = |(kind, code, value): (u16, u16, i32)| {
            let mut e = [0u8; EVENT_SIZE];
            e[0..8].copy_from_slice(&sec.to_le_bytes());
            e[8..16].copy_from_slice(&usec.to_le_bytes());
            e[16..18].copy_from_slice(&kind.to_le_bytes());
            e[18..20].copy_from_slice(&code.to_le_bytes());
            e[20..24].copy_from_slice(&value.to_le_bytes());
            e
        };
        {
            let mut down = self.down.lock();
            for &(kind, code, value) in events {
                if kind == EV_KEY && usize::from(code) < KEY_BYTES * 8 {
                    let (byte, bit) = (usize::from(code) / 8, code % 8);
                    if value == 0 {
                        down[byte] &= !(1 << bit);
                    } else {
                        down[byte] |= 1 << bit;
                    }
                }
            }
        }
        let packet: Vec<[u8; EVENT_SIZE]> = events.iter().copied().chain([(EV_SYN, SYN_REPORT, 0)]).map(encode).collect();
        let mut clients = self.clients.lock();
        clients.retain(|c| c.strong_count() > 0);
        for client in clients.iter().filter_map(Weak::upgrade) {
            let mut q = client.queue.lock();
            if q.len() + packet.len() > QUEUE_MAX {
                // Behind by a queue: what it missed is gone, and it is told so.
                q.clear();
                q.push_back(encode((EV_SYN, SYN_DROPPED, 0)));
            }
            q.extend(packet.iter().copied());
        }
        drop(clients);
        crate::poll::notify_key(std::ptr::from_ref(self) as crate::poll::Key);
    }

    /// The bitmap of `kind`'s codes (`EVIOCGBIT`), `kind` 0 being the event types.
    fn bits(&self, kind: u16) -> Vec<u8> {
        let (codes, size): (Vec<u16>, usize) = match kind {
            0 => (self.spec.types(), SMALL_BYTES),
            EV_KEY => (self.spec.keys.clone(), KEY_BYTES),
            EV_REL => (self.spec.rels.clone(), SMALL_BYTES),
            EV_ABS => (self.spec.abs.clone(), SMALL_BYTES),
            EV_MSC => (self.spec.msc.clone(), SMALL_BYTES),
            0x15 => (Vec::new(), FF_BYTES),
            _ => (Vec::new(), SMALL_BYTES),
        };
        let mut map = vec![0u8; size];
        for code in codes {
            if let Some(b) = map.get_mut(usize::from(code) / 8) {
                *b |= 1 << (code % 8);
            }
        }
        map
    }
}

/// `read` on an input device; `None` for any other descriptor.
pub fn read(file: &OpenFile, buf: &mut [u8], task: &Task) -> Option<Result<usize, Errno>> {
    let nonblocking = *file.flags.lock() & 0o4000 != 0;
    let client = match &*file.kind.lock() {
        FileKind::Evdev(c) => Arc::clone(c),
        _ => return None,
    };
    Some(client_read(&client, buf, nonblocking, task))
}

fn client_read(client: &Client, buf: &mut [u8], nonblocking: bool, task: &Task) -> Result<usize, Errno> {
    if buf.len() < EVENT_SIZE {
        return Err(EINVAL);
    }
    // Registered only once a look found nothing (then looked again before sleeping).
    let mut watching: Option<crate::poll::Watch> = None;
    loop {
        {
            let mut q = client.queue.lock();
            if !q.is_empty() {
                let mut n = 0;
                while n + EVENT_SIZE <= buf.len() {
                    let Some(e) = q.pop_front() else { break };
                    buf[n..n + EVENT_SIZE].copy_from_slice(&e);
                    n += EVENT_SIZE;
                }
                return Ok(n);
            }
        }
        if nonblocking {
            return Err(EAGAIN);
        }
        match watching.take() {
            None => watching = Some(crate::poll::watch(Some(vec![Arc::as_ptr(&client.device) as crate::poll::Key]))),
            Some(w) => w.wait(None, task)?,
        }
    }
}

impl Client {
    /// Readiness: readable while events are queued; writable always.
    #[must_use]
    pub fn readiness(&self) -> u32 {
        (if self.queue.lock().is_empty() { 0 } else { crate::poll::IN }) | crate::poll::OUT
    }

    #[must_use]
    pub fn device(&self) -> &Arc<Device> {
        &self.device
    }
}

/// `ioctl` on an input device: the `EVIOC*` requests `EventHub` makes of a device it opens.
///
/// # Errors
/// `ENOTTY` for a request an evdev device does not have; `ENOENT` for a string the device has none
/// of (its unique id); `EINVAL` for an axis it does not report.
pub fn ioctl(p: &Process, client: &Client, cmd: u64, arg: u64) -> SysResult {
    let device = &client.device;
    let (dir, size, kind, nr) = ((cmd >> 30) & 3, ((cmd >> 16) & 0x3fff) as usize, (cmd >> 8) & 0xff, (cmd & 0xff) as u16);
    if kind != u64::from(b'E') {
        return Err(ENOTTY);
    }
    const READ: u64 = 2;
    const WRITE: u64 = 1;
    // Copy `bytes` out, at most the caller's `size`; the driver answers the length copied.
    let copy_out = |bytes: &[u8]| -> SysResult {
        let n = bytes.len().min(size);
        p.mem.write(arg, &bytes[..n])?;
        Ok(n as u64)
    };
    let string = |s: &str| {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        b
    };
    match (dir, nr) {
        // EVIOCGVERSION: the driver's 1.0.1.
        (READ, 0x01) => p.mem.write_u32(arg, 0x01_0001).map(|()| 0),
        // EVIOCGID: struct input_id.
        (READ, 0x02) => {
            let mut id = [0u8; 8];
            for (i, v) in device.spec.id.iter().enumerate() {
                id[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
            p.mem.write(arg, &id).map(|()| 0)
        }
        // EVIOCGREP: no kernel repeat (Android repeats keys itself); EVIOCSREP: taken.
        (READ, 0x03) => p.mem.write(arg, &[0u8; 8]).map(|()| 0),
        (WRITE, 0x03) => Ok(0),
        // EVIOCGNAME, EVIOCGPHYS.
        (READ, 0x06) => copy_out(&string(&device.spec.name)),
        (READ, 0x07) => copy_out(&string(&format!("omnidroid/input{}", device.number))),
        // EVIOCGUNIQ: none.
        (READ, 0x08) => Err(ENOENT),
        // EVIOCGPROP: no properties (a pointer or a direct device would say so).
        (READ, 0x09) => copy_out(&[0u8; SMALL_BYTES]),
        // EVIOCGKEY: keys down now; EVIOCGLED, EVIOCGSND, EVIOCGSW: none on.
        (READ, 0x18) => copy_out(&device.down.lock()[..]),
        (READ, 0x19..=0x1b) => copy_out(&[0u8; SMALL_BYTES]),
        // EVIOCGBIT(type).
        (READ, 0x20..=0x3f) => copy_out(&device.bits(nr - 0x20)),
        // EVIOCGABS(axis): no absolute axes.
        (READ, 0x40..=0x7f) => Err(EINVAL),
        // EVIOCGRAB, EVIOCREVOKE, EVIOCSCLOCKID (events are stamped with CLOCK_MONOTONIC, the one
        // EventHub asks for).
        (WRITE, 0x90 | 0x91 | 0xa0) => Ok(0),
        _ => Err(ENOTTY),
    }
}

/// Open `/dev/input/event<n>`: `ENODEV` if there is no such device.
pub fn open(n: usize) -> Result<Arc<Client>, Errno> {
    device(n).map(|d| d.open()).ok_or(ENODEV)
}
