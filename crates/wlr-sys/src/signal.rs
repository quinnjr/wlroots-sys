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
//! re-exported below.
//!
//! # No `wl_signal_emit_mutable` on this branch
//!
//! The 0.19+ branches declare `wl_signal_emit_mutable`, which tolerates a
//! handler unlinking *another* listener. It cannot be used here: it was added in
//! libwayland **1.22**, and wlroots 0.15 only requires 1.20 — Ubuntu 22.04 ships
//! exactly 1.20.0, where the symbol does not exist and the declaration fails to
//! link.
//!
//! What is re-exported instead is `wayland-sys`'s `wl_signal_emit`, a Rust
//! reimplementation over `list_for_each_safe`. It survives a handler unlinking
//! *itself* — the common destroy-handler case — but not a handler unlinking the
//! listener *after* it in the list. wlroots 0.15 was itself built against 1.20
//! and so operates under the same constraint; this is a property of the era, not
//! a shortcut taken here.
//!
//! [`container_of!`](crate::container_of) documents the callback pattern that
//! recovers the owning struct from the `*mut wl_listener` a handler is passed.
//! Unsubscribing is `wl_list_remove` on the listener's `link`, as in C.

pub use wayland_sys::server::signal::{
    wl_signal_add, wl_signal_emit, wl_signal_get, wl_signal_init,
};
