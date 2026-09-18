//! Shared test fixtures.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A file with verifiable contents, removed when it goes out of scope.
///
/// Every byte is `(offset / page_size) as u8`, so a test can check that a mapped range came from the
/// file offset it asked for rather than merely that it read *something*. That distinction is the
/// whole point when verifying that a partially unmapped view was re-mapped at the right offsets.
pub struct TempFile {
    dir: PathBuf,
    path: PathBuf,
    page: usize,
}

impl TempFile {
    pub fn new(name: &str, len: usize, page: usize) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("omni-mem-tests-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the test directory");
        let path = dir.join(name);
        let mut bytes = vec![0u8; len];
        for (offset, byte) in bytes.iter_mut().enumerate() {
            *byte = ((offset / page) & 0xff) as u8;
        }
        std::fs::write(&path, &bytes).expect("write the test file");
        Self { dir, path, page }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The byte this file holds at `offset`.
    pub fn byte_at(&self, offset: u64) -> u8 {
        ((offset as usize / self.page) & 0xff) as u8
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub const KIB: usize = 1024;
pub const MIB: usize = 1024 * 1024;
