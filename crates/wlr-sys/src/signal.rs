//! `wl_signal` helpers.
//!
//! wlroots publishes every event as a `struct wl_signal` embedded in its objects,
//! and you subscribe by linking a `struct wl_listener` into it. The functions that
//! do this are `static inline` in `wayland-server-core.h`, so no symbol exists to
//! link against and bindgen cannot generate them.
//!
//! `wayland-sys` already reimplements `wl_signal_init`, `wl_signal_add` and
//! `wl_signal_get` in Rust; they are re-exported here so callers need only one
//! import. `wl_signal_emit_mutable` is a real exported symbol in
//! libwayland-server 1.22+, but `wayland-sys` does not declare it, so it is
//! declared below.
//!
//! ```ignore
//! use std::ffi::c_void;
//! use wayland_sys::server::{wl_listener, wl_signal};
//!
//! unsafe extern "C" fn on_destroy(listener: *mut wl_listener, _data: *mut c_void) {
//!     let output: *mut MyOutput = unsafe { wlr_sys::container_of!(listener, MyOutput, destroy) };
//!     unsafe { drop(Box::from_raw(output)) };
//! }
//!
//! unsafe {
//!     (*output).destroy.notify = on_destroy;
//!     wlr_sys::wl_signal_add(&raw mut (*wlr_output).events.destroy, &raw mut (*output).destroy);
//! }
//! ```
//!
//! Unsubscribing is `wl_list_remove` on the listener's `link`, as in C.

use std::ffi::c_void;

use wayland_sys::server::wl_signal;

pub use wayland_sys::server::signal::{wl_signal_add, wl_signal_get, wl_signal_init};

unsafe extern "C" {
    /// Emit `signal`, tolerating listeners that remove themselves (or others)
    /// from the signal while it is being emitted.
    ///
    /// This is what wlroots itself uses, and what a compositor should use: the
    /// older `wl_signal_emit` corrupts its own iteration if a handler unlinks a
    /// listener, which destroy handlers routinely do.
    ///
    /// Requires libwayland-server 1.22 or newer; wlroots 0.20 already depends on
    /// 1.24, so the symbol is always present.
    ///
    /// # Safety
    ///
    /// `signal` must point at an initialised `wl_signal`, and every listener
    /// linked into it must be live with a valid `notify` callback.
    pub fn wl_signal_emit_mutable(signal: *mut wl_signal, data: *mut c_void);
}
