//! A bounds-checked little-endian view over a byte slice.
//!
//! Every read names what it was reading, so an out-of-bounds error in a 109 MB file says which
//! structure was truncated rather than only that something was.

use crate::error::{ElfError, Result, What};

/// A little-endian, bounds-checked view over `&[u8]`.
///
/// All accessors take an absolute offset into the slice the view was built from. There is no
/// cursor: ELF is a random-access format and threading a position through header parsing only
/// invites off-by-one bugs.
#[derive(Clone, Copy)]
pub struct View<'a> {
    data: &'a [u8],
}

impl<'a> View<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[inline]
    pub fn bytes(&self) -> &'a [u8] {
        self.data
    }

    #[inline]
    fn range(&self, what: What, offset: usize, len: usize) -> Result<&'a [u8]> {
        let end = offset.checked_add(len).ok_or(ElfError::OutOfBounds {
            what,
            offset,
            need: len,
            have: self.data.len(),
        })?;
        self.data
            .get(offset..end)
            .ok_or(ElfError::OutOfBounds {
                what,
                offset,
                need: len,
                have: self.data.len().saturating_sub(offset.min(self.data.len())),
            })
    }

    #[inline]
    pub fn slice(&self, what: What, offset: usize, len: usize) -> Result<&'a [u8]> {
        self.range(what, offset, len)
    }

    /// A sub-view, for handing a nested structure its own bounds.
    #[inline]
    pub fn subview(&self, what: What, offset: usize, len: usize) -> Result<View<'a>> {
        Ok(View::new(self.range(what, offset, len)?))
    }

    #[inline]
    pub fn u8(&self, what: What, offset: usize) -> Result<u8> {
        Ok(self.range(what, offset, 1)?[0])
    }

    #[inline]
    pub fn u16(&self, what: What, offset: usize) -> Result<u16> {
        let b = self.range(what, offset, 2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    #[inline]
    pub fn u32(&self, what: What, offset: usize) -> Result<u32> {
        let b = self.range(what, offset, 4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    #[inline]
    pub fn u64(&self, what: What, offset: usize) -> Result<u64> {
        let b = self.range(what, offset, 8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    #[inline]
    pub fn i64(&self, what: What, offset: usize) -> Result<i64> {
        Ok(self.u64(what, offset)? as i64)
    }

    /// Read a fixed 4-byte array, for magics.
    #[inline]
    pub fn array4(&self, what: What, offset: usize) -> Result<[u8; 4]> {
        let b = self.range(what, offset, 4)?;
        Ok([b[0], b[1], b[2], b[3]])
    }
}

/// Convert a `u64` file quantity to `usize` without silently truncating on a 32-bit host.
#[inline]
pub fn to_usize(what: What, value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| ElfError::OutOfBounds {
        what,
        offset: 0,
        need: usize::MAX,
        have: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian() {
        let v = View::new(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        assert_eq!(v.u16("t", 0).unwrap(), 0x0201);
        assert_eq!(v.u32("t", 0).unwrap(), 0x0403_0201);
        assert_eq!(v.u64("t", 0).unwrap(), 0x0807_0605_0403_0201);
    }

    #[test]
    fn refuses_out_of_bounds_without_panicking() {
        let v = View::new(&[0u8; 4]);
        let err = v.u64("phdr", 0).unwrap_err();
        assert!(matches!(
            err,
            ElfError::OutOfBounds {
                what: "phdr",
                offset: 0,
                need: 8,
                ..
            }
        ));
        assert!(v.u8("phdr", 4).is_err());
        assert!(v.u32("phdr", usize::MAX).is_err());
    }
}
