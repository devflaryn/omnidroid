//! The kernel's xtables (`iptables-legacy`'s `getsockopt`/`setsockopt` on a raw socket): the
//! tables `filter`, `nat`, `mangle` and `raw`, for IPv4 and IPv6, start with their built-in chains
//! (policy ACCEPT); `IPT_SO_GET_INFO` and `IPT_SO_GET_ENTRIES` read a table as it is,
//! `IPT_SO_SET_REPLACE` replaces its rules (handing back the old counters). netd installs its
//! rules so at boot. No packet flows here, so no rule is ever evaluated.
use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::errno::{Errno, SysResult, EINVAL, ENOENT};
use crate::process::Process;

pub const SO_GET_INFO: u64 = 64;
pub const SO_GET_ENTRIES: u64 = 65;
pub const SO_GET_REVISION_MATCH: u64 = 66;
pub const SO_GET_REVISION_TARGET: u64 = 67;
pub const SO_SET_REPLACE: u64 = 64;
pub const SO_SET_ADD_COUNTERS: u64 = 65;

/// `SOL_IP` (IPv4 tables) or `SOL_IPV6` (IPv6).
pub const SOL_IP: u64 = 0;
pub const SOL_IPV6: u64 = 41;

#[derive(Clone)]
struct Table {
    valid_hooks: u32,
    hook_entry: [u32; 5],
    underflow: [u32; 5],
    num_entries: u32,
    entries: Vec<u8>,
}

/// `sizeof(struct ipt_entry)` and `sizeof(struct ip6t_entry)` on a 64-bit kernel: where the
/// target begins; the offsets of `target_offset` and `next_offset` in it.
fn entry_layout(v6: bool) -> (usize, usize) {
    if v6 { (168, 140) } else { (112, 88) }
}

/// One entry matching everything, with a target: a standard verdict or an error name.
fn entry(v6: bool, target: &[u8]) -> Vec<u8> {
    let (size, offsets) = entry_layout(v6);
    let mut e = vec![0u8; size];
    let total = size + target.len();
    e[offsets..offsets + 2].copy_from_slice(&(size as u16).to_le_bytes());
    e[offsets + 2..offsets + 4].copy_from_slice(&(total as u16).to_le_bytes());
    e.extend_from_slice(target);
    e
}

/// `struct xt_standard_target` with a verdict (ACCEPT is `-NF_ACCEPT - 1` = -2): 40 bytes.
fn standard(verdict: i32) -> Vec<u8> {
    let mut t = vec![0u8; 40];
    t[0..2].copy_from_slice(&40u16.to_le_bytes());
    t[32..36].copy_from_slice(&verdict.to_le_bytes());
    t
}

/// `struct xt_error_target` naming `name`: 64 bytes.
fn error(name: &str) -> Vec<u8> {
    let mut t = vec![0u8; 64];
    t[0..2].copy_from_slice(&64u16.to_le_bytes());
    t[2..7].copy_from_slice(b"ERROR");
    t[32..32 + name.len()].copy_from_slice(name.as_bytes());
    t
}

/// A table as the kernel starts it: each built-in chain its policy, ACCEPT; then the end.
fn initial(name: &str, v6: bool) -> Option<Table> {
    let valid_hooks: u32 = match name {
        "filter" => 0b01110,
        "nat" => 0b11011,
        "mangle" => 0b11111,
        "raw" => 0b01001,
        _ => return None,
    };
    let mut t = Table { valid_hooks, hook_entry: [0; 5], underflow: [0; 5], num_entries: 0, entries: Vec::new() };
    for hook in 0..5 {
        if valid_hooks & (1 << hook) != 0 {
            let at = t.entries.len() as u32;
            t.hook_entry[hook] = at;
            t.underflow[hook] = at;
            t.entries.extend_from_slice(&entry(v6, &standard(-2)));
            t.num_entries += 1;
        }
    }
    t.entries.extend_from_slice(&entry(v6, &error("ERROR")));
    t.num_entries += 1;
    Some(t)
}

fn tables() -> &'static Mutex<HashMap<(bool, String), Table>> {
    static T: OnceLock<Mutex<HashMap<(bool, String), Table>>> = OnceLock::new();
    T.get_or_init(Mutex::default)
}

fn table(v6: bool, name: &str) -> Result<Table, Errno> {
    let mut tables = tables().lock();
    if let Some(t) = tables.get(&(v6, name.to_string())) {
        return Ok(t.clone());
    }
    let t = initial(name, v6).ok_or(ENOENT)?;
    tables.insert((v6, name.to_string()), t.clone());
    Ok(t)
}

fn name_at(bytes: &[u8]) -> String {
    let raw = &bytes[..bytes.len().min(32)];
    String::from_utf8_lossy(raw.split(|b| *b == 0).next().unwrap_or_default()).into_owned()
}

/// `getsockopt` on the xtables options: the answer's bytes (the caller writes at most its room).
pub fn get(p: &Process, level: u64, name: u64, val: u64, room: usize) -> Result<Vec<u8>, Errno> {
    let v6 = level == SOL_IPV6;
    match name {
        SO_GET_INFO => {
            let t = table(v6, &name_at(&p.mem.read(val, 32)?))?;
            // struct ipt_getinfo { name[32]; valid_hooks; hook_entry[5]; underflow[5]; num_entries; size }
            let mut out = p.mem.read(val, 32)?;
            let mut put = |v: u32| out.extend_from_slice(&v.to_le_bytes());
            put(t.valid_hooks);
            t.hook_entry.iter().for_each(|v| put(*v));
            t.underflow.iter().for_each(|v| put(*v));
            put(t.num_entries);
            put(t.entries.len() as u32);
            Ok(out)
        }
        SO_GET_ENTRIES => {
            // struct ipt_get_entries { name[32]; size; (pad) entries[] at 40 }
            let head = p.mem.read(val, 40)?;
            let t = table(v6, &name_at(&head))?;
            let size = u32::from_le_bytes(head[32..36].try_into().expect("4")) as usize;
            if size != t.entries.len() || room < 40 + size {
                return Err(EINVAL);
            }
            let mut out = head;
            out.extend_from_slice(&t.entries);
            Ok(out)
        }
        // Every match and target revision is supported.
        SO_GET_REVISION_MATCH | SO_GET_REVISION_TARGET => Ok(p.mem.read(val, room.min(30))?),
        _ => Ok(vec![0; 4]),
    }
}

/// `setsockopt` on the xtables options.
pub fn set(p: &Process, level: u64, name: u64, val: u64, len: usize) -> SysResult {
    let v6 = level == SOL_IPV6;
    match name {
        SO_SET_REPLACE => {
            // struct ipt_replace { name[32]; valid_hooks; num_entries; size; hook_entry[5];
            // underflow[5]; num_counters; (pad) counters*; entries[] at 96 }
            let head = p.mem.read(val, 96)?;
            let word = |at: usize| u32::from_le_bytes(head[at..at + 4].try_into().expect("4"));
            let name = name_at(&head);
            let old = table(v6, &name)?;
            let size = word(40) as usize;
            if 96 + size > len {
                return Err(EINVAL);
            }
            let mut hook_entry = [0u32; 5];
            let mut underflow = [0u32; 5];
            for h in 0..5 {
                hook_entry[h] = word(44 + 4 * h);
                underflow[h] = word(64 + 4 * h);
            }
            let (num_counters, counters) = (word(84) as usize, u64::from_le_bytes(head[88..96].try_into().expect("8")));
            if num_counters != old.num_entries as usize {
                return Err(EINVAL);
            }
            let entries = p.mem.read(val + 96, size)?;
            // The old rules' counters, all zero: no packet was counted.
            if counters != 0 {
                p.mem.write(counters, &vec![0u8; 16 * num_counters])?;
            }
            tables().lock().insert((v6, name), Table { valid_hooks: word(32), hook_entry, underflow, num_entries: word(36), entries });
            Ok(0)
        }
        _ => Ok(0),
    }
}
