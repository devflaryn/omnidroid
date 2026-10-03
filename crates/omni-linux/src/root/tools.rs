//! The magisk tool binary (`su`, `magisk`, `resetprop` are all this one file).
//! Our own arm64 build (device/src/root/magisk.c), committed and pinned in device/SHA256SUMS.

/// The bytes of the tool binary.
#[must_use]
pub fn magisk_binary() -> &'static [u8] {
    include_bytes!("../../device/root/magisk")
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    #[test]
    fn embedded_magisk_matches_sha256sums() {
        let bytes = super::magisk_binary();
        assert!(bytes.len() > 64, "a real arm64 ELF, not the placeholder");
        let got = format!("{:x}", Sha256::digest(bytes));
        let sums = include_str!("../../device/SHA256SUMS");
        assert!(
            sums.lines().any(|l| l.starts_with(&got) && l.contains("root/magisk")),
            "magisk sha {got} is not pinned in device/SHA256SUMS"
        );
    }
}
