//! The `xdg_activation_v1` token object: the compositor-side handle a
//! compositor mints to hand a launching client permission to request focus.
//!
//! The activation protocol splits a focus request in two. A client that wants
//! to be able to activate later asks for a *token*; the compositor (or wlroots,
//! on a client's `get_activation_token`) mints one and gives it a name. The
//! client passes that name to whoever should be activated, and that client
//! redeems it with `activate`. wlroots validates the redemption and raises
//! [`crate::SeatHandler::request_activate`]; applying the focus steal is the
//! compositor's call.
//!
//! This module wraps the *mint* half:
//!
//! * [`ActivationTokenHandle`] owns a token created by
//!   [`Runtime::create_activation_token`] or [`Runtime::add_activation_token`]
//!   and releases it on drop. These are the tokens a compositor hands to a
//!   client it is about to launch (through the environment, a socket, or a
//!   private protocol) — the crate never redeems them itself.
//! * [`Runtime::find_activation_token`] looks one up by name and copies out
//!   the same [`crate::ActivationToken`] snapshot
//!   [`crate::SeatHandler::request_activate`] receives. It returns a snapshot
//!   rather than an owned handle because wlroots keeps the token itself: it
//!   may be destroyed by the client, by a redemption, or by the manager's
//!   display teardown, none of which this crate controls.

use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::ptr::NonNull;

use crate::backend::Registration;
use crate::id::find_id;
use crate::{ActivationToken, Runtime, ToplevelId, sys};

/// An owned `wlr_xdg_activation_token_v1`.
///
/// Created by [`Runtime::create_activation_token`] or
/// [`Runtime::add_activation_token`], both of which leave the token in the
/// manager's token pool as well as handing it over. Dropping the handle calls
/// `wlr_xdg_activation_token_v1_destroy`, which unlinks the token from that
/// pool and frees it — so a token is only redeemable for as long as some handle
/// holds it. Keep it alive until the client that will redeem it has done so.
///
/// Distinct from [`crate::ActivationToken`], the value snapshot delivered to
/// [`crate::SeatHandler::request_activate`]: this handle *owns* a live C object,
/// while that snapshot is plain copied-out data with no lifetime.
///
/// # A token can die without this handle
///
/// wlroots destroys a token on its own — a created token on its 30-second
/// timeout, any token when a client redeems it with `activate`. So this handle
/// also watches the token's `destroy` signal; once it fires, the handle marks
/// itself dead and [`Drop`] and every accessor become no-ops instead of
/// dereferencing freed memory. [`is_alive`](Self::is_alive) reports that state.
pub struct ActivationTokenHandle {
    raw: NonNull<sys::wlr_xdg_activation_token_v1>,
    /// The token's `destroy` listener. Declared before `alive` so it is dropped
    /// first: its `Drop` reads the cell `alive` owns.
    _destroy: Registration,
    /// Set `false` when the token's `destroy` signal fires, whether from this
    /// handle's own [`Drop`] or from wlroots destroying the token first.
    alive: Box<Cell<bool>>,
}

impl std::fmt::Debug for ActivationTokenHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivationTokenHandle")
            .field("alive", &self.is_alive())
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

impl ActivationTokenHandle {
    /// Take ownership of a token wlroots just created.
    ///
    /// `NonNull` so the null check lives in the one caller that can miss, and
    /// this constructor stays free of `unwrap`/`expect`.
    ///
    /// # Safety
    ///
    /// `raw` must be a token returned by `wlr_xdg_activation_token_v1_create`
    /// or `wlr_xdg_activation_v1_add_token` that has not been destroyed, with an
    /// initialised `events.destroy` signal, and the returned handle must be its
    /// only owner.
    pub(crate) unsafe fn from_non_null(
        raw: NonNull<sys::wlr_xdg_activation_token_v1>,
    ) -> ActivationTokenHandle {
        let alive = Box::new(Cell::new(true));
        let alive_ptr: *const Cell<bool> = &*alive;
        // SAFETY: the caller guarantees `raw` is a live token with an
        // initialised `destroy` signal, and `alive` (heap-stable) outlives the
        // registration, which is dropped first in `Self`'s field order.
        let destroy = unsafe {
            Registration::link_owner_destroy(&raw mut (*raw.as_ptr()).events.destroy, alive_ptr)
        };
        ActivationTokenHandle {
            raw,
            _destroy: destroy,
            alive,
        }
    }

    /// Whether the token is still alive.
    ///
    /// `true` until wlroots destroys it — by this handle's [`Drop`], by the
    /// 30-second timeout of a created token, or by a client redeeming it. Every
    /// other accessor is a no-op miss once this is `false`.
    pub fn is_alive(&self) -> bool {
        self.alive.get()
    }

    /// The token's name — the string a client passes to `xdg_activation_v1.activate`.
    ///
    /// `None` once the token has died, or only when a live token names nothing,
    /// which cannot happen for a token created here (wlroots generates the name
    /// at creation). The copy is lossy, matching every other copied C string in
    /// this crate: the value is wlroots-generated today, and replacing
    /// spec-violating bytes is preferable to rejecting the whole token.
    pub fn name(&self) -> Option<String> {
        if !self.is_alive() {
            return None;
        }
        // SAFETY: `alive` is true, so the handle's pointer is still a live token.
        let raw = unsafe { sys::wlr_xdg_activation_token_v1_get_name(self.raw.as_ptr()) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is a non-null, NUL-terminated string wlroots owns; it is
        // copied out here and never freed.
        Some(unsafe { CStr::from_ptr(raw).to_string_lossy().into_owned() })
    }

    /// What the token currently carries, as the same snapshot
    /// [`crate::SeatHandler::request_activate`] receives.
    ///
    /// `None` once the token has died. A token minted by this crate carries no
    /// serial, seat or surface until a client attaches them through the
    /// protocol, so this is the all-empty snapshot for one the compositor made
    /// itself.
    #[must_use]
    pub fn snapshot(&self) -> Option<ActivationToken> {
        if !self.is_alive() {
            return None;
        }
        // SAFETY: `alive` is true, so `raw` is a live token.
        Some(unsafe { snapshot_of(self.raw) })
    }
}

impl Drop for ActivationTokenHandle {
    fn drop(&mut self) {
        if !self.is_alive() {
            // wlroots already freed the token (timeout or redemption). The
            // `_destroy` registration below notices the same flag and skips its
            // unlink, so nothing touches freed memory.
            return;
        }
        // SAFETY: `alive` is true, so this is the sole owner of a live token
        // (the constructor's contract) and wlroots frees it exactly once.
        // `destroy` unlinks it from the manager's pool and emits its `destroy`
        // signal, which flips `alive` before `_destroy` is dropped.
        unsafe { sys::wlr_xdg_activation_token_v1_destroy(self.raw.as_ptr()) };
    }
}

/// Copy a live token's evidence out as the snapshot handler methods receive.
///
/// The requesting toplevel is resolved through the surface's *role* addon, the
/// same lookup `backend.rs`'s `toplevel_id_of_surface` uses — never a signal
/// `data`. The popup check mirrors that helper: since 0.20.28 a popup surface
/// carries an id addon of its own, and `find_id` cannot tell a `PopupId` from
/// a `ToplevelId`, so a popup must be filtered out rather than mislabelled.
///
/// # Safety
///
/// `raw` must point at a live `wlr_xdg_activation_token_v1`.
unsafe fn snapshot_of(raw: NonNull<sys::wlr_xdg_activation_token_v1>) -> ActivationToken {
    let token = raw.as_ptr();
    // SAFETY: the caller guarantees `token` is live; every field read is a
    // plain scalar or pointer, and the surface, when non-null, is the live
    // surface the token recorded.
    unsafe {
        let surface = (*token).surface;
        ActivationToken {
            serial: (*token).serial,
            has_seat: !(*token).seat.is_null(),
            requesting_toplevel: if surface.is_null()
                || !sys::wlr_xdg_popup_try_from_wlr_surface(surface).is_null()
            {
                None
            } else {
                find_id(&raw const (*surface).addons).map(ToplevelId)
            },
        }
    }
}

impl Runtime {
    /// The `xdg_activation_v1` manager, once
    /// [`Runtime::create_xdg_activation_manager`] has run.
    fn activation_manager(&self) -> Option<NonNull<sys::wlr_xdg_activation_v1>> {
        self.xdg_activation_manager_ptr()
    }

    /// Mint a fresh activation token and return an owned handle for it.
    ///
    /// The token is registered with the activation manager, so its name is
    /// redeemable the moment this returns. `None` when no
    /// [`Runtime::create_xdg_activation_manager`] ran, or wlroots could not
    /// allocate the token.
    pub fn create_activation_token(&self) -> Option<ActivationTokenHandle> {
        let manager = self.activation_manager()?;
        // SAFETY: `manager` is a live manager owned by the display, and the
        // returned token is freshly allocated and inserted into the manager's
        // pool; `from_non_null` takes ownership of it.
        let raw = unsafe { sys::wlr_xdg_activation_token_v1_create(manager.as_ptr()) };
        NonNull::new(raw).map(|raw| unsafe { ActivationTokenHandle::from_non_null(raw) })
    }

    /// Register a token under a caller-chosen name and return an owned handle.
    ///
    /// This is the "adopt a token from elsewhere" half of the protocol: the
    /// compositor supplies the name (for example, one it read from a launcher's
    /// environment) and wlroots stores it in the pool so a client can redeem it
    /// with `activate`. The returned handle owns the token; keep it alive until
    /// redemption, or drop it to withdraw the name.
    ///
    /// `None` when no activation manager was created, when `name` contains an
    /// interior NUL, or when wlroots could not allocate the token.
    pub fn add_activation_token(&self, name: &str) -> Option<ActivationTokenHandle> {
        let manager = self.activation_manager()?;
        let name = CString::new(name).ok()?;
        // SAFETY: `manager` is live; the token is freshly allocated and
        // inserted into its pool, and `from_non_null` takes ownership.
        let raw = unsafe { sys::wlr_xdg_activation_v1_add_token(manager.as_ptr(), name.as_ptr()) };
        NonNull::new(raw).map(|raw| unsafe { ActivationTokenHandle::from_non_null(raw) })
    }

    /// Look a registered token up by name and copy out its evidence.
    ///
    /// Returns the [`ActivationToken`] snapshot of the token's serial, seat and
    /// requesting toplevel — the same value
    /// [`crate::SeatHandler::request_activate`] receives. `None` when no
    /// manager was created, when `name` contains an interior NUL, or when no
    /// live token has that name.
    ///
    /// A snapshot rather than an owned handle: wlroots retains the token, and a
    /// client or a redemption may destroy it at any moment. Nothing here keeps
    /// it alive.
    #[must_use]
    pub fn find_activation_token(&self, name: &str) -> Option<ActivationToken> {
        let manager = self.activation_manager()?;
        let name = CString::new(name).ok()?;
        // SAFETY: `manager` is live and `name` is a NUL-terminated string the
        // call only reads. The returned token is borrowed from the manager's
        // pool; the snapshot copies every field out before returning.
        let raw = unsafe { sys::wlr_xdg_activation_v1_find_token(manager.as_ptr(), name.as_ptr()) };
        let raw = NonNull::new(raw)?;
        // SAFETY: the call above returned a live token (non-null checked).
        Some(unsafe { snapshot_of(raw) })
    }
}

#[cfg(test)]
mod tests {
    use super::ActivationTokenHandle;
    use crate::sys;
    use std::ffi::c_void;
    use std::ptr::NonNull;

    // Declared directly so no new dependency is needed; std already links libc.
    // `calloc` rather than a Rust allocation because wlroots frees the token
    // with the C `free` inside `wlr_xdg_activation_token_v1_destroy`.
    unsafe extern "C" {
        fn calloc(nmemb: usize, size: usize) -> *mut c_void;
    }

    /// A heap token wlroots is allowed to free, with the intrusive lists and
    /// `destroy` signal `wlr_xdg_activation_token_v1_destroy` expects.
    struct ScratchToken(*mut sys::wlr_xdg_activation_token_v1);

    impl ScratchToken {
        fn new() -> Self {
            // SAFETY: `calloc` returns null or a zeroed, suitably aligned block
            // of exactly one token.
            let raw = unsafe { calloc(1, std::mem::size_of::<sys::wlr_xdg_activation_token_v1>()) }
                .cast::<sys::wlr_xdg_activation_token_v1>();
            assert!(!raw.is_null(), "calloc failed");
            // SAFETY: `raw` is a live, exclusively-owned, zeroed token. An empty
            // `wl_list` points at itself; `destroy` would walk a null signal, so
            // `events.destroy` is initialised too.
            unsafe {
                (*raw).link.prev = &raw mut (*raw).link;
                (*raw).link.next = &raw mut (*raw).link;
                (*raw).WLR_PRIVATE.seat_destroy.link.prev =
                    &raw mut (*raw).WLR_PRIVATE.seat_destroy.link;
                (*raw).WLR_PRIVATE.seat_destroy.link.next =
                    &raw mut (*raw).WLR_PRIVATE.seat_destroy.link;
                (*raw).WLR_PRIVATE.surface_destroy.link.prev =
                    &raw mut (*raw).WLR_PRIVATE.surface_destroy.link;
                (*raw).WLR_PRIVATE.surface_destroy.link.next =
                    &raw mut (*raw).WLR_PRIVATE.surface_destroy.link;
                sys::wl_signal_init(&raw mut (*raw).events.destroy);
            }
            Self(raw)
        }
    }

    /// wlroots frees the token, so the scratch owner must not free it too.
    impl Drop for ScratchToken {
        fn drop(&mut self) {}
    }

    /// wlroots can destroy a token on its own — a created token on its
    /// 30-second timeout, any token on redemption. The handle must notice and
    /// become an inert no-op: its accessors must not dereference freed memory
    /// and its `Drop` must not free the token a second time. This drives that
    /// exact sequence, with the allocator as the double-free oracle.
    #[test]
    fn a_token_destroyed_by_wlroots_makes_the_handle_a_safe_no_op() {
        let scratch = ScratchToken::new();
        // SAFETY: `scratch` is non-null and outlives the handle.
        let raw = NonNull::new(scratch.0).expect("scratch token is non-null");
        // SAFETY: `raw` is a live token with an initialised destroy signal.
        let handle = unsafe { ActivationTokenHandle::from_non_null(raw) };
        assert!(handle.is_alive(), "a fresh handle owns a live token");

        // Simulate the timeout/redemption destroy: the signal fires the
        // handle's listener and the token is freed underneath it.
        // SAFETY: `raw` is a live token and destroy is its one release path.
        unsafe { sys::wlr_xdg_activation_token_v1_destroy(raw.as_ptr()) };

        assert!(!handle.is_alive(), "the handle saw the token die");
        assert!(handle.name().is_none(), "no name is read off freed memory");
        assert!(handle.snapshot().is_none(), "no snapshot either");
        // Must not double free or unlink from the freed signal list.
        drop(handle);
    }

    /// The ordinary path: the handle owns a live token and dropping it releases
    /// the token exactly once. Repeated under the allocator's scrutiny, a
    /// double free or a use of the freed listener would be reported.
    #[test]
    fn dropping_a_live_handle_destroys_the_token_once() {
        for _ in 0..8 {
            let scratch = ScratchToken::new();
            // SAFETY: `scratch` is non-null and outlives the handle.
            let raw = NonNull::new(scratch.0).expect("scratch token is non-null");
            // SAFETY: `raw` is a live token with an initialised destroy signal.
            let handle = unsafe { ActivationTokenHandle::from_non_null(raw) };
            assert!(handle.is_alive());
            drop(handle);
        }
    }
}
