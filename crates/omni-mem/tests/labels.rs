//! Mapping labels (`omni_mem::label_scope`, `GuestSpace::labelled_regions`): a mapping carries the
//! label in force on the thread that made it, for its whole life -- through an `mprotect` split and
//! a partial `munmap` -- and a mapping made outside any scope carries none.

mod common;

use common::{TempFile, KIB, MIB};
use omni_mem::{
    label_scope, Backing, CommitPolicy, GuestSpace, GuestSpaceConfig, MapExecutability, MapLabel,
    Placement, Protection, RegionKind,
};

fn space() -> GuestSpace {
    GuestSpace::with_config(GuestSpaceConfig { size: 256 * MIB, ..GuestSpaceConfig::default() })
        .expect("reserve a guest address space")
}

fn labels_at(space: &GuestSpace, address: usize) -> Vec<MapLabel> {
    space
        .labelled_regions()
        .into_iter()
        .filter(|(region, _)| region.mapping_start == address)
        .map(|(_, label)| label)
        .collect()
}

#[test]
fn a_mapping_keeps_the_label_it_was_made_under_through_splits_and_partial_unmaps() {
    let space = space();
    let anywhere = Placement::Anywhere { align: space.page_size() };
    let heap = {
        let _label = label_scope(MapLabel::at("engine mmap", 0x2345));
        space.map_anonymous(anywhere, 4 * MIB, Protection::ReadWrite, CommitPolicy::Lazy).unwrap()
    };
    let plain = space.map_anonymous(anywhere, MIB, Protection::ReadWrite, CommitPolicy::Lazy).unwrap();
    let stacks = {
        let _outer = label_scope(MapLabel::new("outer"));
        let _inner = label_scope(MapLabel::new("guest thread stacks"));
        space.map_anonymous(anywhere, MIB, Protection::ReadWrite, CommitPolicy::Eager).unwrap()
    };

    assert_eq!(labels_at(&space, heap), [MapLabel::at("engine mmap", 0x2345)]);
    assert_eq!(labels_at(&space, plain), [MapLabel::default()], "no scope, no label");
    assert!(labels_at(&space, plain)[0].is_unlabelled());
    assert_eq!(labels_at(&space, stacks), [MapLabel::new("guest thread stacks")], "the innermost scope");

    // An mprotect in the middle splits the mapping into three regions; each keeps the label.
    let page = space.page_size();
    space.protect(heap + MIB, 16 * page, Protection::Read).unwrap();
    assert_eq!(labels_at(&space, heap), [MapLabel::at("engine mmap", 0x2345); 3]);
    // A partial unmap re-maps the survivors; they keep it too.
    space.unmap(heap + 2 * MIB, 64 * KIB).unwrap();
    let after = labels_at(&space, heap);
    assert!(after.len() >= 3, "{after:?}");
    assert!(after.iter().all(|label| *label == MapLabel::at("engine mmap", 0x2345)), "{after:?}");

    // The committed bytes a report sums are the region map's own.
    let committed: usize = space
        .labelled_regions()
        .iter()
        .filter(|(_, label)| label.owner == "guest thread stacks")
        .map(|(region, _)| region.committed)
        .sum();
    assert_eq!(committed, MIB, "an eager mapping is committed in full");

    // Free space is not listed at all.
    assert!(space.labelled_regions().iter().all(|(region, _)| region.kind != RegionKind::Free));
}

#[test]
fn a_file_view_carries_its_label() {
    let space = space();
    let page = space.page_size();
    let file = TempFile::new("labels-file-view", 4 * page, page);
    let backing = Backing::open(file.path(), MapExecutability::NonExecutable).unwrap();
    let view = {
        let _label = label_scope(MapLabel::new("libroblox.so"));
        space
            .map_file(&backing, 0, Placement::Anywhere { align: page }, 4 * page, Protection::Read)
            .unwrap()
    };
    let regions = space.labelled_regions();
    let (region, label) = regions.iter().find(|(r, _)| r.start == view).expect("the view");
    assert!(matches!(region.kind, RegionKind::File { .. }), "{region:?}");
    assert_eq!(*label, MapLabel::new("libroblox.so"));
    assert_eq!(region.committed, 0, "a read-only view costs no commit");
}
