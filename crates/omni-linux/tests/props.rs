//! System properties in bionic's own formats (milestone A3), checked by a reader written from
//! bionic's `prop_area::find_property` and `find_prop_bt`.
use omni_linux::props::{self, Properties};

const HEADER: usize = 128;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// bionic's `cmp_prop_name`: the shorter name first, then the bytes.
fn cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// `prop_bt` at `off` (relative to the data start): (namelen, prop, left, right, children, name).
fn bt(data: &[u8], off: u32) -> (u32, u32, u32, u32, u32, &[u8]) {
    let o = off as usize;
    let namelen = u32_at(data, o);
    let name = &data[o + 20..o + 20 + namelen as usize];
    (namelen, u32_at(data, o + 4), u32_at(data, o + 8), u32_at(data, o + 12), u32_at(data, o + 16), name)
}

/// bionic's lookup: one binary tree per `.`-separated level, from the root `prop_bt` at 0.
fn find(area: &[u8], name: &str) -> Option<String> {
    let data = &area[HEADER..];
    let mut current = 0u32;
    let parts: Vec<&str> = name.split('.').collect();
    for part in parts {
        let children = bt(data, current).4;
        if children == 0 {
            return None;
        }
        let mut node = children;
        loop {
            let (_, _, left, right, _, bt_name) = bt(data, node);
            match cmp(part.as_bytes(), bt_name) {
                std::cmp::Ordering::Equal => break,
                std::cmp::Ordering::Less if left != 0 => node = left,
                std::cmp::Ordering::Greater if right != 0 => node = right,
                _ => return None,
            }
        }
        current = node;
    }
    let prop = bt(data, current).1;
    if prop == 0 {
        return None;
    }
    let info = prop as usize;
    let serial = u32_at(data, info);
    if serial & (1 << 16) != 0 {
        let offset = u32_at(data, info + 4 + 56) as usize;
        let start = info + offset;
        let end = start + data[start..].iter().position(|&b| b == 0).unwrap();
        return Some(String::from_utf8(data[start..end].to_vec()).unwrap());
    }
    let len = (serial >> 24) as usize;
    Some(String::from_utf8(data[info + 4..info + 4 + len].to_vec()).unwrap())
}

#[test]
fn build_prop_lines_are_parsed_as_init_parses_them() {
    let parsed = props::parse_build_prop(
        "# a comment\n\nimport /vendor/x.prop\nro.a=1\n  ro.b = two words \nro.c=x=y\nnot a property\n",
    );
    assert_eq!(
        parsed,
        [("ro.a".into(), "1".into()), ("ro.b".into(), "two words".into()), ("ro.c".into(), "x=y".into())]
    );
}

#[test]
fn a_later_file_and_then_the_overlay_win() {
    let mut p = Properties::default();
    p.load(&props::parse_build_prop("ro.x=system\nro.product.cpu.abi=armeabi\n"));
    p.load(&props::parse_build_prop("ro.x=product\n"));
    p.apply_overlay();
    assert_eq!(p.get("ro.x"), Some("product"));
    assert_eq!(p.get("ro.product.cpu.abi"), Some("arm64-v8a"), "the overlay wins last");
}

#[test]
fn every_property_written_is_found_by_bionics_lookup() {
    let mut p = Properties::default();
    let mut names = Vec::new();
    for i in 0..500u32 {
        // Shared prefixes, varied component lengths, siblings on both sides.
        let name = format!("ro.t{}.{}x{}.v{}", i % 7, "q".repeat((i % 5) as usize), i, i * 31 % 97);
        p.set(&name, &format!("value-{i}"));
        names.push(name);
    }
    let area = p.area_bytes();
    assert_eq!(u32_at(&area, 8), 0x504f_5250, "magic");
    assert_eq!(u32_at(&area, 12), 0xfc6e_d0ab, "version");
    assert_eq!(area.len() % (128 << 10), 0, "a whole number of 128 KiB");
    assert!(u32_at(&area, 0) as usize <= area.len() - HEADER, "bytes_used fits");
    for (i, name) in names.iter().enumerate() {
        assert_eq!(find(&area, name).as_deref(), Some(format!("value-{i}").as_str()), "{name}");
    }
    assert_eq!(find(&area, "ro.t1.nope"), None);
}

#[test]
fn a_long_ro_value_uses_the_long_form_and_a_long_other_value_is_dropped() {
    let mut p = Properties::default();
    let long = "L".repeat(150);
    p.set("ro.x.long", &long);
    p.set("persist.long", &long);
    let dropped = p.drop_unrepresentable();
    assert_eq!(dropped, ["persist.long"]);
    assert_eq!(find(&p.area_bytes(), "ro.x.long").as_deref(), Some(long.as_str()));
}

#[test]
fn property_info_maps_every_name_to_the_one_context() {
    let info = props::property_info_bytes();
    assert_eq!(u32_at(&info, 0), 1, "current_version");
    assert_eq!(u32_at(&info, 4), 1, "minimum_supported_version");
    assert_eq!(u32_at(&info, 8) as usize, info.len(), "size");
    let contexts = u32_at(&info, 12) as usize;
    assert_eq!(u32_at(&info, contexts), 1, "one context");
    let name_at = u32_at(&info, contexts + 4) as usize;
    let end = name_at + info[name_at..].iter().position(|&b| b == 0).unwrap();
    assert_eq!(&info[name_at..end], props::CONTEXT.as_bytes());
    let root = u32_at(&info, 20) as usize;
    let entry = u32_at(&info, root) as usize;
    assert_eq!(u32_at(&info, entry + 8), 0, "the root's context index is 0");
    assert_eq!(u32_at(&info, root + 4), 0, "no children");
}

#[test]
fn the_serial_area_is_an_empty_prop_area() {
    let serial = props::serial_area_bytes();
    assert_eq!(u32_at(&serial, 8), 0x504f_5250);
    assert_eq!(serial.len(), 128 << 10);
}


#[test]
fn init_derives_ro_product_and_the_fingerprint_as_it_does_at_boot() {
    let mut p = Properties::default();
    p.load(&props::parse_build_prop(
        "ro.product.system.brand=SysBrand
ro.product.system.model=sysmodel
ro.product.product.model=ProdModel
         ro.product.product.name=pname
ro.product.system_ext.device=extdev
ro.product.system.manufacturer=M
         ro.build.version.release_or_codename=15
ro.build.id=AE3A
ro.build.version.incremental=123
         ro.build.type=user
ro.build.tags=release-keys
",
    ));
    p.derive_as_init();
    assert_eq!(p.get("ro.product.model"), Some("ProdModel"), "product wins over system");
    assert_eq!(p.get("ro.product.brand"), Some("SysBrand"), "system when nothing earlier sets it");
    assert_eq!(p.get("ro.product.device"), Some("extdev"));
    assert_eq!(p.get("ro.build.fingerprint"), Some("SysBrand/pname/extdev:15/AE3A/123:user/release-keys"));
}

/// A slow host is a slow device: Android scales its startup and input timeouts by
/// `ro.hw_timeout_multiplier` (`Build.HW_TIMEOUT_MULTIPLIER`), which the emulator images set. Four
/// cores (the Linux host, i5-4460) ANR'd Roblox's start twice at the stock 15 s ("failed to
/// complete startup"); a host with 12 or more is left at Android's own 1.
#[test]
fn a_host_with_few_cores_is_a_device_with_longer_timeouts() {
    use omni_linux::props::timeout_multiplier;
    assert_eq!(timeout_multiplier(4, None), Some(5));
    assert_eq!(timeout_multiplier(6, None), Some(5));
    assert_eq!(timeout_multiplier(8, None), Some(2));
    assert_eq!(timeout_multiplier(24, None), None, "the Windows host: unchanged");
    assert_eq!(timeout_multiplier(4, Some("3")), Some(3), "OMNI_HW_TIMEOUT_MULTIPLIER names one");
    assert_eq!(timeout_multiplier(4, Some("1")), None, "1 is Android's own");
}
