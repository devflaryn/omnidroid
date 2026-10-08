//! **A zeroed buffer the kernel reads into, without writing its zeros first.**
//!
//! `read`, `pread`, `recvfrom`, `recvmsg` and friends hand the host a buffer of the size the guest
//! asked for, zeroed, and copy back what the host wrote. `vec![0u8; n]` from the C heap is a
//! `HeapAlloc(HEAP_ZERO_MEMORY)`, which -- MEASURED on this host (`scratchpad/ram/heapzero.ps1`,
//! 2026-10-09) -- writes every page of a block the heap carves from a segment (64 KiB and 256 KiB:
//! all pages resident) and none of a block it maps for itself (1 MiB and up: none). So a guest
//! thread that waits in `recvmsg(fd, buf, 64 KiB)` -- Android's netlink listeners, a socket
//! daemon's reader -- holds 64 KiB of resident zeros in the host heap for as long as it waits,
//! and every such call first spends the time to write them.
//!
//! From [`MAPPED_FROM`] up, the buffer is fresh pages of its own instead (reserved and committed
//! through `omni_platform::vm`), which the OS gives zeroed and which become resident only where
//! the host writes; it is given back when dropped. Below it, the heap as before. The bytes are
//! the same either way. `OMNI_ZERO_BUF=0`: the heap for every size (the old behaviour, for an A/B).
use std::sync::OnceLock;

/// The size from which a buffer is pages of its own.
pub const MAPPED_FROM: usize = 64 << 10;

fn mapped_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_ZERO_BUF").as_deref() != Ok("0"))
}

/// `len` zero bytes.
pub enum ZeroBuf {
    Heap(Vec<u8>),
    Mapped { reservation: Option<omni_platform::vm::Reservation>, len: usize },
}

impl ZeroBuf {
    /// `len` zero bytes, as pages of their own from [`MAPPED_FROM`] up.
    #[must_use]
    pub fn new(len: usize) -> Self {
        if len >= MAPPED_FROM && mapped_on() {
            if let Some(b) = Self::mapped(len) {
                return b;
            }
        }
        Self::Heap(vec![0u8; len])
    }

    fn mapped(len: usize) -> Option<Self> {
        use omni_platform::vm;
        let size = len.next_multiple_of(vm::page_size());
        let reservation = vm::reserve(size, vm::page_size()).ok()?;
        // SAFETY: the whole of a fresh reservation of ours, nothing else refers to it.
        if unsafe { vm::commit(reservation.as_ptr(), size, vm::Protection::ReadWrite) }.is_err() {
            let _ = vm::release(reservation);
            return None;
        }
        Some(Self::Mapped { reservation: Some(reservation), len })
    }
}

impl std::ops::Deref for ZeroBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Heap(v) => v,
            // SAFETY: committed read-write for `len` bytes (and zero-filled by the OS at commit),
            // owned by this value until it drops.
            Self::Mapped { reservation: Some(r), len } => unsafe { std::slice::from_raw_parts(r.as_ptr(), *len) },
            Self::Mapped { reservation: None, .. } => &[],
        }
    }
}

impl std::ops::DerefMut for ZeroBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Heap(v) => v,
            // SAFETY: as `deref`, and `&mut self` makes this the only reference.
            Self::Mapped { reservation: Some(r), len } => unsafe { std::slice::from_raw_parts_mut(r.as_ptr(), *len) },
            Self::Mapped { reservation: None, .. } => &mut [],
        }
    }
}

impl Drop for ZeroBuf {
    fn drop(&mut self) {
        if let Self::Mapped { reservation, .. } = self {
            if let Some(r) = reservation.take() {
                let _ = omni_platform::vm::release(r);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_size_reads_zero_and_takes_writes() {
        for len in [0, 1, 4095, MAPPED_FROM - 1, MAPPED_FROM, MAPPED_FROM + 1, 1 << 20, (1 << 20) + 7] {
            let mut b = ZeroBuf::new(len);
            assert_eq!(b.len(), len);
            assert!(b.iter().all(|&x| x == 0), "{len}");
            if len > 1 {
                b[len - 1] = 9;
                b[0] = 7;
                assert_eq!((b[0], b[len - 1]), (7, 9));
            }
            let mapped = matches!(b, ZeroBuf::Mapped { .. });
            assert_eq!(mapped, len >= MAPPED_FROM, "{len}");
        }
    }
}
