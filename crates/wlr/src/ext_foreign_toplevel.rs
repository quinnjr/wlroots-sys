//! `ext_foreign_toplevel_list_v1`: a compositor exports the toplevels it
//! manages as protocol object handles, for a taskbar, a screen-share picker or
//! any client that needs a handle before it can reach the toplevel's pixels.
//!
//! Unlike [`foreign_toplevel`](crate::foreign_toplevel), this protocol is
//! observation only: the client cannot activate or close anything through it.
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

use std::cell::{Cell, RefCell};
use std::ffi::{CString, c_void};
use std::ptr::NonNull;

use crate::backend::{Registration, bound_session, remove_listener};
use crate::runtime::copy_nullable_string;
use crate::{Display, Error, Result, Runtime, sys};

/// A snapshot of an exported toplevel's mutable state.
///
/// Returned by [`ExtForeignToplevelHandle::state`] and taken by
/// [`ExtForeignToplevelHandle::update_state`]. Every field is owned, so the
/// snapshot outlives the call and no wlroots pointer escapes. `None` on a field
/// means wlroots stores no value for it (an empty string is a real value).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtForeignToplevelState {
    /// The window title the compositor last set.
    pub title: Option<String>,
    /// The application id the compositor last set.
    pub app_id: Option<String>,
}

/// The list-death watch shared by an owned [`ExtForeignToplevelHandle`] and its
/// callback.
///
/// Heap-stable: the handle boxes it and never moves the box's contents, so the
/// listener may name its address for the registration's whole life.
struct HandleListeners {
    /// The runtime the handle was created against, read once at creation to
    /// look up the display-owned list and link the watch. Holding this clone
    /// does not keep the list (or any object) alive; it is a handle to the
    /// runtime, not the object being watched.
    runtime: Runtime,
    /// The live handle, until `alive` is cleared.
    raw: NonNull<sys::wlr_ext_foreign_toplevel_handle_v1>,
    /// False once the list has been destroyed (display teardown) or the handle's
    /// own `Drop` has run. Every accessor and mutator is a miss while it is
    /// false.
    alive: Cell<bool>,
    /// The list's `destroy` watch, unlinked by its own callback or by the
    /// handle's `Drop`.
    list_destroy: RefCell<Option<Registration>>,
}

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
        let listeners = Box::new(HandleListeners {
            runtime,
            raw,
            alive: Cell::new(true),
            list_destroy: RefCell::new(None),
        });
        // The box address is stable from here on, so the watch may name it.
        let session: *const () = (&*listeners as *const HandleListeners).cast();

        let list = listeners.runtime.ext_foreign_toplevel_list_ptr();
        if let Some(list) = list {
            // SAFETY: the list is live and its `destroy` signal is initialised;
            // `listeners` (session and its `alive` cell) outlives this
            // registration.
            let watch = unsafe {
                Registration::link_watched(
                    &raw mut (*list.as_ptr()).events.destroy,
                    on_list_destroy,
                    session,
                    &listeners.alive,
                )
            };
            *listeners.list_destroy.borrow_mut() = Some(watch);
        }

        ExtForeignToplevelHandle { listeners }
    }

    /// Whether the handle is still live.
    ///
    /// `false` only once the list has been destroyed (display teardown); every
    /// accessor and mutator then reports a miss rather than dereferencing freed
    /// memory.
    pub fn is_alive(&self) -> bool {
        self.listeners.alive.get()
    }

    /// The live wlroots handle, or `None` once this handle is inert.
    fn raw(&self) -> Option<NonNull<sys::wlr_ext_foreign_toplevel_handle_v1>> {
        self.is_alive().then_some(self.listeners.raw)
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
    /// An inert handle reports [`ExtForeignToplevelState::default`].
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

    /// Set the state clients see. `None` for an inert handle, or for a string
    /// containing an interior NUL, which cannot be passed to wlroots.
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
        if !self.listeners.alive.get() {
            // The list died first and its callback already unlinked the watch;
            // the wlroots handle must not be touched.
            return;
        }
        let watch = self.listeners.list_destroy.borrow_mut().take();
        drop(watch);
        self.listeners.alive.set(false);

        // SAFETY: `alive` was true, so the caller's sole-owner contract holds
        // and wlroots frees the handle exactly once.
        unsafe { sys::wlr_ext_foreign_toplevel_handle_v1_destroy(self.listeners.raw.as_ptr()) };
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
    /// client sees it (and the state passed here) at once. `None` when no list
    /// was created, or when wlroots could not allocate the handle.
    ///
    /// Drop the handle before the display: see [`ExtForeignToplevelHandle`].
    pub fn create_ext_foreign_toplevel(
        &self,
        state: &ExtForeignToplevelState,
    ) -> Option<ExtForeignToplevelHandle> {
        let list = self.ext_foreign_toplevel_list_ptr()?;
        let raw = with_raw_state(state, |state| {
            // SAFETY: `list` is live and owned by the display, and `state` names
            // the two NUL-terminated strings built for this call; the returned
            // handle is freshly allocated and linked into the list.
            unsafe { sys::wlr_ext_foreign_toplevel_handle_v1_create(list.as_ptr(), state) }
        })?;
        let raw = NonNull::new(raw)?;
        // SAFETY: `raw` is a fresh handle with initialised signals and this is
        // its only owner; `from_non_null` links the list-death watch.
        Some(unsafe { ExtForeignToplevelHandle::from_non_null(self.clone(), raw) })
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

/// Recover the [`HandleListeners`] a watch was linked with.
///
/// # Safety
///
/// `l` must be a listener linked with a `HandleListeners` address as its
/// session, and that box must still be alive.
unsafe fn ctx_of<'a>(l: *mut sys::wl_listener) -> Option<&'a HandleListeners> {
    // SAFETY: the caller guarantees `l` is a `Registration` listener, so
    // `bound_session` recovers its live `session`.
    let session = unsafe { bound_session(l) };
    if session.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees the session names a live `HandleListeners`.
    Some(unsafe { &*session.cast::<HandleListeners>() })
}

/// The list is being destroyed (display teardown, before the list itself is
/// freed).
///
/// Marks the owned handle inert and unlinks the watch while the handle memory is
/// still valid, so a later `Drop` never touches the freed list.
unsafe extern "C" fn on_list_destroy(l: *mut sys::wl_listener, _data: *mut c_void) {
    // SAFETY: linked with a `HandleListeners` session and its `alive` cell; the
    // registration below is dropped from inside this emission, while its own
    // list's signal is still alive.
    unsafe {
        let Some(ctx) = ctx_of(l) else { return };
        ctx.alive.set(false);
        remove_listener(l);
        let registration = ctx.list_destroy.borrow_mut().take();
        drop(registration);
    }
}
