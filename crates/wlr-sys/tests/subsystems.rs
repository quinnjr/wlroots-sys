//! Asserts that when a subsystem's `cfg` is set, its symbols were actually bound.
//!
//! `build.rs` decides which headers reach bindgen by scanning the wlroots include
//! tree and subtracting the gated ones. If that gating breaks, the crate still
//! compiles, clippy still passes, and CI stays green — the bindings are simply
//! missing symbols, and the failure surfaces in a downstream compositor.
//!
//! Each item names one entry point per subsystem, gated on that subsystem's cfg.
//! Nothing runs: the coercion is the assertion.
//!
//! wlroots 0.15 has five gated subsystems, not seven. There is no `session` cfg
//! (the session backend was not optional yet, so its header is unconditional),
//! no `vulkan-renderer` (added 0.16), and no colour management (added 0.19).

#![allow(dead_code)]

#[cfg(wlr_has_x11_backend)]
use std::os::raw::c_char;

#[cfg(wlr_has_drm_backend)]
const DRM_BACKEND: unsafe extern "C" fn(
    *mut wlr_sys::wl_display,
    *mut wlr_sys::wlr_session,
    *mut wlr_sys::wlr_device,
    *mut wlr_sys::wlr_backend,
) -> *mut wlr_sys::wlr_backend = wlr_sys::wlr_drm_backend_create;

#[cfg(wlr_has_x11_backend)]
const X11_BACKEND: unsafe extern "C" fn(
    *mut wlr_sys::wl_display,
    *const c_char,
) -> *mut wlr_sys::wlr_backend = wlr_sys::wlr_x11_backend_create;

#[cfg(wlr_has_libinput_backend)]
const LIBINPUT_BACKEND: unsafe extern "C" fn(
    *mut wlr_sys::wl_display,
    *mut wlr_sys::wlr_session,
) -> *mut wlr_sys::wlr_backend = wlr_sys::wlr_libinput_backend_create;

#[cfg(wlr_has_gles2_renderer)]
const GLES2_RENDERER: unsafe extern "C" fn(::std::os::raw::c_int) -> *mut wlr_sys::wlr_renderer =
    wlr_sys::wlr_gles2_renderer_create_with_drm_fd;

#[cfg(wlr_has_xwayland)]
const XWAYLAND: unsafe extern "C" fn(
    *mut wlr_sys::wl_display,
    *mut wlr_sys::wlr_compositor,
    bool,
) -> *mut wlr_sys::wlr_xwayland = wlr_sys::wlr_xwayland_create;

/// `wlr/backend/session.h` is bound unconditionally on 0.15 — there is no
/// `have_session` flag to gate it. This pins that it really is bound.
const SESSION: unsafe extern "C" fn(*mut wlr_sys::wl_display) -> *mut wlr_sys::wlr_session =
    wlr_sys::wlr_session_create;
