//! Asserts that the types crossing into other libraries really are the
//! ecosystem crates' types, not local look-alikes.
//!
//! bindgen only emits what the `wlr_.*` allowlist transitively reaches, so a
//! name missing from a blocklist in `build.rs` does not fail anything — bindgen
//! quietly generates its own definition, `use wayland_sys::server::*` is a glob,
//! and the local one silently wins. The result compiles, runs, and hands
//! downstream code a type that is *not* the one the README promises.
//!
//! That is exactly how `libinput_tablet_tool` escaped: for the lifetime of the
//! bug, `wlr_libinput_get_tablet_tool_handle` returned a locally generated type
//! rather than `input_sys`'s.
//!
//! # What these checks can and cannot prove
//!
//! Most items below coerce a real wlroots function to a signature written in the
//! ecosystem crates' types. Coercion is exact — no subtyping on raw pointers —
//! so a **nominal** type (a `struct` or `enum`) either matches or fails to
//! compile. `const` items are type-checked whether or not anything uses them, as
//! are the bodies of the never-called `fn`s, so nothing here is vacuous.
//!
//! The exception is **type aliases**. `xkb_keysym_t`, `xkb_mod_mask_t`,
//! `xkb_led_index_t` and friends are all `= u32`, and aliases are transparent —
//! a locally generated duplicate *is* the same type, so no check written against
//! them can fail. They are therefore not asserted here. That is sound rather
//! than a gap: a duplicated `u32` alias is ABI-identical, so unlike a duplicated
//! struct it carries no risk.

#![allow(dead_code)]

use std::os::raw::c_char;

// --- wayland-sys -----------------------------------------------------------

// 0.15's `wlr_compositor_create` predates the `version` parameter added later.
const WL_DISPLAY: unsafe extern "C" fn(
    *mut wayland_sys::server::wl_display,
    *mut wlr_sys::wlr_renderer,
) -> *mut wlr_sys::wlr_compositor = wlr_sys::wlr_compositor_create;

const WL_CLIENT: unsafe extern "C" fn(
    *mut wlr_sys::wlr_seat,
    *mut wayland_sys::server::wl_client,
) -> *mut wlr_sys::wlr_seat_client = wlr_sys::wlr_seat_client_for_wl_client;

const WL_RESOURCE: unsafe extern "C" fn(
    *mut wayland_sys::server::wl_resource,
) -> *mut wlr_sys::wlr_surface = wlr_sys::wlr_surface_from_resource;

// No `wl_event_loop` coercion: wlroots 0.15 constructors take a `wl_display`,
// and nothing in its public API accepts a `wl_event_loop`. The type stays
// blocklisted so it cannot be locally regenerated; there is simply no signature
// to pin it against until 0.19.

const SEAT_NAME: unsafe extern "C" fn(
    *mut wayland_sys::server::wl_display,
    *const c_char,
) -> *mut wlr_sys::wlr_seat = wlr_sys::wlr_seat_create;

/// Types embedded by value rather than passed across a signature. A layout
/// mismatch would be caught by bindgen's own `const _` assertions; what this
/// pins is *identity*, which those cannot check.
fn embedded_wayland_types(
    output: *mut wlr_sys::wlr_output,
    source: *mut wlr_sys::wlr_data_source,
    listener: *mut wlr_sys::wl_listener,
) {
    let _: *mut wayland_sys::server::wl_signal = unsafe { &raw mut (*output).events.destroy };
    let _: *mut wayland_sys::common::wl_list = unsafe { &raw mut (*output).modes };
    let _: *mut wayland_sys::common::wl_array = unsafe { &raw mut (*source).mime_types };
    // The crate-root re-export must be the same type too — this is the one
    // downstream consumers name when declaring a callback struct.
    let _: *mut wayland_sys::server::wl_listener = listener;
}

// --- xkbcommon-sys ---------------------------------------------------------

const XKB_KEYMAP: unsafe extern "C" fn(
    *mut wlr_sys::wlr_keyboard,
    *mut xkbcommon_sys::xkb_keymap,
) -> bool = wlr_sys::wlr_keyboard_set_keymap;

fn embedded_xkb_types(kb: *mut wlr_sys::wlr_keyboard) {
    // `xkb_state` is a real struct, so this is a nominal check. The sibling
    // `xkb_mod_mask_t` field is a `= u32` alias and is deliberately not asserted
    // (see the module docs).
    let _: *mut xkbcommon_sys::xkb_state = unsafe { (*kb).xkb_state };
}

// --- input-sys -------------------------------------------------------------

#[cfg(wlr_has_libinput_backend)]
const LIBINPUT_DEVICE: unsafe extern "C" fn(
    *mut wlr_sys::wlr_input_device,
) -> *mut input_sys::libinput_device = wlr_sys::wlr_libinput_get_device_handle;

// `wlr_libinput_get_tablet_tool_handle` does not exist in wlroots 0.15; it was
// added in 0.20. `libinput_tablet_tool` stays blocklisted (harmlessly: nothing
// in 0.15's headers reaches it), but there is no signature to pin it against.

// --- deliberately local ----------------------------------------------------

/// pixman is generated locally because it is embedded *by value* in 17 wlroots
/// struct fields — its layout is load-bearing, and bindgen's own `const _`
/// assertions check every offset.
const _: () = assert!(
    size_of::<wlr_sys::pixman_region32_t>() > 0,
    "pixman_region32_t must be a complete type, not an opaque handle"
);

/// `drmModeModeInfo` is generated locally for the opposite reason: wlroots only
/// forward-declares it (`typedef struct _drmModeModeInfo drmModeModeInfo;` in
/// `backend/drm.h`) and passes it by pointer, so no layout is at stake.
///
/// If this ever stops being zero-sized, wlroots started exposing the layout and
/// the decision to generate it locally rather than use a libdrm crate needs
/// revisiting.
#[cfg(wlr_has_drm_backend)]
const _: () = assert!(
    size_of::<wlr_sys::drmModeModeInfo>() == 0,
    "drmModeModeInfo is expected to stay an opaque forward declaration"
);

/// libc types are *also* generated locally — `timespec`, `dev_t` and friends
/// come from bindgen, not from the `libc` crate, so `wlr_sys::timespec` is not
/// `libc::timespec`. The crate deliberately does not depend on `libc`; this
/// records that so nobody re-adds the dependency expecting identity.
const _: () = assert!(size_of::<wlr_sys::timespec>() > 0);
