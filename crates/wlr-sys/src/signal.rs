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
//! `wayland-sys` also exposes `wl_signal_emit`. It is deliberately not
//! re-exported. Note the reason is narrower than the C one: `wayland-sys`'s
//! version is a Rust reimplementation built on `list_for_each_safe`, so unlike
//! C's macro it already survives a handler unlinking *itself*. What it does not
//! survive is a handler unlinking *another* listener — particularly the next one
//! — which wlroots destroy handlers do. Only `wl_signal_emit_mutable`'s
//! cursor-and-sentinel scheme handles that.
//!
//! [`container_of!`](crate::container_of) documents the callback pattern that
//! recovers the owning struct from the `*mut wl_listener` a handler is passed.
//! Unsubscribing is `wl_list_remove` on the listener's `link`, as in C.

use std::ffi::c_void;

use wayland_sys::server::wl_signal;

pub use wayland_sys::server::signal::{wl_signal_add, wl_signal_get, wl_signal_init};

unsafe extern "C" {
    /// Emit `signal`, tolerating listeners that remove themselves (or others)
    /// from the signal while it is being emitted.
    ///
    /// This is what wlroots itself uses, and what a compositor should use: C's
    /// `wl_signal_emit` corrupts its own iteration if a handler unlinks a
    /// listener, which destroy handlers routinely do.
    ///
    /// Requires libwayland-server 1.22 or newer. wlroots 0.20 already requires
    /// 1.24, and `build.rs` puts `-lwayland-server` on the link line in its own
    /// right, so this declaration resolves regardless of how `wayland-sys` was
    /// configured.
    ///
    /// # Safety
    ///
    /// * `signal` must point at an initialised `wl_signal`, and every listener
    ///   linked into it must be live with a valid `notify` callback.
    /// * `data` is passed verbatim to every listener, each of which will cast it
    ///   to the type that signal is documented to carry and dereference it. So
    ///   `data` must be valid for the whole emission *and* be of that type —
    ///   passing null, or the wrong type, is a null or type-confused
    ///   dereference inside the handler, not an error returned to you.
    /// * `signal` and the object owning it must outlive the call: a listener may
    ///   unlink itself or others, but must not free the object holding `signal`
    ///   and then return.
    /// * `notify` callbacks must not unwind across the FFI boundary.
    pub fn wl_signal_emit_mutable(signal: *mut wl_signal, data: *mut c_void);
}
