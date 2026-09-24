//! **What the extraction cache costs a second process** -- the owner's multi-instance requirement,
//! "share read-only pages", measured across two real processes rather than inferred from one.
//!
//! D11's argument is that `libroblox.so`'s text and rodata are file-backed and read-only, so every
//! instance maps the same page-cache pages and pays nothing private for them. `loader_commit_linux`
//! shows the single-process half (a file view is not `VM_ACCOUNT`). This is the cross-process half:
//! the parent maps the cache entry execute-read and reads every page; a child process does the same
//! while the parent's view is still live; the child then reads its own `/proc/self/smaps` for that
//! view. If the pages are shared, the child's **PSS** for the view is half its RSS (two processes
//! share each page) and its private bytes are zero.
//!
//! A directory target so that `tests/windows_only.rs`, which accounts for every `tests/*.rs` by
//! name, does not see it. The fixture is the real cache entry, under `target/` on an ext4 disk --
//! not `/tmp`, which is tmpfs here and whose pages are shmem rather than page cache.
#![cfg(target_os = "linux")]

#[path = "../common/mod.rs"]
mod common;

use std::sync::Arc;

use omni_mem::{Backing, GuestSpace, MapExecutability, Placement, Protection};

const CHILD: &str = "OMNI_CACHE_SHARING_CHILD";

/// Map the whole cache entry execute-read into a fresh guest space and read every page. Returns the
/// space (which keeps the view alive), the view's address and length, and a checksum.
fn map_and_read(path: &std::path::Path) -> (GuestSpace, usize, usize, u64) {
    let backing: Arc<Backing> = Backing::open(path, MapExecutability::Executable).expect("open");
    let space = GuestSpace::new().expect("a guest space");
    let page = space.page_size();
    let len = (backing.len() as usize) / page * page;
    let address = space
        .map_file(&backing, 0, Placement::Anywhere { align: page }, len, Protection::ReadExecute)
        .expect("map the cache entry execute-read");
    let mut sum = 0u64;
    for offset in (0..len).step_by(page) {
        // SAFETY: the whole view is mapped readable.
        sum = sum.wrapping_add(u64::from(unsafe {
            core::ptr::read_volatile((address + offset) as *const u8)
        }));
    }
    (space, address, len, sum)
}

/// Sum the named `smaps` fields, in kB, over the VMAs inside `[address, address + len)`.
fn smaps_for(address: usize, len: usize) -> std::collections::BTreeMap<String, u64> {
    let text = std::fs::read_to_string("/proc/self/smaps").expect("read smaps");
    let mut totals = std::collections::BTreeMap::new();
    let mut inside = false;
    for line in text.lines() {
        let first = line.split_whitespace().next().unwrap_or("");
        if let Some((start, end)) = first.split_once('-') {
            if let (Ok(start), Ok(end)) =
                (usize::from_str_radix(start, 16), usize::from_str_radix(end, 16))
            {
                inside = start >= address && end <= address + len;
                continue;
            }
        }
        if !inside {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().trim_end_matches("kB").trim();
            if let Ok(kb) = value.parse::<u64>() {
                *totals.entry(key.to_string()).or_insert(0) += kb;
            }
        }
    }
    totals
}

#[test]
fn a_second_process_shares_the_cache_entrys_pages_and_pays_half_their_pss() {
    let path = common::cached_main_lib()
        .expect("the libroblox.so extraction-cache entry: a missing fixture is a failure");
    let (_space, _address, len, sum) = map_and_read(&path);

    let output = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--ignored", "--exact", "child_map_the_cache_entry_and_report", "--nocapture"])
        .env(CHILD, &path)
        .output()
        .expect("run the second process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the second process failed: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let line = stdout
        .lines()
        .find(|line| line.starts_with("SHARING "))
        .unwrap_or_else(|| panic!("the child printed no measurement: {stdout}"));
    let field = |name: &str| -> u64 {
        line.split_whitespace()
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no {name} in {line}"))
    };
    let (rss, pss, shared, private, commit, child_sum) = (
        field("rss_kb"),
        field("pss_kb"),
        field("shared_kb"),
        field("private_kb"),
        field("commit_delta_b"),
        field("checksum"),
    );
    println!(
        "a second process mapping the {:.1} MiB libroblox.so cache entry r-x, every page read, \
         while the first still maps it (n = 1 pair of processes; /proc/self/smaps): RSS {rss} kB, \
         PSS {pss} kB, Shared {shared} kB, Private {private} kB, VM_ACCOUNT delta {commit} B",
        len as f64 / (1024.0 * 1024.0)
    );
    assert_eq!(child_sum, sum, "the two processes read different bytes");
    assert!(rss * 1024 >= (len as u64) * 9 / 10, "the child did not have the view resident: {line}");
    assert_eq!(private, 0, "the child privatised pages of a read-only view: {line}");
    assert!(shared * 1024 >= (len as u64) * 9 / 10, "the pages are not shared: {line}");
    // Two processes map each page, so each is charged half of it. Allow a little for pages another
    // process (a sibling test binary, the page-cache readahead) might also have mapped.
    assert!(
        pss * 100 <= rss * 55,
        "PSS {pss} kB is more than 55% of RSS {rss} kB: the second process is paying for the pages \
         as if they were its own"
    );
    assert_eq!(commit, 0, "a read-only file view must cost no VM_ACCOUNT");
}

#[test]
#[ignore = "run as the second process by a_second_process_shares_the_cache_entrys_pages_and_pays_half_their_pss"]
fn child_map_the_cache_entry_and_report() {
    let path = std::env::var_os(CHILD).expect("run by the parent test, which names the file");
    let before = omni_mem::process_commit_charge().expect("commit charge") as i64;
    let (_space, address, len, sum) = map_and_read(std::path::Path::new(&path));
    let commit = omni_mem::process_commit_charge().expect("commit charge") as i64 - before;
    let totals = smaps_for(address, len);
    let get = |key: &str| totals.get(key).copied().unwrap_or(0);
    println!(
        "SHARING rss_kb={} pss_kb={} shared_kb={} private_kb={} commit_delta_b={commit} checksum={sum}",
        get("Rss"),
        get("Pss"),
        get("Shared_Clean") + get("Shared_Dirty"),
        get("Private_Clean") + get("Private_Dirty"),
    );
}
