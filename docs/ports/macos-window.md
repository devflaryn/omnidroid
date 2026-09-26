# macOS: window, Vulkan surface, audio

Code: `omni-platform/src/window/macos.rs` and `macos/{main_thread,appkit,keys}.rs`,
`omni-platform/src/audio/macos.rs`, `omni-gfx/src/portability.rs`; the module docs carry the
details. Measured on the M1 with MoltenVK 1.4.2 and the Homebrew Vulkan loader 1.4.357; n = 1
unless stated.

## Window

* **AppKit needs the main thread; libtest never runs a test there.** A constructor
  (`main_thread.rs`) moves the real `main` to a pthread and keeps the main thread for AppKit; a
  `Window` is a proxy whose calls are `dispatch_sync_f` to it. Where it cannot (inside an `.app`
  bundle, `LC_MAIN` and `dlsym` disagreeing, ...) `Window::new` answers `MainThreadUnavailable`.
* **Keys**: `keycode` = the `kVK_*` code, `scancode` = the set-1 code of the same physical key; 18
  keys (Fn, F13-F20, volume, keypad `=`, JIS) have none.
* **Wheel**: `deltaY x 120`, signs made physical (natural scrolling undone). A physical wheel under
  natural scrolling is **not measured**.
* **Pointer capture**: the deltas are **accelerated**; AppKit has no unaccelerated figure, and an
  `IOHIDManager` reader would be needed.
* `dpi = 96 x backingScaleFactor`; odd pixel sizes on a 2x display round up (641x481 -> 642x482).

## Vulkan surface and portability

* The view's layer is a `CAMetalLayer` whose `contentsScale` follows the backing scale. A minimised
  window's surface keeps its size, so the window's own `Resized {0,0}` decides.
* **Loader**: `dlopen("libvulkan.1.dylib")` does not search `/opt/homebrew/lib`, so
  `vulkan_loader_candidates()` lists Homebrew's and `/usr/local`'s loader, then MoltenVK itself.
* **Portability**: without `VK_KHR_portability_enumeration` the loader answers
  `VK_ERROR_INCOMPATIBLE_DRIVER`; the instance is retried with it only after that answer.
  `VK_KHR_portability_subset` is enabled with exactly the features the device reports.
* **MoltenVK 1.4.2's gaps on the M1** (`portability_live.rs`): `pointPolygons`,
  `samplerMipLodBias`, `tessellationIsolines`, `tessellationPointMode`; the other 11 subset
  features are supported.
* Enabling the subset and chaining its features have **no detector** here (MoltenVK accepts a
  device without them; no validation layer is installed), so they have no mutation row: kept by
  reading the code.

## Audio

`AudioUnit` `DefaultOutput`; the render callback pulls interleaved f32 from a lock-free ring and
signals a dispatch semaphore once per period. MEASURED by
`the_default_device_opens_and_reports_a_usable_shape`: 48,000 Hz, 2 channels, packed float,
nothing converted; period 512 frames. Open: `format()` is fixed at open, so a default-device rate
change mid-stream would leave it stale.
