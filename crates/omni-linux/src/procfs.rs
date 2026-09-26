//! `/proc` and `/sys`, generated from the process itself (milestone A2).
//!
//! `vfs` reaches this through the [`ProcFs`] trait object a process attaches to its `Vfs` once it
//! exists, so `vfs` does not depend on `process`. A generated file's bytes are produced when it is
//! opened and read from that snapshot, as Linux's seq files are.
use crate::vfs::{DirEnt, Node};

/// The generated part of the file tree.
pub trait ProcFs: Send + Sync {
    /// The node at a normalized absolute path under `/proc` or `/sys`; `None` if there is none.
    fn node(&self, path: &[u8]) -> Option<Node>;
    /// The entries of a generated directory.
    fn list(&self, path: &[u8]) -> Vec<DirEnt>;
    /// The bytes of a generated file.
    fn read(&self, path: &[u8]) -> Option<Vec<u8>>;
}
