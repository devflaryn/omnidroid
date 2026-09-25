//! **Whose a guest mapping is**: a label a mapping carries for a memory report, and nothing else.
//!
//! A guest address space holds the engine's heap, its thread stacks, the loaded library, the
//! graphics layer's shadows and this runtime's own structures side by side, and the region map
//! knows each mapping's size and commit but not who asked for it. A caller that knows -- the
//! `mmap` handler knows the guest's call site, the loader knows the library -- says so around the
//! call that maps:
//!
//! ```ignore
//! let _label = omni_mem::label_scope(MapLabel::new("GLES buffer shadows"));
//! space.map_anonymous(...)?;
//! ```
//!
//! and every mapping made on that thread inside the scope carries the label for its life, through
//! `mprotect` splits and partial `munmap`s ([`GuestSpace::labelled_regions`] reports it).
//!
//! **A scope rather than a parameter**, because thirty call sites across four crates map guest
//! memory and only the report reads the answer: a parameter would change every signature for a
//! diagnostic, and a mapping nobody labelled is reported as exactly that. **Thread-local**, because
//! the question "who is mapping" is asked of the thread that maps. Nothing reads a label to decide
//! anything; it is never part of what [`GuestSpace::region_at`] answers.
//!
//! [`GuestSpace::labelled_regions`]: crate::GuestSpace::labelled_regions
//! [`GuestSpace::region_at`]: crate::GuestSpace::region_at

use std::cell::Cell;

/// Who a mapping is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct MapLabel {
    /// The owner, as a report groups it: `"engine mmap"`, `"guest thread stacks"`. Empty for a
    /// mapping made outside any scope.
    pub owner: &'static str,
    /// A finer key inside the owner, 0 when there is none: for a guest `mmap`, the guest's call
    /// site (its `X30`), so a report can say which code in the engine holds the memory.
    pub site: u64,
}

impl MapLabel {
    /// A label with no site.
    #[must_use]
    pub const fn new(owner: &'static str) -> Self {
        Self { owner, site: 0 }
    }

    /// A label with a site.
    #[must_use]
    pub const fn at(owner: &'static str, site: u64) -> Self {
        Self { owner, site }
    }

    /// Whether this is the label of a mapping made outside any scope.
    #[must_use]
    pub fn is_unlabelled(&self) -> bool {
        self.owner.is_empty()
    }
}

thread_local! {
    static CURRENT: Cell<MapLabel> = const { Cell::new(MapLabel { owner: "", site: 0 }) };
}

/// The label mappings made on this thread now carry.
pub(crate) fn current() -> MapLabel {
    CURRENT.with(Cell::get)
}

/// Label every mapping this thread makes until the returned guard is dropped; the label in force
/// before is restored then, so scopes nest.
#[must_use = "the label applies only while the guard is alive"]
pub fn label_scope(label: MapLabel) -> LabelScope {
    let previous = CURRENT.with(|current| current.replace(label));
    LabelScope { previous, _not_send: core::marker::PhantomData }
}

/// See [`label_scope`]. Not `Send`: it restores *this* thread's label.
#[derive(Debug)]
pub struct LabelScope {
    previous: MapLabel,
    _not_send: core::marker::PhantomData<*const ()>,
}

impl Drop for LabelScope {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_nest_and_restore() {
        assert!(current().is_unlabelled());
        {
            let _outer = label_scope(MapLabel::new("outer"));
            assert_eq!(current(), MapLabel::new("outer"));
            {
                let _inner = label_scope(MapLabel::at("inner", 7));
                assert_eq!(current(), MapLabel::at("inner", 7));
            }
            assert_eq!(current(), MapLabel::new("outer"));
        }
        assert!(current().is_unlabelled());
        // Another thread never sees this one's label.
        let _here = label_scope(MapLabel::new("here"));
        assert!(std::thread::spawn(|| current().is_unlabelled()).join().unwrap());
    }
}
