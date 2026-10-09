//! `OMNI_GUEST_PROF=1` (with `OMNI_THREAD_CPU=<seconds>`): **which guest code** a hot thread's
//! translated-code samples run -- `lib+offset symbol pct%` per hot thread, and per library.
//!
//! [`crate::cpuprof`] classes a sample whose host instruction pointer is in a code cache as `jit`,
//! which says only "translated code" -- 60-90% of a world's busy threads. Here each such address is
//! taken back to the guest: host address -> the translated block holding it, in the shared code
//! cache (vendored patch 0036, `omni_cpu::stats::guest_pcs_of`) -> the block's guest PC -> the
//! guest mapping holding it (`Mm::name_at`: file and offset) -> the ELF there (a `.so` on its own,
//! or one stored in an APK, `extractNativeLibs=false`) -> the function: a `.symtab`/`.dynsym`
//! symbol that covers it, else the start of the `.eh_frame_hdr` function it is in (a stripped
//! library such as `libroblox.so` has ~245,000 of those and almost no symbols), printed `fn_<vaddr>`.
//! Offsets are ELF virtual addresses, what a disassembler shows.
//!
//! All of it runs on the report thread, once a period, after every sampled thread was resumed: the
//! samples themselves only add their host address to a list. The code cache is walked once per
//! report (its lock taken shared; a few ms at 500k blocks, during which no thread can translate),
//! and each library is parsed once per host process and kept.
//!
//! What it cannot see: code in a jit's own cache (`OMNI_JIT_SHARED_CACHE=0`, arm64 hosts), whose
//! samples count as `unresolved`, with the dispatcher, far code and blocks dropped between the
//! sample and the report; and the exact instruction -- a block's first PC stands for the block.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::process::Process;

/// Whether `OMNI_GUEST_PROF=1` asks for guest attribution of `jit` samples.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_GUEST_PROF").is_ok_and(|v| v.trim() == "1"))
}

/// One hot thread's `jit` samples over a period, as the report thread hands them over.
pub struct HotThread {
    pub tid: i32,
    pub name: Vec<u8>,
    /// Every sample of the period (all classes): what the percentages are of.
    pub samples: u32,
    /// The host instruction pointer of each `jit` sample.
    pub jit: Vec<u64>,
    pub process: Option<Weak<Process>>,
}

/// The most recent reports, for a test to read (`cpuprof` prints them as well).
static RECENT: Mutex<Vec<String>> = Mutex::new(Vec::new());
const RECENT_KEPT: usize = 8;

/// The last few `[guestprof]` reports this process made, oldest first.
pub fn recent_reports() -> Vec<String> {
    RECENT.lock().map(|r| r.clone()).unwrap_or_default()
}

/// Where a guest PC is: a library (or mapping name) and a function in it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Place {
    pub lib: String,
    /// The function's start (ELF virtual address), or the PC's own offset where no function is known.
    pub offset: u64,
    pub symbol: Option<String>,
}

/// Rows per thread.
const TOP: usize = 15;

/// The report for one period: per thread, its top functions and its libraries, then the libraries
/// and the thread names over every hot thread. Empty when no thread had a `jit` sample.
pub fn report(threads: &[HotThread], symbols: &mut Symbolizer) -> String {
    let mut all: Vec<u64> = threads.iter().flat_map(|t| t.jit.iter().copied()).collect();
    if all.is_empty() {
        return String::new();
    }
    all.sort_unstable();
    all.dedup();
    let started = std::time::Instant::now();
    let pcs = omni_cpu::stats::guest_pcs_of(&all);
    let resolve_ms = started.elapsed().as_secs_f64() * 1e3;
    let pc_of = |ip: u64| all.binary_search(&ip).ok().map(|i| pcs[i]).filter(|&pc| pc != u64::MAX);

    let mut out = String::new();
    let mut libs_all: HashMap<String, u32> = HashMap::new();
    let mut names: HashMap<String, (u32, u32)> = HashMap::new(); // samples, jit samples
    let mut samples_all = 0u32;
    for t in threads {
        let name = String::from_utf8_lossy(&t.name).into_owned();
        let n = names.entry(name.clone()).or_default();
        n.0 += t.samples;
        n.1 += t.jit.len() as u32;
        samples_all += t.samples;
        if t.jit.is_empty() {
            continue; // in the kernel or the host: `[thread-cpu]` already says where
        }
        let process = t.process.as_ref().and_then(Weak::upgrade);
        let mut places: HashMap<Place, u32> = HashMap::new();
        let mut libs: HashMap<String, u32> = HashMap::new();
        let mut cache: HashMap<u64, Place> = HashMap::new();
        let mut unresolved = 0u32;
        for &ip in &t.jit {
            let Some(pc) = pc_of(ip) else {
                unresolved += 1;
                continue;
            };
            let place = cache
                .entry(pc)
                .or_insert_with(|| match &process {
                    Some(p) => symbols.place(p, pc),
                    None => Place { lib: "[exited]".into(), offset: pc, symbol: None },
                })
                .clone();
            *libs.entry(place.lib.clone()).or_default() += 1;
            *places.entry(place).or_default() += 1;
        }
        if unresolved > 0 {
            *libs.entry("[unresolved]".into()).or_default() += unresolved;
        }
        for (lib, n) in &libs {
            *libs_all.entry(lib.clone()).or_default() += n;
        }
        let total = t.samples.max(1);
        let pct = |n: u32| f64::from(n) * 100.0 / f64::from(total);
        let _ = writeln!(
            out,
            "[guestprof] {} {name:?}: jit {:.0}% of {} samples, {:.0}% of them resolved",
            t.tid,
            pct(t.jit.len() as u32),
            t.samples,
            f64::from(t.jit.len() as u32 - unresolved) * 100.0 / f64::from((t.jit.len() as u32).max(1)),
        );
        let mut places: Vec<(Place, u32)> = places.into_iter().collect();
        places.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.offset.cmp(&b.0.offset)));
        for (p, n) in places.iter().take(TOP) {
            let _ = writeln!(out, "[guestprof]   {}+{:#x} {} {:.1}%", p.lib, p.offset, p.symbol.as_deref().unwrap_or("?"), pct(*n));
        }
        let _ = writeln!(out, "[guestprof]   libs: {}", shares(&libs, total, 8));
    }
    let _ = writeln!(out, "[guestprof] all hot threads, libs: {}", shares(&libs_all, samples_all.max(1), 12));
    let mut by_name: Vec<(String, (u32, u32))> = names.into_iter().collect();
    by_name.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    let named: Vec<String> = by_name
        .iter()
        .take(12)
        .map(|(n, (s, j))| format!("{n:?} {:.0}% (jit {:.0}%)", f64::from(*s) * 100.0 / f64::from(samples_all.max(1)), f64::from(*j) * 100.0 / f64::from(*s.max(&1))))
        .collect();
    let _ = writeln!(out, "[guestprof] thread names: {}", named.join(", "));
    let _ = writeln!(out, "[guestprof] {} distinct host addresses resolved in {resolve_ms:.1} ms", all.len());
    if let Ok(mut recent) = RECENT.lock() {
        if recent.len() == RECENT_KEPT {
            recent.remove(0);
        }
        recent.push(out.clone());
    }
    out
}

fn shares(counts: &HashMap<String, u32>, total: u32, top: usize) -> String {
    let mut v: Vec<(&String, &u32)> = counts.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    v.iter().take(top).map(|(l, n)| format!("{l} {:.1}%", f64::from(**n) * 100.0 / f64::from(total))).collect::<Vec<_>>().join(", ")
}

/// An APK's stored libraries: (data offset, size, entry name).
type StoredLibs = Vec<(u64, u64, String)>;

/// The libraries seen so far, parsed once each (kept for the life of the host process).
#[derive(Default)]
pub struct Symbolizer {
    /// (host file, ELF's offset in it) -> its function map, or `None` if it is not an ELF we read.
    elves: HashMap<(PathBuf, u64), Option<Arc<Elf>>>,
    /// An APK's stored libraries: (data offset, size, name).
    zips: HashMap<PathBuf, Arc<StoredLibs>>,
}

impl Symbolizer {
    /// Where guest `pc` of process `p` is.
    pub fn place(&mut self, p: &Process, pc: u64) -> Place {
        let Some((guest, file_off)) = p.mm.name_at(pc) else {
            return Place { lib: "[anon]".into(), offset: pc & !0xfff, symbol: None };
        };
        let short = String::from_utf8_lossy(guest.rsplit(|&c| c == b'/').next().unwrap_or(&guest)).into_owned();
        match host_file(p, &guest) {
            Some(host) => self.place_in_file(&host, &short, file_off),
            None => Place { lib: short, offset: file_off & !0xff, symbol: None },
        }
    }

    /// Where file offset `file_off` of host file `host` (guest name `short`) is.
    pub fn place_in_file(&mut self, host: &Path, short: &str, file_off: u64) -> Place {
        let (elf_start, lib) = if short.ends_with(".apk") {
            let zip = self.zips.entry(host.to_path_buf()).or_insert_with(|| Arc::new(zip_stored_libs(host).unwrap_or_default())).clone();
            match zip.iter().find(|(at, len, _)| file_off >= *at && file_off < at + len) {
                Some((at, _, name)) => (*at, format!("{short}!{}", name.rsplit('/').next().unwrap_or(name))),
                None => return Place { lib: short.to_string(), offset: file_off & !0xff, symbol: None },
            }
        } else {
            (0, short.to_string())
        };
        let elf = self
            .elves
            .entry((host.to_path_buf(), elf_start))
            .or_insert_with(|| File::open(host).ok().and_then(|mut f| Elf::parse(&mut f, elf_start)).map(Arc::new))
            .clone();
        let rel = file_off - elf_start;
        let Some(elf) = elf else { return Place { lib, offset: rel & !0xff, symbol: None } };
        let lib = elf.soname.clone().unwrap_or(lib);
        match elf.vaddr_of(rel) {
            Some(vaddr) => {
                let (offset, symbol) = elf.function(vaddr);
                Place { lib, offset, symbol }
            }
            None => Place { lib, offset: rel & !0xff, symbol: None },
        }
    }
}

/// The host file behind a guest path, if it is a regular file this runtime keeps on the host.
pub(crate) fn host_file(p: &Process, guest: &[u8]) -> Option<PathBuf> {
    use crate::vfs::Node;
    let r = p.vfs.resolve(b"/", guest, true).ok()?;
    match r.node {
        Node::SysFile { .. } => p.vfs.sysroot().host_path(&r.path),
        Node::HostFile { host } => Some(host),
        _ => None,
    }
}

/// What a library says about its functions.
#[derive(Debug, Default)]
pub struct Elf {
    pub soname: Option<String>,
    /// `PT_LOAD`s: (file offset, virtual address, file size).
    loads: Vec<(u64, u64, u64)>,
    /// Function symbols, by start: (start, size, name).
    symbols: Vec<(u64, u64, String)>,
    /// `.eh_frame_hdr` function starts, ascending.
    starts: Vec<u64>,
}

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_GNU_EH_FRAME: u32 = 0x6474_e550;
const SHT_SYMTAB: u32 = 2;
const SHT_DYNSYM: u32 = 11;
const DT_STRTAB: u64 = 5;
const DT_STRSZ: u64 = 10;
const DT_SONAME: u64 = 14;
/// A section or table larger than this is not read (a corrupt header, not a library).
const MAX_TABLE: u64 = 256 << 20;

fn u16le(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?)))
}
fn u32le(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?)))
}
fn u64le(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn read_at(f: &mut (impl Read + Seek), at: u64, len: u64) -> Option<Vec<u8>> {
    if len > MAX_TABLE {
        return None;
    }
    f.seek(SeekFrom::Start(at)).ok()?;
    let mut v = vec![0; len as usize];
    f.read_exact(&mut v).ok()?;
    Some(v)
}

fn c_str(b: &[u8], at: usize) -> Option<String> {
    let s = b.get(at..)?;
    let end = s.iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&s[..end]).into_owned())
}

impl Elf {
    /// Read the 64-bit little-endian ELF that starts at `start` in `f`: its loads, symbols,
    /// `.eh_frame_hdr` function starts and soname. `None` if it is not one.
    pub fn parse(f: &mut (impl Read + Seek), start: u64) -> Option<Self> {
        let h = read_at(f, start, 64)?;
        if h[..4] != *b"\x7fELF" || h[4] != 2 || h[5] != 1 {
            return None;
        }
        let (phoff, shoff) = (u64le(&h, 0x20)?, u64le(&h, 0x28)?);
        let (phentsize, phnum) = (u16le(&h, 0x36)?, u16le(&h, 0x38)?);
        let (shentsize, shnum) = (u16le(&h, 0x3a)?, u16le(&h, 0x3c)?);
        let mut elf = Self::default();
        let ph = read_at(f, start + phoff, phentsize * phnum)?;
        let mut dynamic = None;
        let mut eh = None;
        for i in 0..phnum as usize {
            let at = i * phentsize as usize;
            let kind = u32le(&ph, at)? as u32;
            let (off, vaddr, filesz) = (u64le(&ph, at + 8)?, u64le(&ph, at + 0x10)?, u64le(&ph, at + 0x20)?);
            match kind {
                PT_LOAD => elf.loads.push((off, vaddr, filesz)),
                PT_DYNAMIC => dynamic = Some((off, filesz)),
                PT_GNU_EH_FRAME => eh = Some((off, vaddr, filesz)),
                _ => {}
            }
        }
        if shoff != 0 && shentsize >= 64 && shnum > 0 {
            if let Some(sh) = read_at(f, start + shoff, shentsize * shnum) {
                let section = |i: u64| -> Option<(u32, u64, u64, u64)> {
                    let at = (i * shentsize) as usize;
                    Some((u32le(&sh, at + 4)? as u32, u64le(&sh, at + 0x18)?, u64le(&sh, at + 0x20)?, u32le(&sh, at + 0x28)?))
                };
                for i in 0..shnum {
                    let Some((kind, off, size, link)) = section(i) else { continue };
                    if kind != SHT_SYMTAB && kind != SHT_DYNSYM {
                        continue;
                    }
                    let Some((_, str_off, str_size, _)) = section(link) else { continue };
                    let (Some(syms), Some(strs)) = (read_at(f, start + off, size), read_at(f, start + str_off, str_size)) else { continue };
                    for s in syms.chunks_exact(24) {
                        let kind = s[4] & 0xf;
                        let shndx = u16le(s, 6).unwrap_or(0);
                        let (value, size) = (u64le(s, 8).unwrap_or(0), u64le(s, 16).unwrap_or(0));
                        // STT_FUNC or STT_GNU_IFUNC, defined.
                        if (kind == 2 || kind == 10) && shndx != 0 && value != 0 {
                            if let Some(name) = c_str(&strs, u32le(s, 0).unwrap_or(0) as usize).filter(|n| !n.is_empty()) {
                                elf.symbols.push((value, size, name));
                            }
                        }
                    }
                }
            }
        }
        elf.symbols.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
        elf.symbols.dedup_by_key(|s| s.0);
        if let Some((off, filesz)) = dynamic {
            elf.soname = elf.read_soname(f, start, off, filesz);
        }
        if let Some((off, vaddr, filesz)) = eh {
            elf.starts = read_at(f, start + off, filesz).and_then(|b| eh_frame_starts(&b, vaddr)).unwrap_or_default();
        }
        Some(elf)
    }

    fn read_soname(&self, f: &mut (impl Read + Seek), start: u64, off: u64, filesz: u64) -> Option<String> {
        let d = read_at(f, start + off, filesz)?;
        let (mut strtab, mut strsz, mut soname) = (None, None, None);
        for e in d.chunks_exact(16) {
            match u64le(e, 0)? {
                0 => break,
                DT_STRTAB => strtab = Some(u64le(e, 8)?),
                DT_STRSZ => strsz = Some(u64le(e, 8)?),
                DT_SONAME => soname = Some(u64le(e, 8)?),
                _ => {}
            }
        }
        let soname = soname?;
        let len = strsz?.checked_sub(soname)?.min(256);
        let name = read_at(f, start + self.file_of(strtab?)? + soname, len)?;
        c_str(&name, 0)
    }

    /// The virtual address of the function symbol named exactly `name`, if the library has one
    /// (patch: HLE resolves `memcpy`/`memset` implementations this way, from the symbol table of
    /// the mapped file rather than a hardcoded address).
    #[must_use]
    pub fn symbol(&self, name: &str) -> Option<u64> {
        self.symbols.iter().find(|s| s.2 == name).map(|s| s.0)
    }

    /// The file offset of virtual address `vaddr`.
    pub fn file_of(&self, vaddr: u64) -> Option<u64> {
        self.loads.iter().find(|(_, v, len)| vaddr >= *v && vaddr < v + len).map(|(o, v, _)| vaddr - v + o)
    }

    /// The virtual address of file offset `off` (relative to the ELF's start).
    pub fn vaddr_of(&self, off: u64) -> Option<u64> {
        self.loads.iter().find(|(o, _, len)| off >= *o && off < o + len).map(|(o, v, _)| off - o + v)
    }

    /// The function `vaddr` is in: a symbol that covers it, else the `.eh_frame_hdr` function it
    /// is in (named after a symbol that starts there, if any, else `fn_<start>`), else `vaddr`
    /// itself, unnamed.
    pub fn function(&self, vaddr: u64) -> (u64, Option<String>) {
        let i = self.symbols.partition_point(|s| s.0 <= vaddr);
        let sym = i.checked_sub(1).map(|i| &self.symbols[i]);
        if let Some((start, size, name)) = sym {
            if vaddr < start + size {
                return (*start, Some(name.clone()));
            }
        }
        let i = self.starts.partition_point(|&s| s <= vaddr);
        if let Some(&start) = i.checked_sub(1).map(|i| &self.starts[i]) {
            let named = sym.filter(|s| s.0 == start).map(|s| s.2.clone());
            return (start, Some(named.unwrap_or_else(|| format!("fn_{start:x}"))));
        }
        // A sized-0 symbol (hand-written assembly) is the best there is.
        if let Some((start, 0, name)) = sym {
            return (*start, Some(name.clone()));
        }
        (vaddr, None)
    }
}

/// The function starts in an `.eh_frame_hdr` at `hdr_vaddr`, from its binary-search table. Only
/// the table encoding every AArch64 toolchain emits (`DW_EH_PE_datarel | sdata4`, 0x3b) is read;
/// anything else gives `None` rather than a guess.
fn eh_frame_starts(b: &[u8], hdr_vaddr: u64) -> Option<Vec<u64>> {
    let (version, ptr_enc, count_enc, table_enc) = (*b.first()?, *b.get(1)?, *b.get(2)?, *b.get(3)?);
    if version != 1 || table_enc != 0x3b {
        return None;
    }
    let ptr_len = match ptr_enc & 0x0f {
        0x03 | 0x0b => 4,
        0x04 | 0x0c | 0x00 => 8,
        _ => return None,
    };
    let (count, count_len) = match count_enc & 0x0f {
        0x03 | 0x0b => (u32le(b, 4 + ptr_len)?, 4),
        0x04 | 0x0c | 0x00 => (u64le(b, 4 + ptr_len)?, 8),
        _ => return None,
    };
    let table = 4 + ptr_len + count_len;
    let mut out = Vec::with_capacity(count.min(1 << 22) as usize);
    for i in 0..count as usize {
        let at = table + i * 8;
        let rel = i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?);
        out.push(hdr_vaddr.wrapping_add(i64::from(rel) as u64));
    }
    out.sort_unstable();
    Some(out)
}

/// An APK's stored (uncompressed) `.so` entries: (data offset, size, name). The only way a library
/// is mapped straight out of an APK.
fn zip_stored_libs(path: &Path) -> Option<StoredLibs> {
    let mut f = File::open(path).ok()?;
    let len = f.seek(SeekFrom::End(0)).ok()?;
    let tail_len = len.min(66_000);
    let tail = read_at(&mut f, len - tail_len, tail_len)?;
    let eocd = (0..tail.len().saturating_sub(21)).rev().find(|&i| tail[i..i + 4] == *b"PK\x05\x06")?;
    let entries = u16le(&tail, eocd + 10)?;
    let (cd_size, cd_off) = (u32le(&tail, eocd + 12)?, u32le(&tail, eocd + 16)?);
    let cd = read_at(&mut f, cd_off, cd_size)?;
    let mut out = Vec::new();
    let mut at = 0usize;
    for _ in 0..entries {
        if cd.get(at..at + 4)? != b"PK\x01\x02" {
            break;
        }
        let method = u16le(&cd, at + 10)?;
        let size = u32le(&cd, at + 20)?;
        let (name_len, extra_len, comment_len) = (u16le(&cd, at + 28)? as usize, u16le(&cd, at + 30)? as usize, u16le(&cd, at + 32)? as usize);
        let local = u32le(&cd, at + 42)?;
        let name = String::from_utf8_lossy(cd.get(at + 46..at + 46 + name_len)?).into_owned();
        at += 46 + name_len + extra_len + comment_len;
        if method != 0 || !name.ends_with(".so") {
            continue;
        }
        let lh = read_at(&mut f, local, 30)?;
        if lh[..4] != *b"PK\x03\x04" {
            continue;
        }
        let data = local + 30 + u16le(&lh, 26)? + u16le(&lh, 28)?;
        out.push((data, size, name));
    }
    out.sort();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A minimal ELF: one PT_LOAD (file 0x0.. at vaddr 0x10000), a `.symtab` with `alpha` at
    /// 0x10100 (0x40 bytes) and `beta` at 0x10200 (0x100), and an `.eh_frame_hdr` naming functions
    /// at 0x10100, 0x10200 and 0x10400 (the last unnamed: a stripped function).
    fn tiny_elf() -> Vec<u8> {
        let mut b = vec![0u8; 0x1000];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        let put16 = |b: &mut Vec<u8>, at: usize, v: u16| b[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let put32 = |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let put64 = |b: &mut Vec<u8>, at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put64(&mut b, 0x20, 0x40); // phoff
        put64(&mut b, 0x28, 0x800); // shoff
        put16(&mut b, 0x36, 56);
        put16(&mut b, 0x38, 2);
        put16(&mut b, 0x3a, 64);
        put16(&mut b, 0x3c, 3);
        // PT_LOAD: offset 0, vaddr 0x10000, filesz 0x1000.
        put32(&mut b, 0x40, PT_LOAD);
        put64(&mut b, 0x48, 0);
        put64(&mut b, 0x50, 0x10000);
        put64(&mut b, 0x60, 0x1000);
        // PT_GNU_EH_FRAME at file 0x600 (vaddr 0x10600).
        put32(&mut b, 0x78, PT_GNU_EH_FRAME);
        put64(&mut b, 0x80, 0x600);
        put64(&mut b, 0x88, 0x10600);
        put64(&mut b, 0x98, 12 + 3 * 8);
        b[0x600..0x604].copy_from_slice(&[1, 0x1b, 0x03, 0x3b]);
        put32(&mut b, 0x608, 3);
        for (i, f) in [0x10100u64, 0x10200, 0x10400].iter().enumerate() {
            put32(&mut b, 0x60c + i * 8, (*f as i64 - 0x10600) as i32 as u32);
        }
        // Strings at 0x700, symbols at 0x740.
        b[0x700..0x70c].copy_from_slice(b"\0alpha\0beta\0");
        for (i, (name, value, size)) in [(1u32, 0x10100u64, 0x40u64), (7, 0x10200, 0x100)].iter().enumerate() {
            let at = 0x740 + (i + 1) * 24;
            put32(&mut b, at, *name);
            b[at + 4] = 2; // STT_FUNC
            put16(&mut b, at + 6, 1);
            put64(&mut b, at + 8, *value);
            put64(&mut b, at + 16, *size);
        }
        // Sections: [0] null, [1] .symtab (link 2), [2] .strtab.
        let sh = 0x800 + 64;
        put32(&mut b, sh + 4, SHT_SYMTAB);
        put64(&mut b, sh + 0x18, 0x740);
        put64(&mut b, sh + 0x20, 3 * 24);
        put32(&mut b, sh + 0x28, 2);
        let sh = 0x800 + 128;
        put32(&mut b, sh + 4, 3);
        put64(&mut b, sh + 0x18, 0x700);
        put64(&mut b, sh + 0x20, 12);
        b
    }

    #[test]
    fn an_address_is_named_by_the_symbol_or_the_unwind_table_function_it_is_in() {
        let elf = Elf::parse(&mut Cursor::new(tiny_elf()), 0).expect("an ELF");
        assert_eq!(elf.vaddr_of(0x123), Some(0x10123));
        assert_eq!(elf.function(0x10104), (0x10100, Some("alpha".into())));
        assert_eq!(elf.function(0x102f0), (0x10200, Some("beta".into())));
        // Past alpha's end but before beta: the unwind table's function is alpha's start.
        assert_eq!(elf.function(0x10150), (0x10100, Some("alpha".into())));
        // A stripped function: only the unwind table knows it.
        assert_eq!(elf.function(0x10480), (0x10400, Some("fn_10400".into())));
        // Before any function.
        assert_eq!(elf.function(0x10010), (0x10010, None));
    }

    #[test]
    fn a_library_stored_in_an_apk_is_found_at_its_offset_and_named_after_its_entry() {
        // A zip with one stored entry, lib/arm64-v8a/libhot.so, holding the tiny ELF.
        let elf = tiny_elf();
        let name = b"lib/arm64-v8a/libhot.so";
        let mut z = Vec::new();
        let lh = |z: &mut Vec<u8>| {
            z.extend_from_slice(b"PK\x03\x04");
            z.extend_from_slice(&[0; 22]);
            z.extend_from_slice(&(name.len() as u16).to_le_bytes());
            z.extend_from_slice(&0u16.to_le_bytes());
        };
        z.extend_from_slice(&[0u8; 100]); // something before it
        let local = z.len() as u32;
        lh(&mut z);
        let at = z.len();
        z[local as usize + 18..local as usize + 22].copy_from_slice(&(elf.len() as u32).to_le_bytes());
        z[local as usize + 22..local as usize + 26].copy_from_slice(&(elf.len() as u32).to_le_bytes());
        z.extend_from_slice(name);
        let data = z.len();
        z.extend_from_slice(&elf);
        let _ = at;
        let cd = z.len() as u32;
        z.extend_from_slice(b"PK\x01\x02");
        z.extend_from_slice(&[0; 6]); // versions, flags
        z.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        z.extend_from_slice(&[0; 8]); // time, date, crc
        z.extend_from_slice(&(elf.len() as u32).to_le_bytes());
        z.extend_from_slice(&(elf.len() as u32).to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&[0; 12]); // extra, comment, disk, attrs
        z.extend_from_slice(&local.to_le_bytes());
        z.extend_from_slice(name);
        let cd_size = z.len() as u32 - cd;
        z.extend_from_slice(b"PK\x05\x06");
        z.extend_from_slice(&[0; 4]);
        z.extend_from_slice(&1u16.to_le_bytes());
        z.extend_from_slice(&1u16.to_le_bytes());
        z.extend_from_slice(&cd_size.to_le_bytes());
        z.extend_from_slice(&cd.to_le_bytes());
        z.extend_from_slice(&[0; 2]);
        let dir = std::env::temp_dir().join(format!("omni-guestprof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let apk = dir.join("base.apk");
        std::fs::write(&apk, &z).unwrap();

        let libs = zip_stored_libs(&apk).expect("the central directory");
        assert_eq!(libs, vec![(data as u64, elf.len() as u64, "lib/arm64-v8a/libhot.so".to_string())]);
        let mut s = Symbolizer::default();
        // File offset of vaddr 0x10204 (beta) inside the APK.
        let place = s.place_in_file(&apk, "base.apk", data as u64 + 0x204);
        assert_eq!(place, Place { lib: "base.apk!libhot.so".into(), offset: 0x10200, symbol: Some("beta".into()) });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
