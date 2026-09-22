//! Renderer abstraction and the guest-facing `libvulkan.so`/EGL/GLES surfaces (D8).
//!
//! # What is here now
//!
//! [`vulkan`] — a real Vulkan renderer over `omni_platform::window`: instance, physical-device
//! selection, logical device, a `VK_KHR_win32_surface` swapchain with correct recreation on
//! resize, and two ways to put a frame on the screen ([`vulkan::Renderer::present_clear`] and
//! [`vulkan::Renderer::present_rgba8`]). It is the **host side** of D8 — the device and the
//! present loop the guest-facing forwarding layer will be built on top of, not that layer itself.
//!
//! [`select`] — every choice the renderer makes, as pure functions over what the driver reported,
//! so that a machine with no GPU can still test them and a machine with one GPU can be tested
//! against tables from GPUs nobody here has.
//!
//! [`host`] — [`GfxVulkanHost`], this crate as `omni_android::vulkan::VulkanHost`. The **guest**
//! side of D8 begins there: `libroblox.so` opens `libvulkan.so` itself and reaches every entry
//! point through `vkGetInstanceProcAddr`, so `omni-android` owns the loader, the thunks, the
//! census and the extension-name rewrite log, and this crate supplies the driver behind them. The
//! trait lives up there and the implementation lives here for the reason `Cargo.toml` records:
//! `ash` and `libloading` must stay out of the adapter's dependency graph.
//!
//! [`claim`] — who owns a window's swapchain. Two Vulkan stacks point at one window in an
//! Omnidroid process — [`vulkan::Renderer`] and the guest's, through [`host::GfxVulkanHost`] —
//! and a native window may have at most one swapchain. This host has no validation layer to say
//! so, so the rule is a named refusal above the driver rather than an undefined behaviour below
//! it.
//!
//! [`image`] — the RGBA8 frame type. Its format is not a preference: D27 scoped texture
//! transcoding to `GL_ETC1_RGB8_OES` decoded to RGBA8, because this host's GPU samples **neither
//! ETC2 nor ASTC** (`docs/research/graphics-spike.md` §3, both families measured), so RGBA8 is
//! what arrives.
//!
//! # Two host facts this crate is written around, both measured
//!
//! **There are no validation layers on this machine.**
//! `vkEnumerateInstanceLayerProperties` reports five layers and `VK_LAYER_KHRONOS_validation` is
//! not among them (spike §6). [`vulkan::Renderer`] enables it when it is present and never
//! requires it — but every lifetime and every layout transition in that file is written as though
//! nothing will ever report a mistake, because here nothing will. The spike's own swapchain
//! use-after-free produced **zero** diagnostic output and crashed the NVIDIA driver instead.
//!
//! **Vulkan is loaded at run time, not linked.** `ash` with `default-features = false` and
//! `loaded` means no Vulkan SDK is needed to build the workspace, and a host with no Vulkan at all
//! gets [`error::GfxError::LoaderMissing`] rather than a link error. `Cargo.toml` records why that
//! is worth the one Global Constraint 4 exception it costs.
//!
//! # What is deliberately absent
//!
//! No shaders, no `VkPipeline`, no SPIR-V, and therefore no shader compiler in the build. The
//! present path is `vkCmdClearColorImage` and `vkCmdBlitImage`. D8 records that Roblox ships
//! **1,364 SPIR-V modules** of its own; the shaders this project runs will be the guest's, and a
//! triangle of our own would only have added a second source of them. See [`vulkan`].
//!
//! No EGL or GLES surface yet. D8 requires those symbols to *resolve*, because `libroblox.so`
//! hard-links 91 EGL+GL imports through `DT_NEEDED` and will not load otherwise — but resolving
//! them is a link-time requirement of `omni-android`'s import table, not a rendering requirement
//! of this crate.

#![warn(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod claim;
pub mod error;
pub mod host;
pub mod image;
pub mod select;
pub mod vulkan;

pub use crate::claim::{claim_window, WindowClaim, WindowClaimed, WindowKey};
pub use crate::error::{GfxError, GfxResult, VkError};
pub use crate::host::{GfxVulkanHost, PresentedImage};
pub use crate::image::Rgba8Image;
pub use crate::select::PresentMode;
pub use crate::vulkan::{FrameOutcome, Renderer, RendererConfig};

/// The texture transcoder. See [`omni_texture`] for the census that scoped it to one format.
///
/// A separate crate rather than a module, and D19's argument is why: `omni-gfx` transitively links
/// Vulkan and the windowing system, and pure computation that must not be able to reach the OS
/// belongs where `cargo tree -p omni-texture -e normal` can prove it cannot. The renderer will
/// call it from `glCompressedTexImage2D`; the guest hands over `GL_ETC1_RGB8_OES` and gets RGBA8
/// back — which is exactly what [`Rgba8Image`] accepts.
pub use omni_texture;
