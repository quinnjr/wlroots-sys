//! wlroots' EGL wrapper, for compositors that initialise EGL themselves.
//!
//! This crate wraps wlroots' *use* of EGL; it does not wrap EGL. `EGLDisplay`
//! and `EGLContext` are `void *` typedefs from `<EGL/egl.h>`, and they cross
//! this boundary as `*mut c_void` from `unsafe` functions rather than being
//! re-typed — a consumer doing custom GL work already has an EGL crate, and
//! this crate taking a second one would put two incompatible definitions of the
//! same handle in one process.
//!
//! # There is no `wlr_egl_destroy`
//!
//! wlroots exposes exactly one way to release a `wlr_egl`: hand it to
//! `wlr_gles2_renderer_create`, which takes ownership and destroys it with the
//! renderer. So [`Egl`] has **no `Drop`** — there is nothing it could call —
//! and an [`Egl`] that is never passed to
//! [`Renderer::gles2_from_egl`](crate::Renderer::gles2_from_egl) leaks. That is
//! wlroots' API, stated rather than papered over.

use std::ffi::c_void;
use std::ptr::NonNull;

use crate::error::{Error, Result};
use crate::sys;

/// A `wlr_egl` built around a caller-supplied EGL display and context.
///
/// Consumed by [`Renderer::gles2_from_egl`](crate::Renderer::gles2_from_egl),
/// which is its only release path. Useless without the `gles2-renderer`
/// feature, and inert rather than an error there: the type still compiles,
/// there is simply no renderer constructor to hand it to.
pub struct Egl {
    raw: NonNull<sys::wlr_egl>,
}

impl Egl {
    /// Wrap an existing `EGLDisplay` and `EGLContext`.
    ///
    /// # Safety
    ///
    /// `display` must be a live `EGLDisplay` and `context` a live `EGLContext`
    /// on it, both of which must outlive the returned value and the renderer it
    /// is eventually given to. wlroots stores them and calls through them; it
    /// does not validate them and cannot.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if wlroots could not query the display's extensions or
    /// the allocation failed.
    pub unsafe fn create_with_context(display: *mut c_void, context: *mut c_void) -> Result<Egl> {
        // SAFETY: forwarded verbatim to this function's own contract — the two
        // handles are the caller's to vouch for.
        let raw = unsafe { sys::wlr_egl_create_with_context(display, context) };
        Ok(Egl {
            raw: NonNull::new(raw).ok_or(Error::Create("wlr_egl_create_with_context"))?,
        })
    }

    /// The `EGLDisplay` this was built around.
    ///
    /// # Safety
    ///
    /// The returned handle is borrowed from whoever created it — the same one
    /// passed to [`create_with_context`](Egl::create_with_context) — and using
    /// it is subject to EGL's own rules, which this crate knows nothing about.
    /// It must not be terminated while wlroots still holds it.
    pub unsafe fn display(&self) -> *mut c_void {
        // SAFETY: this value owns a live `wlr_egl`; the call is a field read.
        unsafe { sys::wlr_egl_get_display(self.raw.as_ptr()) }
    }

    /// The `EGLContext` this was built around.
    ///
    /// # Safety
    ///
    /// As [`display`](Egl::display): borrowed, and subject to EGL's rules
    /// rather than this crate's. In particular the GLES2 renderer treats the
    /// current context as global state and saves and restores it around every
    /// operation, so making this current behind wlroots' back inside a render
    /// pass corrupts the pass.
    pub unsafe fn context(&self) -> *mut c_void {
        // SAFETY: as above.
        unsafe { sys::wlr_egl_get_context(self.raw.as_ptr()) }
    }

    /// The raw `wlr_egl`, for interop with code that speaks `wlr-sys` directly.
    ///
    /// Borrowed — this value still holds it, and the only release path remains
    /// [`Renderer::gles2_from_egl`](crate::Renderer::gles2_from_egl).
    pub fn as_ptr(&self) -> *mut sys::wlr_egl {
        self.raw.as_ptr()
    }

    /// Give up ownership, for the one caller that hands it to wlroots.
    ///
    /// Not a `Drop`-dodging trick: this type has no `Drop`, because wlroots
    /// offers nothing for one to call.
    #[cfg_attr(not(wlr_has_gles2_renderer), allow(dead_code))]
    pub(crate) fn into_raw(self) -> *mut sys::wlr_egl {
        self.raw.as_ptr()
    }
}

impl std::fmt::Debug for Egl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `wlr_egl` is opaque, so the pointer is the only honest field.
        f.debug_struct("Egl").field("raw", &self.raw).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `Egl` names EGL state that belongs to one thread's current context,
    /// and wlroots' GLES2 renderer treats that context as global.
    #[test]
    fn an_egl_does_not_cross_a_thread() {
        use static_assertions::assert_not_impl_any;

        assert_not_impl_any!(Egl: Send, Sync);
    }
}
