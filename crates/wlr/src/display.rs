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
    /// Re-borrow a loop pointer this crate already holds.
    ///
    /// # Safety
    ///
    /// `raw` must name a live `wl_event_loop` belonging to a display that
    /// outlives `'d`. [`Backend`](crate::Backend) is the only caller: it keeps
    /// the pointer it was created from, and its own `'d` is that display's.
    pub(crate) unsafe fn from_raw(raw: NonNull<sys::wl_event_loop>) -> EventLoop<'d> {
        EventLoop {
            raw,
            _display: PhantomData,
        }
    }

    pub(crate) fn as_ptr(&self) -> *mut sys::wl_event_loop {
        self.raw.as_ptr()
    }

    /// The same pointer, still carrying its non-nullness, for the one caller
    /// that stores it rather than passing it straight to C.
    pub(crate) fn as_non_null(&self) -> NonNull<sys::wl_event_loop> {
        self.raw
    }

    /// Dispatch pending events. `timeout_ms` of 0 returns immediately.
    ///
    /// # Errors
    ///
    /// [`Error::Reentrant`] if called from inside a handler, and that refusal
    /// is load-bearing rather than defensive. A handler is passed borrowed
    /// handles — [`Output`](crate::Output) and whatever later slices add —
    /// whose lifetime stops them escaping the call but says nothing about what
    /// may happen *during* it. Driving the loop here lets wlroots destroy and
    /// free the very object a handle in the enclosing frame still names, and
    /// nothing in that sequence needs `unsafe`: this method is safe, takes
    /// `&self`, and a handler's own state may hold a `&Display` to re-derive
    /// the loop from. Refusing is what makes a live handle's validity an
    /// invariant rather than an expectation. Queue the work in your own state
    /// and do it once the handler has returned.
    ///
    /// [`Error::Operation`] if libwayland reports the dispatch failed.
    pub fn dispatch(&self, timeout_ms: i32) -> Result<()> {
        if crate::dispatch::in_handler() {
            return Err(Error::Reentrant("EventLoop::dispatch"));
        }

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

    /// A display always has an event loop, and `event_loop` hands back a
    /// usable pointer to it rather than the display's own.
    ///
    /// Compares the two pointers directly rather than just asserting
    /// non-nullness: `EventLoop::raw` is a `NonNull`, so a non-nullness check
    /// alone is statically incapable of failing and would test nothing about
    /// the "rather than the display's own" half of the claim above.
    #[test]
    fn a_display_yields_a_non_null_event_loop() {
        let display = Display::new().expect("wl_display_create failed");
        let loop_ = display.event_loop();
        assert_ne!(
            loop_.as_ptr().cast::<std::ffi::c_void>(),
            display.raw.as_ptr().cast::<std::ffi::c_void>(),
            "event_loop() must return the loop's own pointer, not the display's"
        );
    }

    /// Dispatching outside any handler must work — this is the path
    /// `Backend::run` takes on every turn of its loop, and the refusal added
    /// for handlers must not catch it.
    #[test]
    fn dispatching_outside_a_handler_is_allowed() {
        let display = Display::new().expect("wl_display_create failed");
        assert_eq!(
            display.event_loop().dispatch(0),
            Ok(()),
            "no handler is running, so there is nothing to refuse"
        );
    }
}
