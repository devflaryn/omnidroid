//! System properties, written in bionic's own formats (milestone A3), so the real `libc.so` maps and
//! reads them unmodified: `/dev/__properties__/property_info` (the context trie), one prop area per
//! context (here one: [`CONTEXT`]) and `properties_serial`.
//!
//! The formats are Android 15's `bionic/libc/system_properties` (`prop_area`, `prop_bt`,
//! `prop_info`) and `system/core/property_service/libpropertyinfoparser` (`PropertyInfoAreaHeader`,
//! `TrieNodeInternal`, `PropertyEntry`). Writes are allocated as bionic's own `prop_area::add`
//! allocates them, 4-aligned from the data start, so any reader of that code finds them.
use std::collections::HashMap;

use crate::vfs::Sysroot;

/// The one context every property lives in.
pub const CONTEXT: &str = "u:object_r:default_prop:s0";

const PROP_AREA_MAGIC: u32 = 0x504f_5250;
const PROP_AREA_VERSION: u32 = 0xfc6e_d0ab;
const HEADER: usize = 128;
const PROP_BT: usize = 20;
const PROP_VALUE_MAX: usize = 92;
const PROP_INFO: usize = 4 + PROP_VALUE_MAX;
const AREA_UNIT: usize = 128 << 10;
const LONG_FLAG: u32 = 1 << 16;
const LONG_LEGACY_ERROR: &[u8] = b"Must use __system_property_read_callback() to read";

/// The build properties the image's generic partitions set, in init's load order. `/vendor` is
/// deliberately absent: this image's vendor partition is the emulator's (`ro.kernel.qemu=1`,
/// `ranchu` HALs), and omnidroid is this device's vendor.
const BUILD_PROPS: [&str; 3] = ["/system/build.prop", "/system_ext/etc/build.prop", "/product/etc/build.prop"];

/// What omnidroid, as the vendor, sets on top.
const OVERLAY: [(&str, &str); 14] = [
    // The property service speaks protocol 2 (a reply for every set).
    ("ro.property_service.version", "2"),
    // The runtime this device offers, as the options AndroidRuntime adds from
    // dalvik.vm.extra-opts: CMC, the collector the boot image is compiled for, in its
    // userfaultfd-less fallback mode; no JIT until its code cache (memfd, a writable and an
    // executable view) is supported.
    ("dalvik.vm.extra-opts", "-Xgc:CMC -Xusejit:false"),
    ("dalvik.vm.usejit", "false"),
    ("ro.product.cpu.abi", "arm64-v8a"),
    ("ro.product.cpu.abilist", "arm64-v8a"),
    ("ro.product.cpu.abilist64", "arm64-v8a"),
    ("ro.product.cpu.abilist32", ""),
    ("ro.debuggable", "0"),
    ("ro.secure", "1"),
    ("ro.hardware", "omnidroid"),
    ("ro.boot.hardware", "omnidroid"),
    // The GPU drivers (D3a): the image's own ANGLE for GLES, on omnidroid's Vulkan driver
    // (`/vendor/lib64/hw/vulkan.omni.so`), which forwards to the host's GPU.
    ("ro.hardware.egl", "angle"),
    ("ro.hardware.vulkan", "omni"),
    // The display (the host composer's, D3): 160 dpi, as the D design reports it.
    ("ro.sf.lcd_density", "160"),
];

/// `key=value` lines as init reads a `build.prop`: comments, blanks, `import` lines and lines
/// without `=` are skipped; key and value are trimmed; the value keeps any further `=`.
#[must_use]
pub fn parse_build_prop(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("import "))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty() && !k.contains(char::is_whitespace))
        .collect()
}

#[derive(Default, Debug, Clone)]
pub struct Properties {
    entries: Vec<(String, String)>,
    index: HashMap<String, usize>,
}

impl Properties {
    /// The sysroot's build properties with the overlay applied, and the names dropped because
    /// bionic cannot hold them.
    #[must_use]
    pub fn from_sysroot(sysroot: &Sysroot) -> (Self, Vec<String>) {
        let mut p = Self::default();
        for path in BUILD_PROPS {
            if let Some(host) = sysroot.host_path(path.as_bytes()) {
                if let Ok(text) = std::fs::read_to_string(host) {
                    p.load(&parse_build_prop(&text));
                }
            }
        }
        p.derive_as_init();
        p.apply_overlay();
        let dropped = p.drop_unrepresentable();
        (p, dropped)
    }

    pub fn set(&mut self, name: &str, value: &str) {
        match self.index.get(name) {
            Some(&i) => self.entries[i].1 = value.to_string(),
            None => {
                self.index.insert(name.to_string(), self.entries.len());
                self.entries.push((name.to_string(), value.to_string()));
            }
        }
    }

    pub fn load(&mut self, pairs: &[(String, String)]) {
        for (k, v) in pairs {
            self.set(k, v);
        }
    }

    /// What init derives at boot (`property_initialize_ro_product_props`,
    /// `property_derive_build_fingerprint`): each `ro.product.X` from the first partition in the
    /// default source order that sets `ro.product.<partition>.X`, and `ro.build.fingerprint` from
    /// its parts when no file sets it.
    pub fn derive_as_init(&mut self) {
        const SOURCES: [&str; 5] = ["product", "odm", "vendor", "system_ext", "system"];
        for field in ["brand", "device", "manufacturer", "model", "name"] {
            let key = format!("ro.product.{field}");
            if self.get(&key).is_some() {
                continue;
            }
            let found = SOURCES
                .iter()
                .find_map(|source| self.get(&format!("ro.product.{source}.{field}")).map(str::to_string));
            if let Some(value) = found {
                self.set(&key, &value);
            }
        }
        if self.get("ro.build.fingerprint").is_none() {
            let part = |k: &str| self.get(k).unwrap_or("").to_string();
            let fingerprint = format!(
                "{}/{}/{}:{}/{}/{}:{}/{}",
                part("ro.product.brand"),
                part("ro.product.name"),
                part("ro.product.device"),
                part("ro.build.version.release_or_codename"),
                part("ro.build.id"),
                part("ro.build.version.incremental"),
                part("ro.build.type"),
                part("ro.build.tags"),
            );
            self.set("ro.build.fingerprint", &fingerprint);
        }
    }

    pub fn apply_overlay(&mut self) {
        for (k, v) in OVERLAY {
            self.set(k, v);
        }
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.index.get(name).map(|&i| self.entries[i].1.as_str())
    }

    /// Remove what bionic cannot represent: a value of `PROP_VALUE_MAX` bytes or more outside `ro.`
    /// (only read-only properties have the long form). Returns the names removed.
    pub fn drop_unrepresentable(&mut self) -> Vec<String> {
        let dropped: Vec<String> = self
            .entries
            .iter()
            .filter(|(k, v)| v.len() >= PROP_VALUE_MAX && !k.starts_with("ro."))
            .map(|(k, _)| k.clone())
            .collect();
        self.entries.retain(|(k, _)| !dropped.contains(k));
        self.index = self.entries.iter().enumerate().map(|(i, (k, _))| (k.clone(), i)).collect();
        dropped
    }

    /// The prop area holding every property, sized up to a whole number of 128 KiB.
    #[must_use]
    pub fn area_bytes(&self) -> Vec<u8> {
        let mut area = Area::new();
        for (k, v) in &self.entries {
            area.add(k, v);
        }
        area.finish()
    }
}

/// An empty prop area: `properties_serial`, whose `serial` bionic watches.
#[must_use]
pub fn serial_area_bytes() -> Vec<u8> {
    Area::new().finish()
}

/// The serialized `property_info`: one context, one type, a root node that maps every name to
/// context 0.
#[must_use]
pub fn property_info_bytes() -> Vec<u8> {
    const HEADER_LEN: u32 = 24;
    let contexts_offset = HEADER_LEN; // [count, offset]
    let types_offset = contexts_offset + 8; // [count, offset]
    let entry_offset = types_offset + 8; // PropertyEntry, 16 bytes
    let root_offset = entry_offset + 16; // TrieNodeInternal, 28 bytes
    let strings = root_offset + 28;
    let context_at = strings;
    let type_at = context_at + CONTEXT.len() as u32 + 1;
    let root_name_at = type_at + "string".len() as u32 + 1;
    let size = root_name_at + "root".len() as u32 + 1;
    let words: [u32; 21] = [
        1, 1, size, contexts_offset, types_offset, root_offset, // header
        1, context_at, // contexts
        1, type_at, // types
        root_name_at, 4, 0, 0, // PropertyEntry { name_offset, namelen, context_index, type_index }
        entry_offset, 0, 0, 0, 0, 0, 0, // TrieNodeInternal: entry, children, prefixes, exact matches
    ];
    let mut out: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    for s in [CONTEXT, "string", "root"] {
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    }
    debug_assert_eq!(out.len(), size as usize);
    out
}

/// A prop area being written: `data` is everything after the 128-byte header.
struct Area {
    data: Vec<u8>,
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// bionic's `cmp_prop_name`: the shorter name first, then the bytes.
fn cmp_prop_name(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

impl Area {
    fn new() -> Self {
        // The root `prop_bt` (empty name) and, after it, the dirty-backup area bionic reserves.
        Self { data: vec![0u8; PROP_BT + align4(PROP_VALUE_MAX)] }
    }

    fn u32_at(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.data[at..at + 4].try_into().expect("four bytes"))
    }

    fn set_u32(&mut self, at: usize, value: u32) {
        self.data[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn allocate(&mut self, size: usize) -> usize {
        let at = self.data.len();
        self.data.resize(at + align4(size), 0);
        at
    }

    fn new_bt(&mut self, name: &[u8]) -> u32 {
        let at = self.allocate(PROP_BT + name.len() + 1);
        self.set_u32(at, name.len() as u32);
        self.data[at + PROP_BT..at + PROP_BT + name.len()].copy_from_slice(name);
        at as u32
    }

    fn bt_name(&self, at: usize) -> &[u8] {
        let len = self.u32_at(at) as usize;
        &self.data[at + PROP_BT..at + PROP_BT + len]
    }

    /// The child of `parent` named `name`, created where bionic's `find_prop_bt` would put it.
    fn child(&mut self, parent: usize, name: &[u8]) -> usize {
        const CHILDREN: usize = 16;
        const LEFT: usize = 8;
        const RIGHT: usize = 12;
        let first = self.u32_at(parent + CHILDREN);
        if first == 0 {
            let node = self.new_bt(name);
            self.set_u32(parent + CHILDREN, node);
            return node as usize;
        }
        let mut node = first as usize;
        loop {
            let side = match cmp_prop_name(name, self.bt_name(node)) {
                std::cmp::Ordering::Equal => return node,
                std::cmp::Ordering::Less => LEFT,
                std::cmp::Ordering::Greater => RIGHT,
            };
            let next = self.u32_at(node + side);
            if next == 0 {
                let created = self.new_bt(name);
                self.set_u32(node + side, created);
                return created as usize;
            }
            node = next as usize;
        }
    }

    fn add(&mut self, name: &str, value: &str) -> usize {
        let mut current = 0usize;
        for part in name.split('.') {
            current = self.child(current, part.as_bytes());
        }
        let info = self.allocate(PROP_INFO + name.len() + 1);
        self.data[info + PROP_INFO..info + PROP_INFO + name.len()].copy_from_slice(name.as_bytes());
        let value = value.as_bytes();
        if value.len() < PROP_VALUE_MAX {
            self.set_u32(info, (value.len() as u32) << 24);
            self.data[info + 4..info + 4 + value.len()].copy_from_slice(value);
        } else {
            // The long form (`ro.*` only): the value after the `prop_info`, found by an offset
            // relative to it at byte 56 of `value`; `value` holds the legacy error text.
            let long = self.allocate(value.len() + 1);
            self.data[long..long + value.len()].copy_from_slice(value);
            self.set_u32(info, ((LONG_LEGACY_ERROR.len() as u32) << 24) | LONG_FLAG);
            self.data[info + 4..info + 4 + LONG_LEGACY_ERROR.len()].copy_from_slice(LONG_LEGACY_ERROR);
            self.set_u32(info + 4 + 56, (long - info) as u32);
        }
        self.set_u32(current + 4, info as u32); // prop
        info
    }

    fn finish(self) -> Vec<u8> {
        self.bytes(0, 0)
    }

    /// The area as mapped: the header (`bytes_used`, `serial`, magic, version), the data, and
    /// zeros up to `capacity` (at least a whole number of 128 KiB).
    fn bytes(&self, serial: u32, capacity: usize) -> Vec<u8> {
        let mut out = vec![0u8; HEADER];
        out[0..4].copy_from_slice(&(self.data.len() as u32).to_le_bytes()); // bytes_used
        out[4..8].copy_from_slice(&serial.to_le_bytes());
        out[8..12].copy_from_slice(&PROP_AREA_MAGIC.to_le_bytes());
        out[12..16].copy_from_slice(&PROP_AREA_VERSION.to_le_bytes());
        out.extend_from_slice(&self.data);
        let size = out.len().div_ceil(AREA_UNIT).max(1) * AREA_UNIT;
        out.resize(size.max(capacity), 0);
        out
    }
}

/// The property service: init's, for every process of an instance. The prop area only grows,
/// as bionic's does -- readers keep pointers into it -- and every change is written into every
/// process's mapping of it, so a `setprop` in one process is what `getprop` reads in another.
pub struct PropertyService {
    live: parking_lot::Mutex<Live>,
}

struct Live {
    area: Area,
    /// name -> offset of its `prop_info` in the area's data.
    info: HashMap<String, usize>,
    serial: u32,
    capacity: usize,
    mappings: Vec<(std::sync::Weak<crate::process::Process>, u64, bool)>,
}

/// `PROP_ERROR_*` answers of `setprop`.
pub const PROP_SUCCESS: u32 = 0;
pub const PROP_ERROR_READ_ONLY_PROPERTY: u32 = 0x0b;
pub const PROP_ERROR_INVALID_NAME: u32 = 0x10;
pub const PROP_ERROR_INVALID_VALUE: u32 = 0x14;

impl PropertyService {
    /// The service of this host process, started from the first process's sysroot.
    pub fn global(sysroot: &Sysroot) -> std::sync::Arc<Self> {
        static SERVICE: std::sync::OnceLock<std::sync::Arc<PropertyService>> = std::sync::OnceLock::new();
        std::sync::Arc::clone(SERVICE.get_or_init(|| {
            let (props, _) = Properties::from_sysroot(sysroot);
            let mut area = Area::new();
            let mut info = HashMap::new();
            for (k, v) in &props.entries {
                info.insert(k.clone(), area.add(k, v));
            }
            // Room to grow: what the build set, and a megabyte more for what the system sets.
            let capacity = (HEADER + area.data.len() + (1 << 20)).div_ceil(AREA_UNIT) * AREA_UNIT;
            std::sync::Arc::new(Self { live: parking_lot::Mutex::new(Live { area, info, serial: 0, capacity, mappings: Vec::new() }) })
        }))
    }

    /// The prop area as it is now, at its full mapped size.
    #[must_use]
    pub fn area_bytes(&self) -> Vec<u8> {
        let live = self.live.lock();
        live.area.bytes(live.serial, live.capacity)
    }

    /// A process mapped the prop area (`serial` false) or `properties_serial` (`serial` true)
    /// at `at`: changes are written there from now on.
    pub fn watch(&self, process: std::sync::Weak<crate::process::Process>, at: u64, serial: bool) {
        self.live.lock().mappings.push((process, at, serial));
    }

    /// A process maps the prop area (`serial` false) or `properties_serial` (`serial` true) at
    /// `at`: `fill` writes the area as it is now into the mapping, and every later change is
    /// written there too. Both happen under the service's lock, so no change falls between the
    /// bytes a process starts from and the changes it is sent (a process spawned before a daemon
    /// sets a property, and mapping the area after, sees it).
    ///
    /// # Errors
    /// What `fill` fails with; the mapping is then not watched.
    pub fn attach<E>(&self, process: std::sync::Weak<crate::process::Process>, at: u64, serial: bool, fill: impl FnOnce(&[u8]) -> Result<(), E>) -> Result<(), E> {
        let mut live = self.live.lock();
        let bytes = if serial { Area::new().bytes(live.serial, 0) } else { live.area.bytes(live.serial, live.capacity) };
        fill(&bytes)?;
        live.mappings.push((process, at, serial));
        Ok(())
    }

    /// `setprop`: a new property is added, an existing one changed; `ro.*` is set once.
    pub fn set(&self, name: &str, value: &str) -> u32 {
        // Control messages are init's, not properties.
        if name.starts_with("ctl.") {
            if let Some(init) = crate::init::current() {
                let (n, v) = (name.to_string(), value.to_string());
                // Not on the caller's thread: starting a process must not wait on its sender.
                std::thread::spawn(move || {
                    init.control(&n, &v);
                });
            }
            return PROP_SUCCESS;
        }
        if name.is_empty() || name.len() >= 256 || name.contains(char::is_whitespace) {
            return PROP_ERROR_INVALID_NAME;
        }
        if value.len() >= PROP_VALUE_MAX && !name.starts_with("ro.") {
            return PROP_ERROR_INVALID_VALUE;
        }
        let mut live = self.live.lock();
        let changed_info = match live.info.get(name).copied() {
            Some(_) if name.starts_with("ro.") => return PROP_ERROR_READ_ONLY_PROPERTY,
            Some(at) => {
                let old = live.area.u32_at(at);
                let serial = ((value.len() as u32) << 24) | (old.wrapping_add(2) & 0x00ff_fffe);
                live.area.data[at + 4..at + 4 + PROP_VALUE_MAX].fill(0);
                live.area.data[at + 4..at + 4 + value.len()].copy_from_slice(value.as_bytes());
                live.area.set_u32(at, serial);
                at
            }
            None => {
                if HEADER + live.area.data.len() + 512 + value.len() > live.capacity {
                    return PROP_ERROR_INVALID_VALUE; // the area is full
                }
                let at = live.area.add(name, value);
                live.info.insert(name.to_string(), at);
                at
            }
        };
        live.serial = live.serial.wrapping_add(1);
        let area = live.area.bytes(live.serial, live.capacity);
        let serial_area = Area::new().bytes(live.serial, 0);
        live.mappings.retain(|(p, _, _)| p.strong_count() > 0);
        let targets: Vec<_> = live.mappings.iter().filter_map(|(p, at, s)| p.upgrade().map(|p| (p, *at, *s))).collect();
        drop(live);
        for (p, at, serial) in targets {
            let bytes = if serial { &serial_area[..HEADER] } else { &area[..] };
            if p.mm.kernel_write(&p.mem, at, bytes).is_ok() {
                // Wake whoever waits on the area's serial or on this property's.
                let _ = p.futexes.wake(at + 4, i32::MAX as u64, u32::MAX);
                if !serial {
                    let _ = p.futexes.wake(at + (HEADER + changed_info) as u64, i32::MAX as u64, u32::MAX);
                }
            }
        }
        PROP_SUCCESS
    }
}
