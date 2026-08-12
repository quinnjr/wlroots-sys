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

    /// Create a Wayland socket with the first free `wayland-N` name and
    /// return that name.
    ///
    /// The name is what a client's `WAYLAND_DISPLAY` must be set to. libwayland
    /// owns the returned string for the display's life, so it is copied out
    /// rather than borrowed — a `&str` tied to `&self` would be a lifetime
    /// this crate cannot actually prove.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if libwayland could not bind any socket name,
    /// which in practice means `XDG_RUNTIME_DIR` is unset or full.
    pub fn add_socket_auto(&self) -> Result<String> {
        use sys::wayland_sys::ffi_dispatch;
        // `allow`, not `expect`: unused only under the `dlopen` expansion of
        // `ffi_dispatch!`. See the identical comment on `Display::new`.
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: `self` is live, so its display is.
        let name = unsafe {
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_display_add_socket_auto,
                self.raw.as_ptr()
            )
        };
        if name.is_null() {
            return Err(Error::Operation("wl_display_add_socket_auto"));
        }
        // SAFETY: libwayland returned a non-null, NUL-terminated C string that
        // it owns for the display's life; this reads it and copies it out
        // before returning, and never frees it.
        let name = unsafe { std::ffi::CStr::from_ptr(name) };
        Ok(name.to_string_lossy().into_owned())
    }

    /// Push queued protocol messages out to every client.
    ///
    /// Necessary on every turn of a hand-driven loop, and easy to forget:
    /// libwayland's own `wl_display_run` does this for you, but this crate
    /// drives `wl_event_loop_dispatch` directly, so nothing else will.
    /// [`Backend::run_all`](crate::Backend::run_all) calls it once per turn,
    /// which is why a consumer using that entry point never has to.
    ///
    /// Infallible: libwayland reports per-client flush failures by killing the
    /// offending client, and returns nothing to the caller.
    pub fn flush_clients(&self) {
        use sys::wayland_sys::ffi_dispatch;
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: `self` is live, so its display is.
        unsafe {
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_display_flush_clients,
                self.raw.as_ptr()
            )
        };
    }

    /// The raw display, for the one in-crate caller that has to compare it
    /// against a backend's event loop.
    pub(crate) fn as_ptr(&self) -> *mut sys::wl_display {
        self.raw.as_ptr()
    }

    /// The display's event loop.
    ///
    /// # Panics
    ///
    /// Panics if libwayland reports that this display has no event loop.
    /// Unreachable in normal use: `wl_display_create` creates the loop and
    /// libwayland never tears it down separately, so a null here would mean
    /// libwayland broke its own contract. Threading it through a [`Result`]
    /// every caller would have to unwrap would be the worse trade.
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

    /// The socket name is the thing a client's `WAYLAND_DISPLAY` is set to,
    /// so it has to come back as an owned, non-empty `wayland-N`.
    #[test]
    fn a_display_can_bind_an_automatic_socket() {
        let display = Display::new().expect("wl_display_create failed");
        match display.add_socket_auto() {
            Ok(name) => assert!(
                name.starts_with("wayland-"),
                "libwayland names automatic sockets wayland-N, got {name:?}"
            ),
            // CI containers without an XDG_RUNTIME_DIR cannot bind at all,
            // and that is a property of the machine rather than of this code.
            Err(e) => assert_eq!(e, Error::Operation("wl_display_add_socket_auto")),
        }
    }

    /// Flushing with no clients connected must be a harmless no-op rather
    /// than a fault — `run_all` calls it on every single turn.
    #[test]
    fn flushing_a_clientless_display_is_harmless() {
        let display = Display::new().expect("wl_display_create failed");
        display.flush_clients();
        display.flush_clients();
    }
}
