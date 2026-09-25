//! `OMNI_MEM_REPORT`'s table over a guest space whose contents the test made: the engine's heap
//! (labelled with a call site inside a registered library), a GLES shadow, and an unlabelled
//! runtime mapping -- measured through the real OS census, so what each row says is checked
//! against what was committed and touched.

use std::time::Duration;

use omni_android::memreport::{self, Image, ENGINE_MMAP, GLES_SHADOWS};
use omni_mem::{label_scope, CommitPolicy, GuestSpace, GuestSpaceConfig, MapLabel, Placement, Protection};

const MIB: usize = 1 << 20;

/// `(mapped, committed, resident, private, count)` of the row whose owner is `owner`.
fn row(text: &str, side: &str, owner: &str) -> (f64, f64, f64, f64, usize) {
    let line = text
        .lines()
        .find(|line| {
            line.strip_prefix("MEMREPORT   ")
                .and_then(|rest| rest.strip_prefix(side))
                .is_some_and(|rest| rest.trim_start().starts_with(owner))
        })
        .unwrap_or_else(|| panic!("no `{side} {owner}` row in:\n{text}"));
    let fields: Vec<&str> = line.split_whitespace().rev().take(5).collect();
    let number = |i: usize| fields[i].parse::<f64>().unwrap_or_else(|_| panic!("{line}"));
    (number(4), number(3), number(2), number(1), fields[0].parse().expect("a count"))
}

fn touch(address: usize, len: usize) {
    for offset in (0..len).step_by(4096) {
        // SAFETY: the caller committed `[address, address + len)` read-write.
        unsafe { ((address + offset) as *mut u8).write_volatile(1) };
    }
}

#[test]
fn each_owner_row_reports_what_was_committed_and_touched_under_its_label() {
    let space = GuestSpace::with_config(GuestSpaceConfig { size: 1 << 30, ..GuestSpaceConfig::default() })
        .expect("a guest space");
    let anywhere = Placement::Anywhere { align: space.page_size() };
    let library = Image { name: "libroblox.so".into(), base: 0x7000_0000, len: 0x100_0000 };
    let heap = {
        let _label = label_scope(MapLabel::at(ENGINE_MMAP, (library.base + 0x22_7310) as u64));
        space.map_anonymous(anywhere, 8 * MIB, Protection::ReadWrite, CommitPolicy::Lazy).unwrap()
    };
    // The engine touches 2 MiB of it: the pager would commit granules on the faults; here the
    // test commits them as the pager does, then touches them.
    space.ensure_committed(heap, 2 * MIB).unwrap();
    touch(heap, 2 * MIB);
    let shadow = {
        let _label = label_scope(MapLabel::new(GLES_SHADOWS));
        space.map_anonymous(anywhere, MIB, Protection::ReadWrite, CommitPolicy::Eager).unwrap()
    };
    touch(shadow, MIB / 2);
    let runtime = space.map_anonymous(anywhere, 64 * 1024, Protection::ReadWrite, CommitPolicy::Eager).unwrap();
    touch(runtime, 64 * 1024);

    let text = memreport::report(&space, &[library], None, Duration::from_secs(60));
    // What the owner reads in a live run's log; `--nocapture` shows it.
    eprintln!("{text}");
    assert!(text.lines().all(|line| line.starts_with("MEMREPORT")), "{text}");
    assert!(text.starts_with("MEMREPORT +60s: private (commit charge) "), "{text}");

    let (mapped, committed, resident, private, count) = row(&text, "guest", ENGINE_MMAP);
    assert_eq!((mapped, committed, count), (8.0, 2.0, 1), "{text}");
    assert!(text.contains("from libroblox.so+0x227310: 2.0 MiB committed"), "{text}");
    let gles = row(&text, "guest", GLES_SHADOWS);
    assert_eq!((gles.0, gles.1, gles.4), (1.0, 1.0, 1), "{text}");
    let runtime_row = row(&text, "guest", "runtime structures (unlabelled)");
    assert_eq!(runtime_row.1, 0.1, "64 KiB, printed to one decimal: {text}");
    assert!(text.contains("the engine's own count: no memProfStorage file yet"), "{text}");

    if cfg!(any(windows, target_os = "linux")) {
        // Residency is the OS's answer, page for page.
        assert_eq!((resident, private), (2.0, 2.0), "{text}");
        assert_eq!((gles.2, gles.3), (0.5, 0.5), "{text}");
        // The host side is there, and the test binary's own stacks and heap are in it.
        let stacks = row(&text, "host", "host thread stacks");
        assert!(stacks.4 >= 1 && stacks.2 > 0.0, "{text}");
        let other = row(&text, "host", "other private");
        assert!(other.1 > 0.0, "{text}");
        let images = row(&text, "host", "images");
        assert!(images.4 >= 1 && images.2 > 0.0, "{text}");
        assert!(text.contains("guest space: 3.1 MiB committed per its region map, 3.1 MiB private committed per the OS"), "{text}");
    } else {
        assert!(text.contains("host side missing"), "{text}");
    }
}
