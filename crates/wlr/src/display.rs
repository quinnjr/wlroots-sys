//! The Wayland display and its event loop.
//!
//! `Display` is one of the few things this crate genuinely owns, so it is one of
//! the few places RAII applies: `Drop` destroys it. Everything reachable *from*
//! a display is owned by wlroots and must not be dropped by us.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::{Error, Result, sys};

/// An owned Wayland display.
pub struct Display {
    raw: NonNull<sys::wl_display>,
}

impl Display {
    /// Create a display.
    pub fn new() -> Result<Self> {
        use sys::wayland_sys::ffi_dispatch;
        // `ffi_dispatch!`'s non-`dlopen` expansion calls the wrapped function as a
        // bare name, so this glob import is load-bearing there; its `dlopen`
        // expansion instead goes through a function-pointer table on the handle
        // and never references the name, so the same import is unused there.
        // `allow` (not `expect`) because the non-`dlopen` build is the default —
        // the one `cargo test`, `cargo clippy`, and CI's primary lint gate all
        // exercise — and the import genuinely is used there. `expect` would
        // break that everyday build, and it would fail as a plain, unrelated-
        // looking lint diagnostic with no pointer back to this comment. Do not
        // "upgrade" this to `expect`.
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: no arguments, no preconditions. `ffi_dispatch!` so this links
        // whether or not wayland-sys was built with its `dlopen` feature.
        let raw = unsafe {
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_display_create,
            )
        };
        let raw = NonNull::new(raw).ok_or(Error::Create("wl_display_create"))?;
        Ok(Display { raw })
    }

    /// The display's event loop.
    pub fn event_loop(&self) -> EventLoop<'_> {
        use sys::wayland_sys::ffi_dispatch;
        // `allow`, not `expect`: unused only under the `dlopen` expansion of
        // `ffi_dispatch!`, which calls through a function-pointer table instead
        // of the bare name this import brings into scope. See the identical
        // comment on `Display::new` for the full explanation.
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: `self` is live, so its display is.
        let raw = unsafe {
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_display_get_event_loop,
                self.raw.as_ptr()
            )
        };
        EventLoop {
            // A live display always has an event loop; libwayland creates it in
            // `wl_display_create` and never tears it down separately. A null
            // here would mean libwayland broke its own contract, which is a
            // bug worth panicking loudly on rather than threading through a
            // `Result` that every caller would have to unwrap anyway.
            raw: NonNull::new(raw).expect("display has no event loop"),
            _display: PhantomData,
        }
    }

    // Unused outside this module's tests until the dispatch-time constructors
    // that call this for real are wired up; `expect` (rather than `allow`)
    // makes the compiler flag this attribute itself as unnecessary the
    // moment those callers land.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn as_ptr(&self) -> *mut sys::wl_display {
        self.raw.as_ptr()
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        use sys::wayland_sys::ffi_dispatch;
        // `allow`, not `expect`: unused only under the `dlopen` expansion of
        // `ffi_dispatch!`, which calls through a function-pointer table instead
        // of the bare name this import brings into scope. See the identical
        // comment on `Display::new` for the full explanation.
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: we own this display and destroy it exactly once, here, as
        // the display goes out of scope.
        unsafe {
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_display_destroy,
                self.raw.as_ptr()
            )
        };
    }
}

/// The display's event loop, borrowed from the [`Display`].
pub struct EventLoop<'d> {
    raw: NonNull<sys::wl_event_loop>,
    _display: PhantomData<&'d Display>,
}

impl<'d> EventLoop<'d> {
    pub(crate) fn as_ptr(&self) -> *mut sys::wl_event_loop {
        self.raw.as_ptr()
    }

    /// Dispatch pending events. `timeout_ms` of 0 returns immediately.
    pub fn dispatch(&self, timeout_ms: i32) -> Result<()> {
        use sys::wayland_sys::ffi_dispatch;
        // `allow`, not `expect`: unused only under the `dlopen` expansion of
        // `ffi_dispatch!`, which calls through a function-pointer table instead
        // of the bare name this import brings into scope. See the identical
        // comment on `Display::new` for the full explanation.
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: the borrow guarantees the display, and so the loop, is live.
        let rc = unsafe {
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_event_loop_dispatch,
                self.raw.as_ptr(),
                timeout_ms
            )
        };
        if rc >= 0 {
            Ok(())
        } else {
            Err(Error::Operation("wl_event_loop_dispatch"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises `as_ptr` on both `Display` and `EventLoop`, which otherwise
    /// have no caller until the dispatch-time code in later tasks lands.
    #[test]
    fn as_ptr_is_non_null_for_display_and_event_loop() {
        let display = Display::new().expect("wl_display_create failed");
        assert!(!display.as_ptr().is_null());

        let loop_ = display.event_loop();
        assert!(!loop_.as_ptr().is_null());
    }
}
