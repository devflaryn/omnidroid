//! Renderer abstraction and the guest-facing `libvulkan.so`/EGL/GLES surfaces (D8).
//!
//! Still almost empty: created by M0 task 1 so later tasks have somewhere to land, and graphics
//! work itself starts at M6.
//!
//! The one thing here is the re-export of [`omni_texture`], the runtime transcoder for the
//! compressed texture formats the guest uses and the host GPU cannot sample. It is a separate
//! crate rather than a module: the host supports neither ETC2 nor ASTC
//! (`docs/research/graphics-spike.md` §3), the APK ships 38 ETC1 textures
//! (`tools/texture_census.py`), and decoding them is pure computation that must not be able to
//! reach the OS -- which `cargo tree -p omni-texture -e normal` being one line makes checkable
//! rather than conventional, exactly as D19 argued for `omni-bionic`. The renderer will call it
//! from `glCompressedTexImage2D`; the guest hands over `GL_ETC1_RGB8_OES` and gets RGBA8 back.

/// The texture transcoder. See [`omni_texture`] for the census that scoped it to one format.
pub use omni_texture;
