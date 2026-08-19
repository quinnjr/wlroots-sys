//! The GLES2 renderer's own surface.
//!
//! Gated on `wlr_has_gles2_renderer`, which `crates/wlr/build.rs` re-emits from
//! `wlr-sys`' `DEP_WLROOTS_HAVE_GLES2_RENDERER`. Without that build script the
//! cfg is never true and this module compiles to nothing, silently — see the
//! build script's own doc.
//!
//! The view-type pattern and the reason for it are in `pixman.rs`.
//!
//! # GL names are integers, not a GL crate
//!
//! `GLenum` and `GLuint` are both `unsigned int`. They cross this boundary as
//! `u32` with the relevant constants written into the docs, because this crate
//! wraps wlroots' use of GL rather than GL itself and a `gl` dependency here
//! would be a second, competing definition of the same names.

use std::ffi::CString;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::buffer::Buffer;
use crate::sys;

/// A [`Renderer`](crate::Renderer) that has been proved to be the GLES2
/// renderer.
///
/// Obtained from [`Renderer::as_gles2`](crate::Renderer::as_gles2). Borrows the
/// renderer, so it cannot outlive it.
#[derive(Clone, Copy)]
pub struct Gles2<'r> {
    raw: NonNull<sys::wlr_renderer>,
    _renderer: PhantomData<&'r ()>,
}

impl<'r> Gles2<'r> {
    /// # Safety
    ///
    /// `raw` must point at a live `wlr_renderer` that outlives `'r` and for
    /// which `wlr_renderer_is_gles2` returned true.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_renderer) -> Gles2<'r> {
        Gles2 {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_renderer"),
            _renderer: PhantomData,
        }
    }

    /// Whether the GL or EGL extension `ext` is available to this renderer.
    ///
    /// `false` for a name containing an interior NUL rather than a panic: no
    /// extension has one, so such a name cannot be present, and a query is not
    /// worth aborting a compositor over.
    pub fn check_ext(&self, ext: &str) -> bool {
        let Ok(ext) = CString::new(ext) else {
            return false;
        };
        // SAFETY: `'r` guarantees the renderer is live and the type test that
        // produced this view proved it is the GLES2 one; `ext` is a live,
        // NUL-terminated string for the call, which wlroots only reads.
        unsafe { sys::wlr_gles2_renderer_check_ext(self.raw.as_ptr(), ext.as_ptr()) }
    }

    /// The GL framebuffer object name this renderer renders into `buffer`
    /// through.
    ///
    /// Creates the renderer's cache entry for `buffer` if there is not one
    /// already, exactly as the pixman accessor does — `get_buffer_fbo` calls
    /// `gles2_buffer_get_or_create` (`render/gles2/renderer.c`), and it makes
    /// the renderer's EGL context current to do it. Not a way to ask whether
    /// the buffer has been rendered into.
    ///
    /// `None` when wlroots answered 0, which is not a valid FBO name: the
    /// context could not be made current, or the buffer could not be imported.
    /// The name is valid only while the renderer keeps that entry.
    pub fn buffer_fbo(&self, buffer: &Buffer<'_>) -> Option<u32> {
        // SAFETY: as above; the borrow keeps the buffer alive for the call.
        let fbo =
            unsafe { sys::wlr_gles2_renderer_get_buffer_fbo(self.raw.as_ptr(), buffer.as_ptr()) };
        if fbo == 0 { None } else { Some(fbo) }
    }

    /// The `wlr_egl` this renderer was built on.
    ///
    /// # Safety
    ///
    /// **Borrowed** — the renderer owns it and destroys it, so this must not be
    /// handed to another renderer constructor. Making its context current
    /// behind wlroots' back inside a render pass corrupts the pass: the GLES2
    /// renderer treats the current EGL context as global state and saves and
    /// restores it around each operation.
    pub unsafe fn egl(&self) -> *mut sys::wlr_egl {
        // SAFETY: as above; the call is a field read.
        unsafe { sys::wlr_gles2_renderer_get_egl(self.raw.as_ptr()) }
    }
}

impl std::fmt::Debug for Gles2<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gles2").field("raw", &self.raw).finish()
    }
}

/// How a GLES2 texture is bound: `wlr_gles2_texture_attribs`.
///
/// From [`Texture::gles2_attribs`](crate::Texture::gles2_attribs), for a
/// consumer doing their own GL drawing with wlroots' texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gles2TextureAttribs {
    /// The GL texture target — one of [`TARGET_2D`](Gles2TextureAttribs::TARGET_2D)
    /// or [`TARGET_EXTERNAL_OES`](Gles2TextureAttribs::TARGET_EXTERNAL_OES).
    pub target: u32,
    /// The GL texture name.
    pub tex: u32,
    /// Whether the texture's format has an alpha channel.
    pub has_alpha: bool,
}

impl Gles2TextureAttribs {
    /// `GL_TEXTURE_2D`, from `<GLES2/gl2.h>`.
    ///
    /// Written out rather than taken from a GL crate: see this module's doc.
    pub const TARGET_2D: u32 = 0x0DE1;

    /// `GL_TEXTURE_EXTERNAL_OES`, from `<GLES2/gl2ext.h>` — what an imported
    /// DMA-BUF is usually bound as, and what needs `samplerExternalOES` in a
    /// shader rather than `sampler2D`.
    pub const TARGET_EXTERNAL_OES: u32 = 0x8D65;
}
