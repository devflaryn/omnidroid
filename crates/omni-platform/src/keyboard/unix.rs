//! The keyboard seam on Linux and other unix: no backend (see the module header). The guest keeps
//! Android's `Generic.kcm`.
use super::{HostLayout, KeyboardError, KeyboardResult};

pub(super) fn current_layout() -> KeyboardResult<HostLayout> {
    Err(KeyboardError::Unsupported("Linux: the XKB layout is not read yet"))
}
