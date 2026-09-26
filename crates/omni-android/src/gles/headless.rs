//! **Headless mode's GLES half**: every GL call passes [`dispatch`](super::dispatch), so the drop
//! is decided there, at the call.
//!
//! While headless is on and no screenshot is waiting for a real frame, these are **not forwarded**
//! when the bound draw framebuffer is per-frame ([`crate::headless::history`]; framebuffer 0, the
//! window's, always is): the draws (`glDrawArrays*`, `glDrawElements*`, `glDrawRangeElements*`,
//! `glMultiDraw*`), the clears (`glClear`, `glClearBuffer*`) and `glBlitFramebuffer`. Each of them
//! returns `void`, so the engine reads nothing different. Everything else is forwarded: binds,
//! uploads, shader compiles, queries, fences and `eglSwapBuffers` -- which still swaps, and still
//! paces the engine at the rate the host swaps at.
//!
//! **The draw framebuffer is tracked per thread**, from `glBindFramebuffer(GL_FRAMEBUFFER or
//! GL_DRAW_FRAMEBUFFER, name)`: a context is current on one thread, and the engine renders on one.
//! A binding is what marks a framebuffer as rendered in the frame.
//!
//! **The screenshot** is read in `eglSwapBuffers`, before the host's swap: framebuffer 0 bound for
//! reading, `glReadPixels` of the surface's size as RGBA8, every binding put back as it was.

use std::cell::Cell;

use super::{Call, Gles};
use crate::boundary::ImportCall;
use crate::error::AbiResult;

/// `GL_FRAMEBUFFER`.
const GL_FRAMEBUFFER: u32 = 0x8D40;
/// `GL_READ_FRAMEBUFFER`.
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
/// `GL_DRAW_FRAMEBUFFER`.
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
/// `GL_READ_FRAMEBUFFER_BINDING`.
const GL_READ_FRAMEBUFFER_BINDING: u32 = 0x8CAA;
/// `GL_PIXEL_PACK_BUFFER` and its binding.
const GL_PIXEL_PACK_BUFFER: u32 = 0x88EB;
const GL_PIXEL_PACK_BUFFER_BINDING: u32 = 0x88ED;
/// `GL_PACK_ALIGNMENT`.
const GL_PACK_ALIGNMENT: u32 = 0x0D05;
/// `GL_RGBA`, `GL_UNSIGNED_BYTE`.
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
/// `EGL_WIDTH`, `EGL_HEIGHT`.
const EGL_WIDTH: u32 = 0x3057;
const EGL_HEIGHT: u32 = 0x3056;

/// What a GL command is to headless mode, decided once per slot from its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    /// Forwarded, whatever headless says.
    Other,
    /// A draw: dropped into a per-frame target.
    Draw,
    /// A clear or a blit: dropped into a per-frame target, not counted as a draw.
    Fill,
    /// `glBindFramebuffer`: tracked, forwarded.
    Bind,
    /// `glDeleteFramebuffers`: their history is forgotten, forwarded.
    Delete,
}

/// The role of the command `name`.
pub(super) fn role_for(name: &str) -> Role {
    let base = name.trim_end_matches(|c: char| c.is_ascii_uppercase());
    if ["glDrawArrays", "glDrawElements", "glDrawRangeElements", "glMultiDrawArrays", "glMultiDrawElements"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        Role::Draw
    } else if matches!(
        base,
        "glClear" | "glClearBufferfv" | "glClearBufferiv" | "glClearBufferuiv" | "glClearBufferfi" | "glBlitFramebuffer"
    ) {
        Role::Fill
    } else if base == "glBindFramebuffer" {
        Role::Bind
    } else if base == "glDeleteFramebuffers" {
        Role::Delete
    } else {
        Role::Other
    }
}

thread_local! {
    /// The draw framebuffer bound on this thread's current context.
    static DRAW_FRAMEBUFFER: Cell<u32> = const { Cell::new(0) };
}

impl Gles {
    /// Headless mode at the call: whether `call` (of `role`) is dropped rather than forwarded.
    pub(super) fn headless_drops(&self, c: &mut ImportCall<'_, '_>, call: &Call, role: Role) -> bool {
        match role {
            Role::Other => false,
            Role::Bind => {
                let target = call.lanes[0] as u32;
                if target == GL_FRAMEBUFFER || target == GL_DRAW_FRAMEBUFFER {
                    let name = call.lanes[1] as u32;
                    DRAW_FRAMEBUFFER.with(|bound| bound.set(name));
                    self.headless.rendered(u64::from(name));
                }
                false
            }
            Role::Delete => {
                let count = (call.lanes[0] as u32 as usize).min(4096);
                if let Ok(at) = usize::try_from(call.lanes[1]) {
                    if count > 0 && at != 0 {
                        if let Ok(bytes) = c.mem().read_bytes(at, count * 4, c.blame(1)) {
                            for name in bytes.chunks_exact(4) {
                                let name = u32::from_le_bytes(name.try_into().expect("four"));
                                if name != 0 {
                                    self.headless.forget_target(u64::from(name));
                                }
                            }
                        }
                    }
                }
                false
            }
            Role::Draw | Role::Fill => {
                if !self.headless.is_on() {
                    return false;
                }
                let target = DRAW_FRAMEBUFFER.with(Cell::get);
                let dropped = self.headless.drops_now() && self.headless.per_frame(u64::from(target));
                if role == Role::Draw {
                    self.headless.count_draw(dropped);
                }
                dropped
            }
        }
    }

    /// Headless mode's screenshot, at the guest's `eglSwapBuffers(display, surface)` and before the
    /// host's: when one is due, read the surface's framebuffer back and save it.
    pub(super) fn screenshot_before_swap(&self, call: &Call) {
        let Some(shot) = self.headless.take_due_screenshot() else { return };
        match self.read_back_window(call) {
            Ok((width, height, mut rgba)) => {
                crate::headless::png::flip_rows(width, &mut rgba);
                crate::headless::save_png(shot.path, width, height, rgba);
            }
            Err(why) => eprintln!("SCREENSHOT: failed {}: {why}", shot.path.display()),
        }
    }

    fn read_back_window(&self, call: &Call) -> Result<(u32, u32, Vec<u8>), String> {
        let (display, surface) = (call.lanes[0], call.lanes[1]);
        let query = |attribute: u32| -> Result<u32, String> {
            let mut value: i32 = 0;
            let ok = self
                .host_call(call, "eglQuerySurface", &[display, surface, u64::from(attribute), &mut value as *mut i32 as u64])
                .map_err(|e| e.to_string())?;
            if ok as u32 == 0 || value <= 0 {
                return Err(format!("eglQuerySurface({attribute:#x}) answered {ok} / {value}"));
            }
            Ok(value as u32)
        };
        let (width, height) = (query(EGL_WIDTH)?, query(EGL_HEIGHT)?);
        let get = |pname: u32| -> AbiResult<u64> {
            let mut value: i32 = 0;
            self.host_call(call, "glGetIntegerv", &[u64::from(pname), &mut value as *mut i32 as u64])?;
            Ok(value as u32 as u64)
        };
        let fail = |e: crate::error::AbiError| e.to_string();
        let read_fb = get(GL_READ_FRAMEBUFFER_BINDING).map_err(fail)?;
        let pack_buffer = get(GL_PIXEL_PACK_BUFFER_BINDING).map_err(fail)?;
        let alignment = get(GL_PACK_ALIGNMENT).map_err(fail)?;
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        let bind = |target: u32, name: u64| self.host_call(call, "glBindFramebuffer", &[u64::from(target), name]);
        let result = (|| -> AbiResult<()> {
            bind(GL_READ_FRAMEBUFFER, 0)?;
            self.host_call(call, "glBindBuffer", &[u64::from(GL_PIXEL_PACK_BUFFER), 0])?;
            self.host_call(call, "glPixelStorei", &[u64::from(GL_PACK_ALIGNMENT), 4])?;
            self.host_call(
                call,
                "glReadPixels",
                &[
                    0,
                    0,
                    u64::from(width),
                    u64::from(height),
                    u64::from(GL_RGBA),
                    u64::from(GL_UNSIGNED_BYTE),
                    rgba.as_mut_ptr() as u64,
                ],
            )?;
            Ok(())
        })();
        // Put back what the engine had bound, whatever happened.
        let _ = bind(GL_READ_FRAMEBUFFER, read_fb);
        let _ = self.host_call(call, "glBindBuffer", &[u64::from(GL_PIXEL_PACK_BUFFER), pack_buffer]);
        let _ = self.host_call(call, "glPixelStorei", &[u64::from(GL_PACK_ALIGNMENT), alignment]);
        result.map_err(fail)?;
        Ok((width, height, rgba))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_draws_clears_and_blits_are_named_and_nothing_else() {
        for name in [
            "glDrawArrays",
            "glDrawElements",
            "glDrawArraysInstanced",
            "glDrawElementsInstancedEXT",
            "glDrawRangeElements",
            "glDrawElementsBaseVertexOES",
            "glDrawArraysIndirect",
            "glMultiDrawArraysEXT",
        ] {
            assert_eq!(role_for(name), Role::Draw, "{name}");
        }
        for name in ["glClear", "glClearBufferfv", "glClearBufferfi", "glBlitFramebuffer", "glBlitFramebufferANGLE"] {
            assert_eq!(role_for(name), Role::Fill, "{name}");
        }
        assert_eq!(role_for("glBindFramebuffer"), Role::Bind);
        assert_eq!(role_for("glBindFramebufferOES"), Role::Bind);
        assert_eq!(role_for("glDeleteFramebuffers"), Role::Delete);
        for name in ["glDrawBuffers", "glClearColor", "glClearDepthf", "glClearStencil", "glBindRenderbuffer", "glReadPixels"] {
            assert_eq!(role_for(name), Role::Other, "{name}");
        }
    }
}
