//! `ext_foreign_toplevel_list_v1`: a compositor exports the toplevels it
//! manages as protocol object handles, for a taskbar, a screen-share picker or
//! any client that needs a handle before it can reach the toplevel's pixels.
//!
//! # Privileged: this global broadcasts window state to whoever binds it
//!
//! The protocol carries no client requests — a bound client cannot activate or
//! close anything through it — but that does not make it safe to expose. Every
//! client that binds the list receives a handle for **every** exported window
//! together with its current title, app id and identifier, and keeps receiving
//! updates for as long as it stays bound. That is sensitive cross-app data
//! (which windows exist, what they are titled), readable by any process that
//! can connect, and a ready-made sandbox-enumeration primitive: "observation
//! only" describes the request direction, not the exposure.
//!
//! Gate the list global at bind time — see the rule in
//! [`Runtime::lookup_security_context`](crate::Runtime::lookup_security_context).
//!
//! A compositor calls [`Runtime::create_ext_foreign_toplevel_list`] once and
//! then mints one owned [`ExtForeignToplevelHandle`] per window with
//! [`Runtime::create_ext_foreign_toplevel`], updating it through
//! [`ExtForeignToplevelHandle::update_state`] as the window's title or app id
//! changes. A client that binds the list receives a handle for every export
//! and its current state.
//!
//! # Ownership
//!
//! [`ExtForeignToplevelHandle`] is owned by the compositor, like
//! [`ForeignToplevelHandle`](crate::ForeignToplevelHandle): `Drop` is the
//! single release path and there is no public `destroy`, so a handle cannot be
//! released twice. Drop unlinks the handle's list-death watch and calls
//! `wlr_ext_foreign_toplevel_handle_v1_destroy`, which sends `closed` to every
//! client holding it. Nothing here is delivered to a handler — the protocol has
//! no client requests — so a drop from inside a handler needs no deferral.
//!
//! A handle must still be dropped before the [`Display`](crate::Display) it was
//! created against: wlroots frees the list with the display, and destroying a
//! handle afterwards dereferences the freed list. The handle watches the
//! list's `destroy` signal and becomes inert once it fires, so a late `Drop` is
//! a no-op rather than a use-after-free.

use std::ffi::CString;
use std::ptr::NonNull;

use crate::backend::{Registration, bound_session, remove_listener};
use crate::owned_handle::{OwnedHandle, on_watched_destroy};
use crate::runtime::{RuntimeInner, copy_nullable_string};
use crate::{Display, Error, Result, Runtime, sys};

/// A snapshot of an exported toplevel's mutable state.
///
/// Returned by [`ExtForeignToplevelHandle::state`] and taken by
/// [`ExtForeignToplevelHandle::update_state`]. Every field is owned, so the
/// snapshot outlives the call and no wlroots pointer escapes. `None` on a field
/// means wlroots stores no value for it (an empty string is a real value).
///
/// New-in-milestone and unreleased: marked [`#[non_exhaustive]`] so future
/// protocol fields can be added without breaking downstream construction.
/// Exhaustiveness was never promised for this snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ExtForeignToplevelState {
    /// The window title the compositor last set.
    pub title: Option<String>,
    /// The application id the compositor last set.
    pub app_id: Option<String>,
}

/// The list-death watch shared by an owned [`ExtForeignToplevelHandle`] and its
/// callback.
///
/// [`OwnedHandle`](crate::owned_handle::OwnedHandle) specialised to this
/// module's object: heap-stable, so the listener may name its address for the
/// registration's whole life. See that module for the unlink ordering both
/// paths below preserve.
type HandleListeners = OwnedHandle<sys::wlr_ext_foreign_toplevel_handle_v1>;

/// An exported toplevel, owned by the compositor.
///
/// Created by [`Runtime::create_ext_foreign_toplevel`]. Drop is the single
/// release path (see the module docs): it unlinks the list-death watch and
/// destroys the wlroots object, telling every client that holds it the window is
/// closed.
pub struct ExtForeignToplevelHandle {
    listeners: Box<HandleListeners>,
}

impl std::fmt::Debug for ExtForeignToplevelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtForeignToplevelHandle")
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

impl ExtForeignToplevelHandle {
    /// Take ownership of a handle wlroots just created and link its watch.
    ///
    /// # Safety
    ///
    /// `raw` must be a handle returned by `wlr_ext_foreign_toplevel_handle_v1_create`
    /// that has not been destroyed, with its `destroy` signals initialised and
    /// its list still alive, and the returned handle must be its only owner.
    pub(crate) unsafe fn from_non_null(
        runtime: Runtime,
        raw: NonNull<sys::wlr_ext_foreign_toplevel_handle_v1>,
    ) -> ExtForeignToplevelHandle {
        let listeners: Box<HandleListeners> = OwnedHandle::boxed(runtime, raw);
        // The box address is stable from here on, so the watch may name it.
        let session: *const () = OwnedHandle::session(&listeners);

        let list = listeners.runtime.ext_foreign_toplevel_list_ptr();
        if let Some(list) = list {
            // SAFETY: the list is live and its `destroy` signal is initialised;
            // `listeners` (session and its `alive` cell) outlives this
            // registration.
            let watch = unsafe {
                Registration::link_watched(
                    &raw mut (*list.as_ptr()).events.destroy,
                    on_watched_destroy::<sys::wlr_ext_foreign_toplevel_handle_v1>,
                    session,
                    &listeners.alive,
                )
            };
            *listeners.watched_destroy.borrow_mut() = Some(watch);
        }

        ExtForeignToplevelHandle { listeners }
    }

    /// Whether the handle is still live.
    ///
    /// `false` only once the list has been destroyed (display teardown); every
    /// accessor and mutator then reports a miss rather than dereferencing freed
    /// memory.
    pub fn is_alive(&self) -> bool {
        self.listeners.is_alive()
    }

    /// The live wlroots handle, or `None` once this handle is inert.
    fn raw(&self) -> Option<NonNull<sys::wlr_ext_foreign_toplevel_handle_v1>> {
        self.listeners.live_raw()
    }

    /// The handle's stable identifier, as the protocol reports it.
    ///
    /// wlroots mints this at creation; two handles carrying the same identifier
    /// name the same underlying toplevel across list instances. `None` for an
    /// inert handle.
    pub fn identifier(&self) -> Option<String> {
        let raw = self.raw()?;
        // SAFETY: `raw` is live; `identifier` is a NUL-terminated string wlroots
        // owns, copied out here.
        unsafe { copy_nullable_string((*raw.as_ptr()).identifier) }
    }

    /// The toplevel's mutable state, copied out.
    ///
    /// An inert handle reports [`ExtForeignToplevelState::default`], which is
    /// also a legitimate empty state — check [`is_alive`](Self::is_alive) to
    /// tell the two apart.
    #[must_use]
    pub fn state(&self) -> ExtForeignToplevelState {
        let Some(raw) = self.raw() else {
            return ExtForeignToplevelState::default();
        };
        // SAFETY: `raw` is live; both fields are null or NUL-terminated strings
        // wlroots owns, copied out here.
        unsafe {
            let handle = raw.as_ptr();
            ExtForeignToplevelState {
                title: copy_nullable_string((*handle).title),
                app_id: copy_nullable_string((*handle).app_id),
            }
        }
    }

    /// Set the state clients see.
    ///
    /// `None` for two distinct misses, both silent by design (there is no
    /// [`Error`](crate::Error) variant that names one without stretching its
    /// documented semantics, and this signature is frozen within the 0.20.x
    /// line): the handle is inert — its list died with the display — or a
    /// field contains an interior NUL, which cannot be passed to wlroots and
    /// is refused rather than truncated, leaving the previous state in place.
    ///
    /// wlroots copies both strings into its own storage, so the state's
    /// `String`s are only borrowed for the call.
    pub fn update_state(&self, state: &ExtForeignToplevelState) -> Option<()> {
        let raw = self.raw()?;
        with_raw_state(state, |state| {
            // SAFETY: `raw` is live and the state's pointers name the two
            // NUL-terminated strings built for this call.
            unsafe { sys::wlr_ext_foreign_toplevel_handle_v1_update_state(raw.as_ptr(), state) };
        })
    }
}

impl Drop for ExtForeignToplevelHandle {
    fn drop(&mut self) {
        let Some(raw) = self.listeners.take_live_raw() else {
            // The list died first and its callback already unlinked the watch;
            // the wlroots handle must not be touched.
            return;
        };

        // SAFETY: `take_live_raw` returned `Some`, so the caller's sole-owner
        // contract holds and wlroots frees the handle exactly once.
        unsafe { sys::wlr_ext_foreign_toplevel_handle_v1_destroy(raw.as_ptr()) };
    }
}

impl Runtime {
    /// Create the `ext_foreign_toplevel_list_v1` global. Errors if called
    /// twice.
    ///
    /// The list lives and dies with `display`; this crate never frees it.
    /// Handles are created against it with
    /// [`Runtime::create_ext_foreign_toplevel`] and must be dropped before the
    /// display.
    pub fn create_ext_foreign_toplevel_list(&self, display: &Display, version: u32) -> Result<()> {
        if self.inner.ext_foreign_toplevel_list.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_ext_foreign_toplevel_list called twice",
            ));
        }
        // SAFETY: `display` is live for the call; wlroots owns the returned list
        // and frees it with the display.
        let raw =
            unsafe { sys::wlr_ext_foreign_toplevel_list_v1_create(display.as_ptr(), version) };
        let raw =
            NonNull::new(raw).ok_or(Error::Create("wlr_ext_foreign_toplevel_list_v1_create"))?;
        *self.inner.ext_foreign_toplevel_list.borrow_mut() = Some(raw);
        // Link the teardown watch before returning: the list dies with the
        // display, and without this the stored pointer would dangle across
        // display teardown. The watch clears the pointer and the liveness
        // flag from inside the list's own `destroy` emission, while the list
        // memory is still valid.
        self.inner.ext_foreign_toplevel_list_alive.set(true);
        // SAFETY: `raw` is a live list with initialised signals, and
        // `self.inner` (the session pointer and the `alive` cell) outlives
        // the registration: both live in the same `Rc`-allocated
        // `RuntimeInner`, whose heap address never moves, and the callback
        // unlinks itself before either can go stale. `link_watched`'s
        // contract otherwise forwarded verbatim.
        let watch = unsafe {
            Registration::link_watched(
                &raw mut (*raw.as_ptr()).events.destroy,
                on_ext_foreign_toplevel_list_destroy,
                std::rc::Rc::as_ptr(&self.inner).cast::<()>(),
                &self.inner.ext_foreign_toplevel_list_alive as *const _,
            )
        };
        *self.inner.ext_foreign_toplevel_list_destroy.borrow_mut() = Some(watch);
        Ok(())
    }

    /// The list pointer, once created.
    pub(crate) fn ext_foreign_toplevel_list_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_ext_foreign_toplevel_list_v1>> {
        *self.inner.ext_foreign_toplevel_list.borrow()
    }

    /// Export a toplevel and return the owned handle for it.
    ///
    /// The handle is registered with the list immediately, so every bound
    /// client sees it (and the state passed here) at once.
    ///
    /// `None` for three distinct misses, all silent by design (there is no
    /// [`Error`](crate::Error) variant that names one without stretching its
    /// documented semantics, and this signature is frozen within the 0.20.x
    /// line): no list was ever created; the list died with its display (the
    /// teardown watch linked at creation cleared the stored pointer, so a
    /// post-teardown call misses rather than dereferencing freed memory); or
    /// a state field contains an interior NUL, which cannot be passed to
    /// wlroots and is refused rather than truncated. A null return from
    /// wlroots itself with a live list is an allocation failure the test
    /// suite asserts never happens (`debug_assert!(false, ...)` fires there,
    /// with no release behavior change).
    ///
    /// Drop the handle before the display: see [`ExtForeignToplevelHandle`].
    pub fn create_ext_foreign_toplevel(
        &self,
        state: &ExtForeignToplevelState,
    ) -> Option<ExtForeignToplevelHandle> {
        if !self.inner.ext_foreign_toplevel_list_alive.get() {
            return None;
        }
        let list = self.ext_foreign_toplevel_list_ptr()?;
        let raw = with_raw_state(state, |state| {
            // SAFETY: the liveness flag above is true, so the list's
            // `destroy` has not fired and the display-owned `list` is still
            // live, and `state` names the two NUL-terminated strings built
            // for this call; the returned handle is freshly allocated and
            // linked into the list.
            unsafe { sys::wlr_ext_foreign_toplevel_handle_v1_create(list.as_ptr(), state) }
        })?;
        let raw = match NonNull::new(raw) {
            Some(raw) => raw,
            // Unexpected: the list is live, so only an allocation failure
            // explains a null return. Asserted in debug like the other
            // should-never-fire arms in this crate; still a silent miss,
            // never trapped.
            None => {
                debug_assert!(
                    false,
                    "wlr_ext_foreign_toplevel_handle_v1_create returned null with a live list"
                );
                return None;
            }
        };
        // SAFETY: `raw` is a fresh handle with initialised signals and this is
        // its only owner; `from_non_null` links the list-death watch.
        Some(unsafe { ExtForeignToplevelHandle::from_non_null(self.clone(), raw) })
    }
}

/// The ext-foreign-toplevel list is being destroyed (display teardown, before
/// the list itself is freed).
///
/// Clears the stored list pointer and the liveness flag while the list memory
/// is still valid, so a later [`Runtime::create_ext_foreign_toplevel`]
/// returns `None` instead of dereferencing freed memory, and unlinks this
/// listener so wlroots' post-destroy empty-signal assertion holds. A later
/// `RuntimeInner` drop then finds the flag false and the registration gone,
/// and never touches the freed signal list.
///
/// This is the list side of the teardown story; each owned
/// [`ExtForeignToplevelHandle`]'s own watch (the shared owned-handle
/// list-death watch) marks that handle inert at the same emission.
unsafe extern "C" fn on_ext_foreign_toplevel_list_destroy(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `create_ext_foreign_toplevel_list` into a live
    // list's `events.destroy` with a `session` pointing at the owning
    // `RuntimeInner`, which outlives the registration. Every call below is
    // infallible and cannot unwind out of this `extern "C"` frame.
    unsafe {
        let session = bound_session(l);
        if session.is_null() {
            return;
        }
        let inner = &*session.cast::<RuntimeInner>();
        inner.ext_foreign_toplevel_list_alive.set(false);
        *inner.ext_foreign_toplevel_list.borrow_mut() = None;
        remove_listener(l);
        let registration = inner.ext_foreign_toplevel_list_destroy.borrow_mut().take();
        drop(registration);
    }
}

/// Build the C state struct for a call, keeping the strings alive only for it.
///
/// wlroots copies both strings into its own storage, so the `CString`s may be
/// dropped when `f` returns. `None` when a field contains an interior NUL,
/// which cannot be passed to wlroots.
fn with_raw_state<R>(
    state: &ExtForeignToplevelState,
    f: impl FnOnce(*const sys::wlr_ext_foreign_toplevel_handle_v1_state) -> R,
) -> Option<R> {
    let title = state.title.as_deref().map(CString::new).transpose().ok()?;
    let app_id = state.app_id.as_deref().map(CString::new).transpose().ok()?;
    let raw = sys::wlr_ext_foreign_toplevel_handle_v1_state {
        title: title.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
        app_id: app_id.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
    };
    Some(f(&raw))
}
