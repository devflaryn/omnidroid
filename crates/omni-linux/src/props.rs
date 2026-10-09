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

/// The build properties the image's partitions set, in init's load order. The vendor partition's
/// are the image's too: init runs that partition's HALs (KeyMint, audio, codecs), which read them
/// (its patch level, its heap sizes); what omnidroid sets as this device's vendor (`OVERLAY`) is
/// applied over all of them.
const BUILD_PROPS: [&str; 4] = ["/system/build.prop", "/system_ext/etc/build.prop", "/vendor/build.prop", "/product/etc/build.prop"];

/// What omnidroid, as the vendor, sets on top.
const OVERLAY: [(&str, &str); 18] = [
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
    // What a bootloader hands init (`androidboot.*`): omnidroid boots the image unverified, as an
    // unlocked device does, and reports the pinned image's vbmeta as its `VerifiedBootParams.textproto`
    // gives it (KeyMint waits for these before it registers).
    ("ro.boot.verifiedbootstate", "orange"),
    ("ro.boot.vbmeta.device_state", "unlocked"),
    ("ro.boot.vbmeta.digest", "836f26adcab3883794ba405c6bf019f74afbdc3c9d76bdb26cb1ea1672ffa8e8"),
    ("ro.boot.vbmeta.hash_alg", "sha256"),
    ("ro.boot.vbmeta.size", "6720"),
    // The GPU drivers (`ro.hardware.egl`, `ro.hardware.vulkan`) are the backend's
    // (`crate::gpu::backend::properties`), set after these.
    // The display (the host composer's, D3): 160 dpi, as the D design reports it.
    ("ro.sf.lcd_density", "160"),
    // A slow device's timeouts: Android's own scale for them (`Build.HW_TIMEOUT_MULTIPLIER`,
    // `android::base::HwTimeoutMultiplier`), which the input dispatcher's 5 s and ActivityManager's
    // ANR timeouts are multiplied by, as on an emulator or Cuttlefish. Translated, an app's start
    // can hold its main thread past 5 s: in PS99 Roblox's `LauncherAliasMain` did (2 and 4 ANRs in
    // two runs, 2026-09-29), and the "isn't responding" dialog then took the pointer and the keys
    // (a space pressed its "Close app").
    ("ro.hw_timeout_multiplier", "5"),
];

/// `ro.hw_timeout_multiplier` for a host with `cpus` logical CPUs (`asked`: `OMNI_HW_TIMEOUT_MULTIPLIER`),
/// or none (Android's own 1). Android scales its timeouts by it (`Build.HW_TIMEOUT_MULTIPLIER`: an
/// app's startup, input dispatch, broadcasts), as emulator images set it for a device slower than a
/// phone. MEASURED (Linux, i5-4460, 4 cores, 2026-09-29): Roblox's start was killed twice at the
/// stock 15 s ("failed to complete startup") while system_server held a core; the Windows host
/// (24 threads) never hit it.
#[must_use]
pub fn timeout_multiplier(cpus: usize, asked: Option<&str>) -> Option<u32> {
    let m = match asked.and_then(|a| a.trim().parse::<u32>().ok()) {
        Some(m) => m,
        None if cpus >= 12 => 1,
        None if cpus >= 8 => 2,
        None => 5,
    };
    (m > 1).then_some(m)
}

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
        // `OMNI_BOOT_IMAGE_UNCOMPRESSED=1`: the boot image is mapped where it was compiled for, not
        // relocated (`crate::boot_image`).
        if crate::boot_image::enabled() {
            // `OMNI_BOOT_IMAGE_VERBOSE=1`: ART says how it loads each image (`-verbose:image`).
            let verbose = if std::env::var("OMNI_BOOT_IMAGE_VERBOSE").as_deref() == Ok("1") { " -verbose:image" } else { "" };
            let opts = format!("{} {}{verbose}", self.get("dalvik.vm.extra-opts").unwrap_or_default(), crate::boot_image::NO_RELOCATE);
            self.set("dalvik.vm.extra-opts", opts.trim());
        } else if crate::jit_snapshot::fixed_layout() {
            // Translation snapshots with `OMNI_JIT_SNAPSHOT_LIB_ZONE=1`: the boot image -- and with it
            // boot.oat, the framework's compiled code that every Java process runs -- at the address
            // it was compiled for, not moved by a random delta each boot (measured: boot.oat at
            // 0x71494000 in one run, 0x71e28000 in the next), so its restored blocks verify.
            let opts = format!("{} {}", self.get("dalvik.vm.extra-opts").unwrap_or_default(), crate::boot_image::NO_RELOCATE);
            self.set("dalvik.vm.extra-opts", opts.trim());
        }
        // The GPU drivers: on Vulkan (D3a) the image's own ANGLE for GLES on omnidroid's Vulkan
        // driver (`/vendor/lib64/hw/vulkan.omni.so`); on GL omnidroid's GLES driver
        // (`/vendor/lib64/egl/libGLES_omni.so`) and no Vulkan. Both forward to the host's GPU.
        for (k, v) in crate::gpu::backend::properties(crate::gpu::backend::backend()) {
            self.set(k, v);
        }
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        if let Some(m) = timeout_multiplier(cpus, std::env::var("OMNI_HW_TIMEOUT_MULTIPLIER").ok().as_deref()) {
            self.set("ro.hw_timeout_multiplier", &m.to_string());
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

/// [`serial_area_bytes`] as a [`PropBlob`]: its 128-byte header, and its length.
#[must_use]
pub fn serial_area_blob() -> PropBlob {
    let area = Area::new();
    PropBlob { head: area.head(0), len: area.mapped_len(0) }
}

/// **A prop area file's bytes, kept without its zero tail.** A prop area is mostly room to grow:
/// the live one ~61 KiB of header and properties in 1.125 MiB (1 MiB of room, whole 128 KiB
/// units), `properties_serial` a 128-byte header in 128 KiB. Every guest process kept both whole
/// for its `/dev/__properties__` files (`crate::procfs::PropFiles`), written zeros and all
/// (`Vec::resize`): ~1.25 MiB of resident memory a process, ~1.2 MiB of it zeros -- the system's
/// host process's 65 processes ~81 MiB, its 1.25 MiB allocations (census, 2026-10-09). The bytes
/// are the same: [`head`](Self::head) and zeros up to [`len`](Self::len).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropBlob {
    head: Vec<u8>,
    len: usize,
}

impl PropBlob {
    /// A blob of exactly `bytes` (kept whole: the `OMNI_PROP_FULL_COPY=1` arm, and small files).
    #[must_use]
    pub fn whole(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        Self { head: bytes, len }
    }

    /// The bytes up to where only zeros follow (they may end in zeros themselves).
    #[must_use]
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// The file's length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the file is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The file's bytes: the head, then zeros.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len);
        out.extend_from_slice(&self.head);
        out.resize(self.len, 0);
        out
    }
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
        let mut out = self.head(serial);
        out.resize(self.mapped_len(capacity), 0);
        out
    }

    /// [`bytes`](Self::bytes) without the zeros after the data.
    fn head(&self, serial: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.data.len());
        out.resize(HEADER, 0);
        out[0..4].copy_from_slice(&(self.data.len() as u32).to_le_bytes()); // bytes_used
        out[4..8].copy_from_slice(&serial.to_le_bytes());
        out[8..12].copy_from_slice(&PROP_AREA_MAGIC.to_le_bytes());
        out[12..16].copy_from_slice(&PROP_AREA_VERSION.to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// The length [`bytes`](Self::bytes) has at `capacity`.
    fn mapped_len(&self, capacity: usize) -> usize {
        ((HEADER + self.data.len()).div_ceil(AREA_UNIT).max(1) * AREA_UNIT).max(capacity)
    }
}

/// A property that names Magisk.
fn is_root_prop(name: &str) -> bool {
    name.to_ascii_lowercase().contains("magisk")
}

/// The property service: init's, for every process of an instance. The prop area only grows,
/// as bionic's does -- readers keep pointers into it -- and every change is written into every
/// process's mapping of it, so a `setprop` in one process is what `getprop` reads in another.
pub struct PropertyService {
    live: parking_lot::Mutex<Live>,
    /// Notified after every change, for init's `wait_for_prop`.
    changed: parking_lot::Condvar,
    /// Held from a change's snapshot until every mapping has it: changes reach the mappings in the
    /// order they were made. Two setters publishing unordered let an older snapshot land last --
    /// a property went back to its old value and serial in a process's mapping, and a waiter on
    /// that serial (vold's `WaitForProperty` for `selinux.restorecon_recursive`) slept for good;
    /// system_server's Watchdog then killed it after 65 s in `IVold.prepareUserStorage` (r14).
    publish: parking_lot::Mutex<()>,
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
        Self::global_with_view(sysroot, &crate::root::ProcessView::default())
    }

    /// As `global`, built with `view` if this is the first call in the host process (the OnceLock
    /// keeps the first). Each app runs in its own host process, so its area is its own: `spawn_as`
    /// calls this with the app's view before anything else builds the service.
    pub fn global_with_view(sysroot: &Sysroot, view: &crate::root::ProcessView) -> std::sync::Arc<Self> {
        static SERVICE: std::sync::OnceLock<std::sync::Arc<PropertyService>> = std::sync::OnceLock::new();
        std::sync::Arc::clone(SERVICE.get_or_init(|| std::sync::Arc::new(Self::build(sysroot, view))))
    }

    /// A fresh property service for testing (not behind OnceLock).
    #[cfg(test)]
    fn for_test(sysroot: &Sysroot) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::build(sysroot, &crate::root::ProcessView::default()))
    }

    /// A fresh service built with a spoofed view (the `emu-hide` Pixel props applied).
    #[cfg(test)]
    fn for_test_spoofed(sysroot: &Sysroot) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::build(sysroot, &crate::root::ProcessView { spoofed: true, ..Default::default() }))
    }

    /// The service as `view` sees it. A hidden process gets no Magisk/root props (none are added
    /// today -- su and magisk live in the root layer -- so this guards that invariant).
    fn build(sysroot: &Sysroot, view: &crate::root::ProcessView) -> Self {
        let (props, _) = Properties::from_sysroot(sysroot);
        Self::build_from(props, view)
    }

    /// The one place `view` shapes the prop set (the hidden removal now; Task 5's spoof overrides
    /// for `view.spoofed` go here too).
    fn build_from(mut props: Properties, view: &crate::root::ProcessView) -> Self {
        // A spoofed process reads a Pixel: the overrides replace the values here, before the area
        // is frozen (an existing `ro.*` cannot be re-set at runtime).
        if view.spoofed {
            for (k, v) in crate::root::spoof::pixel_overrides() {
                props.set(k, v);
            }
        }
        let dropped = |k: &str| view.spoofed && crate::root::spoof::removals().iter().any(|p| k.starts_with(p));
        let mut area = Area::new();
        let mut info = HashMap::new();
        for (k, v) in props.entries.iter().filter(|(k, _)| !(view.hidden && is_root_prop(k)) && !dropped(k)) {
            info.insert(k.clone(), area.add(k, v));
        }
        // Room to grow: what the build set, and a megabyte more for what the system sets.
        let capacity = (HEADER + area.data.len() + (1 << 20)).div_ceil(AREA_UNIT) * AREA_UNIT;
        Self {
            live: parking_lot::Mutex::new(Live { area, info, serial: 0, capacity, mappings: Vec::new() }),
            changed: parking_lot::Condvar::new(),
            publish: parking_lot::Mutex::new(()),
        }
    }

    /// The prop area as it is now, at its full mapped size.
    #[must_use]
    pub fn area_bytes(&self) -> Vec<u8> {
        let live = self.live.lock();
        live.area.bytes(live.serial, live.capacity)
    }

    /// [`area_bytes`](Self::area_bytes) as a [`PropBlob`], never built with its zero tail.
    #[must_use]
    pub fn area_blob(&self) -> PropBlob {
        let live = self.live.lock();
        PropBlob { head: live.area.head(live.serial), len: live.area.mapped_len(live.capacity) }
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

    /// Every property and its value.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, String)> {
        let live = self.live.lock();
        live.info.keys().filter_map(|k| Self::value(&live, k).map(|v| (k.clone(), v))).collect()
    }

    /// A property's value, as `getprop` reads it.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        Self::value(&self.live.lock(), name)
    }

    fn value(live: &Live, name: &str) -> Option<String> {
        let at = *live.info.get(name)?;
        let serial = live.area.u32_at(at);
        let bytes = if serial & LONG_FLAG != 0 {
            let long = at + live.area.u32_at(at + 4 + 56) as usize;
            let end = live.area.data[long..].iter().position(|b| *b == 0).map_or(live.area.data.len(), |n| long + n);
            &live.area.data[long..end]
        } else {
            &live.area.data[at + 4..at + 4 + (serial >> 24) as usize]
        };
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    /// init's `wait_for_prop`: wait until `name` is `value` (`*` for any value), at most
    /// `timeout`. Whether it is.
    pub fn wait_for(&self, name: &str, value: &str, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut live = self.live.lock();
        loop {
            if Self::value(&live, name).is_some_and(|v| value == "*" || v == value) {
                return true;
            }
            if self.changed.wait_until(&mut live, deadline).timed_out() {
                return Self::value(&live, name).is_some_and(|v| value == "*" || v == value);
            }
        }
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
        self.set_inner(name, value, false)
    }

    fn set_inner(&self, name: &str, value: &str, force: bool) -> u32 {
        let _publishing = self.publish.lock();
        let mut live = self.live.lock();
        let changed_info = match live.info.get(name).copied() {
            Some(_) if name.starts_with("ro.") && !force => return PROP_ERROR_READ_ONLY_PROPERTY,
            Some(at) => {
                // Long values (>= PROP_VALUE_MAX) cannot be stored in-place. Route through append.
                if value.len() >= PROP_VALUE_MAX {
                    if HEADER + live.area.data.len() + 512 + value.len() > live.capacity {
                        return PROP_ERROR_INVALID_VALUE; // the area is full
                    }
                    let new_at = live.area.add(name, value);
                    live.info.insert(name.to_string(), new_at);
                    new_at
                } else {
                    let old = live.area.u32_at(at);
                    // Clear LONG_FLAG when rewriting to a short value (mask 0x00fe_fffe clears bit 16).
                    let serial = ((value.len() as u32) << 24) | (old.wrapping_add(2) & 0x00fe_fffe);
                    live.area.data[at + 4..at + 4 + PROP_VALUE_MAX].fill(0);
                    live.area.data[at + 4..at + 4 + value.len()].copy_from_slice(value.as_bytes());
                    live.area.set_u32(at, serial);
                    at
                }
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
        // The area only grows, so past its header and data every mapping holds zeros already: only
        // that much is written (~61 KiB, not the ~1.1 MiB capacity -- which, written into every
        // mapping at every change, cost ~88 MB of copying a `setprop` and committed each mapping's
        // zero tail again). `OMNI_PROP_FULL_COPY=1`: all of it, as before.
        let used = if crate::mm::full_prop_copy() { area.len() } else { (HEADER + live.area.data.len()).min(area.len()) };
        let serial_area = Area::new().bytes(live.serial, 0);
        live.mappings.retain(|(p, _, _)| p.strong_count() > 0);
        let targets: Vec<_> = live.mappings.iter().filter_map(|(p, at, s)| p.upgrade().map(|p| (p, *at, *s))).collect();
        drop(live);
        self.changed.notify_all();
        for (p, at, serial) in targets {
            let bytes = if serial { &serial_area[..HEADER] } else { &area[..used] };
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

    /// Set a property, bypassing the read-only guard for `ro.*` properties.
    pub fn set_forced(&self, name: &str, value: &str) -> u32 {
        self.set_inner(name, value, true)
    }

    /// Remove a property so `get` returns `None`. Returns whether it existed.
    /// Models deletion as "empty value + host-invisible": the in-area record is overwritten with
    /// an empty value before removal from the host info map, so guest readers observe the change.
    pub fn delete(&self, name: &str) -> bool {
        let _publishing = self.publish.lock();
        let mut live = self.live.lock();
        let at = match live.info.get(name).copied() {
            Some(at) => at,
            None => return false,
        };
        // Overwrite the in-area record to empty so guests see the deletion.
        // Clear LONG_FLAG when rewriting to empty (mask 0x00fe_fffe clears bit 16).
        let old = live.area.u32_at(at);
        let serial = (0u32 << 24) | (old.wrapping_add(2) & 0x00fe_fffe);
        live.area.data[at + 4..at + 4 + PROP_VALUE_MAX].fill(0);
        live.area.set_u32(at, serial);
        // Remove from host-side info after the area is updated.
        live.info.remove(name);
        live.serial = live.serial.wrapping_add(1);
        let area = live.area.bytes(live.serial, live.capacity);
        let used = if crate::mm::full_prop_copy() { area.len() } else { (HEADER + live.area.data.len()).min(area.len()) };
        let serial_area = Area::new().bytes(live.serial, 0);
        live.mappings.retain(|(p, _, _)| p.strong_count() > 0);
        let targets: Vec<_> = live.mappings.iter().filter_map(|(p, at, s)| p.upgrade().map(|p| (p, *at, *s))).collect();
        drop(live);
        self.changed.notify_all();
        for (p, at, serial) in targets {
            let bytes = if serial { &serial_area[..HEADER] } else { &area[..used] };
            if p.mm.kernel_write(&p.mem, at, bytes).is_ok() {
                let _ = p.futexes.wake(at + 4, i32::MAX as u64, u32::MAX);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn test_manifest() -> Manifest {
        let mut manifest = Manifest::default();
        // Minimal manifest with just the root directory
        manifest.entries.insert(b"/".to_vec(), crate::manifest::Entry::Dir { mode: 0o755 });
        manifest
    }

    /// What `PropertyService::set` relies on to write only `HEADER + data` into each mapping: past
    /// them, the area as mapped is zeros (the area only grows, so every mapping holds zeros there).
    #[test]
    fn past_the_header_and_data_an_area_is_zeros() {
        let mut area = Area::new();
        for i in 0..200 {
            area.add(&format!("test.prop.number{i}"), &format!("value {i}"));
        }
        let used = HEADER + area.data.len();
        let bytes = area.bytes(7, 1 << 20);
        assert!(bytes.len() >= 1 << 20);
        assert!(bytes[used..].iter().all(|&b| b == 0), "a non-zero byte past {used}");
        assert!(bytes[..used].iter().any(|&b| b != 0));
        assert!(used < bytes.len() / 4, "{used} of {}", bytes.len());
    }

    #[test]
    fn spoof_overrides_existing_ro_props_at_build_time() {
        let root = crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), test_manifest());
        let svc = PropertyService::for_test_spoofed(&root);
        assert_eq!(svc.get("ro.product.model").as_deref(), Some("Pixel 8"));
        assert_eq!(svc.get("ro.hardware").as_deref(), Some("zuma"));
        assert_eq!(svc.get("ro.boot.hardware").as_deref(), Some("zuma"));
    }

    #[test]
    fn spoof_replaces_seeded_emulator_values_and_drops_qemu_keys() {
        let mut props = Properties::default();
        for (k, v) in [
            ("ro.product.model", "sdk_gphone64_arm64"),
            ("ro.build.fingerprint", "generic/sdk_gphone64_arm64/emu:14/X/1:userdebug/test-keys"),
            ("ro.build.tags", "test-keys"),
            ("ro.kernel.qemu", "1"),
            ("ro.kernel.qemu.foo", "x"),
            ("ro.boot.qemu.avd_name", "a"),
            ("ro.boot.verifiedbootstate", "orange"),
        ] {
            props.set(k, v);
        }
        let svc = PropertyService::build_from(props.clone(), &crate::root::ProcessView { spoofed: true, ..Default::default() });
        assert_eq!(svc.get("ro.product.model").as_deref(), Some("Pixel 8"));
        assert!(svc.get("ro.build.fingerprint").unwrap().starts_with("google/shiba/"));
        assert_eq!(svc.get("ro.build.tags").as_deref(), Some("release-keys"));
        assert_eq!(svc.get("ro.boot.verifiedbootstate").as_deref(), Some("green"));
        for k in ["ro.kernel.qemu", "ro.kernel.qemu.foo", "ro.boot.qemu.avd_name"] {
            assert_eq!(svc.get(k), None, "{k} must be gone");
        }
        let plain = PropertyService::build_from(props, &crate::root::ProcessView::default());
        assert_eq!(plain.get("ro.kernel.qemu").as_deref(), Some("1"));
    }

    #[test]
    fn spoof_is_per_process_and_scoped() {
        let root = crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), test_manifest());
        let spoofed = PropertyService::for_test_spoofed(&root);
        let plain = PropertyService::for_test(&root);
        assert_eq!(spoofed.get("ro.hardware").as_deref(), Some("zuma"));
        assert_eq!(plain.get("ro.hardware").as_deref(), Some("omnidroid"), "non-spoofed keeps the original");
        assert_ne!(plain.get("ro.product.model").as_deref(), Some("Pixel 8"));
        assert_eq!(plain.get("ro.boot.hardware").as_deref(), Some("omnidroid"));
    }

    #[test]
    fn a_hidden_view_builds_no_magisk_props() {
        let mut props = Properties::default();
        props.set("ro.magisk.version", "27000");
        props.set("ro.build.tags", "release-keys");
        let has = |view: &crate::root::ProcessView| {
            let svc = PropertyService::build_from(props.clone(), view);
            (svc.get("ro.magisk.version"), svc.get("ro.build.tags"))
        };
        let (marker, other) = has(&crate::root::ProcessView { hidden: true, ..Default::default() });
        assert_eq!((marker, other.as_deref()), (None, Some("release-keys")), "hidden: marker gone, the rest kept");
        let (marker, _) = has(&crate::root::ProcessView::default());
        assert_eq!(marker.as_deref(), Some("27000"), "not hidden: byte-identical, marker kept");
    }

    #[test]
    fn set_forced_overrides_a_read_only_property_and_delete_removes_it() {
        let root = crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), test_manifest());
        let svc = PropertyService::for_test(&root);
        // An existing ro.* cannot be changed by set...
        let name = "ro.build.tags";
        assert_eq!(svc.set(name, "release-keys"), PROP_SUCCESS);
        assert_eq!(svc.set(name, "test-keys"), PROP_ERROR_READ_ONLY_PROPERTY);
        // ...but set_forced changes it.
        assert_eq!(svc.set_forced(name, "test-keys"), PROP_SUCCESS);
        assert_eq!(svc.get(name).as_deref(), Some("test-keys"));
        assert!(svc.delete(name));
        assert_eq!(svc.get(name), None);
    }

    #[test]
    fn set_forced_long_value_on_existing_property_does_not_corrupt_neighbors() {
        let root = crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), test_manifest());
        let svc = PropertyService::for_test(&root);
        // Set two short properties.
        let name1 = "ro.test.prop1";
        let name2 = "ro.test.prop2";
        assert_eq!(svc.set(name1, "short1"), PROP_SUCCESS);
        assert_eq!(svc.set(name2, "short2"), PROP_SUCCESS);
        // Overwrite name1 with a long value via set_forced (length >= PROP_VALUE_MAX).
        let long_value = "x".repeat(100); // 100 bytes, >= PROP_VALUE_MAX (92)
        assert_eq!(svc.set_forced(name1, &long_value), PROP_SUCCESS);
        // Verify name1 has the full long value.
        assert_eq!(svc.get(name1).as_deref(), Some(long_value.as_str()));
        // Verify name2 is still intact (not corrupted by the long value write).
        assert_eq!(svc.get(name2).as_deref(), Some("short2"));
    }

    #[test]
    fn a_kept_prop_area_is_the_same_bytes_without_its_zero_tail() {
        let root = crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), test_manifest());
        let svc = PropertyService::for_test(&root);
        assert_eq!(svc.set("ro.test.kept", "v"), PROP_SUCCESS);
        assert_eq!(svc.set("sys.test.kept", "w"), PROP_SUCCESS); // a serial past 0
        let blob = svc.area_blob();
        let whole = svc.area_bytes();
        assert_eq!(blob.len(), whole.len());
        assert_eq!(blob.to_bytes(), whole, "the same bytes");
        assert!(whole.len() >= 1 << 20, "the room to grow is there: {}", whole.len());
        assert!(blob.head().len() < whole.len() / 4, "kept without the room: {} of {}", blob.head().len(), whole.len());
        assert!(whole[blob.head().len()..].iter().all(|&b| b == 0), "only zeros dropped");

        let serial = serial_area_blob();
        assert_eq!(serial.to_bytes(), serial_area_bytes());
        assert!(serial.head().len() < 4096, "a header and an empty trie: {}", serial.head().len());
        assert_eq!(PropBlob::whole(vec![1, 0, 0]).to_bytes(), vec![1, 0, 0]);
    }

    #[test]
    fn delete_of_missing_property_returns_false() {
        let root = crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), test_manifest());
        let svc = PropertyService::for_test(&root);
        assert!(!svc.delete("ro.nonexistent.prop"));
    }

    #[test]
    fn delete_makes_get_return_none_and_advances_serial() {
        let root = crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), test_manifest());
        let svc = PropertyService::for_test(&root);
        let name = "ro.test.deletable";
        assert_eq!(svc.set(name, "value"), PROP_SUCCESS);
        assert_eq!(svc.get(name).as_deref(), Some("value"));
        // Get the area before delete.
        let area_before = svc.area_bytes();
        let serial_before = u32::from_le_bytes(area_before[4..8].try_into().unwrap());
        // Delete the property.
        assert!(svc.delete(name));
        assert_eq!(svc.get(name), None);
        // Verify serial advanced.
        let area_after = svc.area_bytes();
        let serial_after = u32::from_le_bytes(area_after[4..8].try_into().unwrap());
        assert!(serial_after > serial_before);
    }

    #[test]
    fn set_forced_long_then_short_clears_long_flag() {
        let root = crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), test_manifest());
        let svc = PropertyService::for_test(&root);
        let name = "ro.test.longshort";
        // First, set_forced a long value (>= 92 bytes) to create a long-form record.
        let long_value = "x".repeat(100);
        assert_eq!(svc.set_forced(name, &long_value), PROP_SUCCESS);
        assert_eq!(svc.get(name).as_deref(), Some(long_value.as_str()));
        // Now overwrite in-place with a short value.
        let short_value = "short";
        assert_eq!(svc.set_forced(name, short_value), PROP_SUCCESS);
        // Verify the short value is returned (not garbage from stale long-value offset).
        assert_eq!(svc.get(name).as_deref(), Some(short_value));
        // Verify via get returning the exact short value with no trailing garbage
        // (indicating LONG_FLAG is clear in the in-area record).
        assert_eq!(svc.get(name).unwrap().len(), short_value.len());
        assert_eq!(svc.get(name).unwrap(), short_value);
    }

    #[test]
    fn delete_long_form_record_clears_long_flag() {
        let root = crate::vfs::Sysroot::from_manifest(&std::env::temp_dir(), test_manifest());
        let svc = PropertyService::for_test(&root);
        let name = "ro.test.dellong";
        // Set a long value to create a long-form record.
        let long_value = "y".repeat(100);
        assert_eq!(svc.set_forced(name, &long_value), PROP_SUCCESS);
        assert_eq!(svc.get(name).as_deref(), Some(long_value.as_str()));
        // Delete it.
        assert!(svc.delete(name));
        // Verify get returns None (not garbage from stale long-value offset).
        assert_eq!(svc.get(name), None);
        // After deletion, the in-area record has an empty value and LONG_FLAG clear,
        // so no guest reading it would see long-form garbage.
    }
}