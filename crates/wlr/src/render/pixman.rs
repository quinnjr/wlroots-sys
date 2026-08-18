//! The pixman renderer's own surface.
//!
//! # Views carry the proof of the type test
//!
//! Every backend-specific wlroots accessor has the same precondition —
//! `wlr_pixman_texture_get_image` is undefined behaviour on a texture that is
//! not a pixman texture, and the header says so and nothing enforces it. A
//! `Renderer::pixman_image()` returning `Option` would still have to be called
//! on the right renderer.
//!
//! So the type test produces a *value*: [`Renderer::as_pixman`](crate::Renderer::as_pixman)
//! answers `Some(Pixman<'_>)` only when `wlr_renderer_is_pixman` is true, and
//! every pixman-only accessor hangs off that view. The precondition becomes
//! unrepresentable rather than documented. `gles2.rs` and `vulkan.rs` follow
//! the same shape.
//!
//! # `pixman_image_t` is a foreign type
//!
//! `wlr-sys` binds `pixman_image_t` — it is reachable through wlroots' own
//! structs — but binds none of pixman's functions, and this crate does not wrap
//! pixman. The image pointers here are escape hatches for a consumer who
//! already links pixman, handed out from `unsafe` functions and **borrowed**:
//! they belong to the renderer's per-buffer cache or to the texture, and
//! `pixman_image_unref`ing one is a double free.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::buffer::Buffer;
use crate::sys;

/// A [`Renderer`](crate::Renderer) that has been proved to be the pixman
/// renderer.
///
/// Obtained from [`Renderer::as_pixman`](crate::Renderer::as_pixman). Borrows the
/// renderer, so it cannot outlive it.
#[derive(Clone, Copy)]
pub struct Pixman<'r> {
    raw: NonNull<sys::wlr_renderer>,
    _renderer: PhantomData<&'r ()>,
}

impl<'r> Pixman<'r> {
    /// # Safety
    ///
    /// `raw` must point at a live `wlr_renderer` that outlives `'r` and for
    /// which `wlr_renderer_is_pixman` returned true.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_renderer) -> Pixman<'r> {
        Pixman {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_renderer"),
            _renderer: PhantomData,
        }
    }

    /// The `pixman_image_t` this renderer draws into `buffer` through.
    ///
    /// Creates the renderer's cache entry for `buffer` if there is not one
    /// already — the name says "get", but `wlr_pixman_renderer_get_buffer_image`
    /// calls `create_buffer` on a miss (`render/pixman/renderer.c`). So this is
    /// not a way to ask whether the buffer has been rendered into.
    ///
    /// `None` only when the entry could not be made, which is what a buffer the
    /// pixman renderer cannot map — a DMA-BUF, say — answers.
    ///
    /// # Safety
    ///
    /// The image is **borrowed**: it belongs to the renderer's per-buffer
    /// cache, stays valid only while that entry does, and must never be
    /// `pixman_image_unref`ed. Using it at all requires pixman, which this
    /// crate does not wrap.
    pub unsafe fn buffer_image(&self, buffer: &Buffer<'_>) -> Option<*mut sys::pixman_image_t> {
        // SAFETY: `'r` guarantees the renderer is live and the type test that
        // produced this view proved it is the pixman one; the borrow keeps the
        // buffer alive for the call.
        let image = unsafe {
            sys::wlr_pixman_renderer_get_buffer_image(self.raw.as_ptr(), buffer.as_ptr())
        };
        if image.is_null() {
            return None;
        }
        Some(image)
    }
}

impl std::fmt::Debug for Pixman<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pixman").field("raw", &self.raw).finish()
    }
}
