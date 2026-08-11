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
        // SAFETY: `self` is live, so its display is, and the returned handle
        // borrows `self` — so the display outlives the loop handle.
        unsafe { EventLoop::from_raw(self.raw) }
    }
}

/// The event loop libwayland created alongside `display`.
///
/// # Safety
///
/// `display` must name a live `wl_display`.
unsafe fn loop_of(display: NonNull<sys::wl_display>) -> NonNull<sys::wl_event_loop> {
    use sys::wayland_sys::ffi_dispatch;
    // `allow`, not `expect`: unused only under the `dlopen` expansion of
    // `ffi_dispatch!`, which calls through a function-pointer table instead
    // of the bare name this import brings into scope. See the identical
    // comment on `Display::new` for the full explanation.
    #[allow(unused_imports)]
    use sys::wayland_sys::server::*;

    // SAFETY: the caller guarantees the display is live, and this only reads a
    // pointer out of it.
    let raw = unsafe {
        ffi_dispatch!(
            sys::wayland_sys::server::wayland_server_handle(),
            wl_display_get_event_loop,
            display.as_ptr()
        )
    };

    // A live display always has an event loop; libwayland creates it in
    // `wl_display_create` and never tears it down separately. A null here would
    // mean libwayland broke its own contract, which is a bug worth panicking
    // loudly on rather than threading through a `Result` that every caller
    // would have to unwrap anyway.
    NonNull::new(raw).expect("display has no event loop")
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
///
/// Privately carries the display it came from as well as the loop, which is
/// where this branch's one real difference from `wlr 0.20` lives. 0.17's
/// `wlr_backend_autocreate` takes a `*mut wl_display`; 0.20's takes a
/// `*mut wl_event_loop`. Since [`Backend::autocreate`](crate::Backend::autocreate)
/// takes an `&EventLoop` on every branch — the public API is held identical
/// across them — this branch has to be able to get from the loop handle back to
/// its display, and libwayland offers no `wl_event_loop_get_display` to do it
/// with. So the display pointer rides along from the moment
/// [`Display::event_loop`] mints the handle, and the public surface is
/// untouched.
pub struct EventLoop<'d> {
    raw: NonNull<sys::wl_event_loop>,

    /// The display `raw` belongs to. Never dereferenced here; handed to
    /// wlroots by `Backend::autocreate`, and re-derived from by
    /// [`EventLoop::from_raw`].
    display: NonNull<sys::wl_display>,

    _owner: PhantomData<&'d Display>,
}

impl<'d> EventLoop<'d> {
    /// Re-borrow a display pointer this crate already holds, as its loop.
    ///
    /// Takes the *display* rather than the loop, unlike the 0.19 and 0.20
    /// branches, and that is what keeps this sound rather than merely
    /// convenient: an `EventLoop` on this branch must carry both pointers, and
    /// deriving the loop here means the pair can never disagree. Handing this
    /// two separately-stored pointers would let a caller build a handle whose
    /// display belongs to some other loop — which `Backend::autocreate` would
    /// then pass straight to wlroots.
    ///
    /// # Safety
    ///
    /// `display` must name a live `wl_display` that outlives `'d`.
    /// [`Backend`](crate::Backend) is the only caller: it keeps the display it
    /// was created from, and its own `'d` is that display's.
    pub(crate) unsafe fn from_raw(display: NonNull<sys::wl_display>) -> EventLoop<'d> {
        // SAFETY: the caller guarantees the display is live.
        let raw = unsafe { loop_of(display) };
        EventLoop {
            raw,
            display,
            _owner: PhantomData,
        }
    }

    /// The display this loop belongs to, still carrying its non-nullness.
    ///
    /// Two callers, both in `backend.rs`: `wlr_backend_autocreate` wants it as
    /// a bare pointer, and `Backend` stores it so `run` can rebuild this handle.
    pub(crate) fn display(&self) -> NonNull<sys::wl_display> {
        self.display
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
            loop_.raw.as_ptr().cast::<std::ffi::c_void>(),
            display.raw.as_ptr().cast::<std::ffi::c_void>(),
            "event_loop() must return the loop's own pointer, not the display's"
        );
    }

    /// ...and the handle also carries the display it came from, which is what
    /// `Backend::autocreate` hands to 0.17's `wlr_backend_autocreate`.
    ///
    /// This is the branch-specific half of the previous test, and it is not
    /// redundant with it: `EventLoop::display` is a `NonNull` too, so the only
    /// thing worth asserting is that it names *this* display. A handle
    /// carrying some other display's pointer would compile, would pass every
    /// other test in this crate, and would hand wlroots a backend wired to the
    /// wrong loop — which is exactly the mistake `from_raw` taking the display
    /// (rather than both pointers) exists to make unwritable.
    #[test]
    fn an_event_loop_remembers_its_display() {
        let display = Display::new().expect("wl_display_create failed");
        let loop_ = display.event_loop();
        assert_eq!(
            loop_.display().as_ptr(),
            display.raw.as_ptr(),
            "the loop handle must carry the display it was derived from"
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
