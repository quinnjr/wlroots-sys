//! Asserts that when a subsystem's `cfg` is set, its symbols were actually bound.
//!
//! `build.rs` decides which headers reach bindgen by scanning the wlroots include
//! tree and subtracting the gated ones. If that gating breaks — a header is
//! renamed upstream, an `extra_pc` probe stops resolving, `collect_headers`
//! regresses — the crate still compiles, `cargo clippy --all-targets` still
//! passes, and CI stays green. The bindings are simply missing symbols, and the
//! failure surfaces in a downstream compositor as `cannot find function
//! 'wlr_xwayland_create' in crate 'wlr_sys'`.
//!
//! Each item below names one entry point per subsystem, gated on that
//! subsystem's cfg. Nothing runs: the coercion is the assertion, so a header
//! that silently stopped being bound becomes a compile error here instead.
//!
//! Gated on the cfg alone, not `all(feature, cfg)` — `build.rs` only emits a cfg
//! when the feature is on *and* the library has it, so the cfg already implies
//! the feature.

#![allow(dead_code)]

#[cfg(wlr_has_x11_backend)]
use std::os::raw::c_char;
#[cfg(wlr_has_vulkan_renderer)]
use std::os::raw::c_int;

#[cfg(wlr_has_session)]
const SESSION: unsafe extern "C" fn(*mut wlr_sys::wl_event_loop) -> *mut wlr_sys::wlr_session =
    wlr_sys::wlr_session_create;

#[cfg(wlr_has_drm_backend)]
const DRM_BACKEND: unsafe extern "C" fn(
    *mut wlr_sys::wlr_session,
    *mut wlr_sys::wlr_device,
    *mut wlr_sys::wlr_backend,
) -> *mut wlr_sys::wlr_backend = wlr_sys::wlr_drm_backend_create;

#[cfg(wlr_has_x11_backend)]
const X11_BACKEND: unsafe extern "C" fn(
    *mut wlr_sys::wl_event_loop,
    *const c_char,
) -> *mut wlr_sys::wlr_backend = wlr_sys::wlr_x11_backend_create;

#[cfg(wlr_has_libinput_backend)]
const LIBINPUT_BACKEND: unsafe extern "C" fn(
    *mut wlr_sys::wlr_session,
) -> *mut wlr_sys::wlr_backend = wlr_sys::wlr_libinput_backend_create;

#[cfg(wlr_has_gles2_renderer)]
const GLES2_RENDERER: unsafe extern "C" fn(*mut wlr_sys::wlr_egl) -> *mut wlr_sys::wlr_renderer =
    wlr_sys::wlr_gles2_renderer_create;

#[cfg(wlr_has_vulkan_renderer)]
const VULKAN_RENDERER: unsafe extern "C" fn(c_int) -> *mut wlr_sys::wlr_renderer =
    wlr_sys::wlr_vk_renderer_create_with_drm_fd;

#[cfg(wlr_has_xwayland)]
const XWAYLAND: unsafe extern "C" fn(
    *mut wlr_sys::wl_display,
    *mut wlr_sys::wlr_compositor,
    bool,
) -> *mut wlr_sys::wlr_xwayland = wlr_sys::wlr_xwayland_create;

/// Colour management has no dedicated header — `wlr/render/color.h` is always
/// bound — so the cfg only reports whether ICC support was compiled in. Pin the
/// header's presence rather than a gated symbol.
const COLOR: unsafe extern "C" fn(*mut wlr_sys::wlr_color_transform) = {
    // Both `wlr_color_transform_unref` and the type come from the always-bound
    // `wlr/render/color.h`; if that header ever became gated this stops
    // compiling in the default configuration.
    wlr_sys::wlr_color_transform_unref
};
