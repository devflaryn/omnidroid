//! Hardware HALs served from the host, on the binder broker: what a device's vendor processes
//! serve, for hardware this runtime provides from the host (sub-project D). Each registers with the
//! real `servicemanager` and is declared by omnidroid's device overlay ([`crate::device`]).
pub mod gralloc;
pub mod parcel;
