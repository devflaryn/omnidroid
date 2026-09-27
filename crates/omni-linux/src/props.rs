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
const OVERLAY: [(&str, &str); 10] = [
    // The runtime this device offers (AndroidRuntime turns these into ART's -Xgc: and -Xusejit:):
    // CMC, the collector the boot image is compiled for, in its userfaultfd-less fallback mode;
    // no JIT until its code cache (memfd, a writable and an executable view) is supported.
    ("dalvik.vm.gctype", "CMC"),
    ("dalvik.vm.usejit", "false"),
    ("ro.product.cpu.abi", "arm64-v8a"),
    ("ro.product.cpu.abilist", "arm64-v8a"),
    ("ro.product.cpu.abilist64", "arm64-v8a"),
    ("ro.product.cpu.abilist32", ""),
    ("ro.debuggable", "0"),
    ("ro.secure", "1"),
    ("ro.hardware", "omnidroid"),
    ("ro.boot.hardware", "omnidroid"),
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

    fn add(&mut self, name: &str, value: &str) {
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
    }

    fn finish(self) -> Vec<u8> {
        let mut out = vec![0u8; HEADER];
        out[0..4].copy_from_slice(&(self.data.len() as u32).to_le_bytes()); // bytes_used
        out[8..12].copy_from_slice(&PROP_AREA_MAGIC.to_le_bytes());
        out[12..16].copy_from_slice(&PROP_AREA_VERSION.to_le_bytes());
        out.extend_from_slice(&self.data);
        let size = out.len().div_ceil(AREA_UNIT).max(1) * AREA_UNIT;
        out.resize(size, 0);
        out
    }
}
