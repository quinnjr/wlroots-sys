//! End-to-end smoke test: stand up a wlroots headless backend, run one turn of
//! the event loop, and tear it down.
//!
//! This exercises the parts of the crate that compile-time layout assertions
//! cannot: that the symbols actually resolve at link time, that `wl_display`
//! from `wayland-sys` really is the type wlroots expects, and that the wlroots
//! event-loop plumbing works against a real libwayland. It needs no GPU, no
//! seat, and no running compositor.
//!
//! ```sh
//! cargo run -p wlr-sys --example headless
//! ```

use wayland_sys::ffi_dispatch;
// Glob-imported because `ffi_dispatch!` calls these as bare names when
// `wayland-sys` is linked directly rather than dlopen'd.
use wayland_sys::server::*;

fn main() {
    // SAFETY: single-threaded setup and teardown of objects we exclusively own.
    // Every pointer is checked before use, and each is destroyed exactly once.
    unsafe {
        wlr_sys::wlr_log_init(wlr_sys::wlr_log_importance::WLR_DEBUG, None);

        // The first argument is only evaluated when `wayland-sys` is built with
        // its `dlopen` feature; otherwise the macro drops it.
        let display = ffi_dispatch!(
            wayland_sys::server::wayland_server_handle(),
            wl_display_create,
        );
        assert!(!display.is_null(), "wl_display_create failed");

        let event_loop = ffi_dispatch!(
            wayland_sys::server::wayland_server_handle(),
            wl_display_get_event_loop,
            display
        );
        assert!(!event_loop.is_null(), "wl_display_get_event_loop failed");

        // The headless backend renders nowhere and drives itself, which is what
        // makes it usable in CI.
        let backend = wlr_sys::wlr_headless_backend_create(event_loop);
        assert!(!backend.is_null(), "wlr_headless_backend_create failed");

        assert!(
            wlr_sys::wlr_backend_start(backend),
            "wlr_backend_start failed"
        );
        println!("headless backend started");

        // A zero timeout returns immediately whether or not anything was ready;
        // we only care that dispatching does not fault.
        let dispatched = ffi_dispatch!(
            wayland_sys::server::wayland_server_handle(),
            wl_event_loop_dispatch,
            event_loop,
            0
        );
        assert!(dispatched >= 0, "wl_event_loop_dispatch failed");
        println!("dispatched one event-loop iteration");

        wlr_sys::wlr_backend_destroy(backend);
        ffi_dispatch!(
            wayland_sys::server::wayland_server_handle(),
            wl_display_destroy,
            display
        );
        println!("torn down cleanly");
    }
}
