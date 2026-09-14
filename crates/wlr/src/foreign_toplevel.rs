//! `zwlr_foreign_toplevel_management_v1`: a compositor exports the toplevels it
//! manages, and taskbars, docks and window switchers observe and drive them.
//!
//! The protocol splits the two directions:
//!
//! * The compositor **exports** a window with
//!   [`Runtime::create_foreign_toplevel`], an owned [`ForeignToplevelHandle`]
//!   that it keeps for as long as the window lives and drives through the
//!   `set_*` mutators — title, app id, the maximize/minimize/activate/fullscreen
//!   state bits, its parent, and which outputs it is on. A client that binds the
//!   manager receives a handle for every export and its current state.
//! * A client **requests** an action on that window — activate, close,
//!   maximize, minimize, fullscreen, or draw a rectangle against one of its
//!   surfaces. Those land on the defaulted `ToplevelHandler::foreign_toplevel_*`
//!   methods. wlroots applies none of them; every one is the compositor's to
//!   decide, exactly like `SeatHandler::request_activate`.
//!
//! # Ownership
//!
//! [`ForeignToplevelHandle`] is the first **owned** object whose per-object
//! signals are delivered to a handler. Drop is the single release path: it
//! unlinks this handle's request listeners and its manager-death watch, then
//! calls `wlr_foreign_toplevel_handle_v1_destroy`, which sends `closed` to every
//! client holding it. There is no public `destroy` method, so a handle cannot be
//! released twice — dropping it is the only way. When the drop happens *inside*
//! a handler — a compositor dropping the handle from
//! [`ToplevelHandler::foreign_toplevel_close`](crate::ToplevelHandler::foreign_toplevel_close),
//! say — the wlroots destroy is deferred to the end of the dispatch turn, so the
//! request signal wlroots is still emitting is not freed underneath it; the
//! listeners are unlinked immediately either way.
//!
//! A handle must be dropped before the [`Display`](crate::Display) it was
//! created against. wlroots frees the manager with the display, and destroying a
//! handle afterwards dereferences the freed manager. The handle watches the
//! manager's `destroy` signal and becomes inert once it fires, so a late `Drop`
//! is a no-op rather than a use-after-free — but the handle's own state is not
//! observable past that point, so keeping one alive past its display is still a
//! mistake, just a survivable one.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::NonNull;

use crate::backend::{Registration, bound_session, remove_listener};
use crate::id::find_surface_id;
use crate::{Display, Error, Output, Result, Runtime, SurfaceId, sys};

/// Identifies one exported toplevel while the compositor holds its handle.
///
/// Opaque to consumers, like [`ToplevelId`](crate::ToplevelId): the compositor
/// receives one from [`ForeignToplevelHandle::id`] and matches it against the
/// id a request handler was handed. The wrapped value is the handle's own
/// address, which is the identity wlroots' request events carry; hiding it keeps
/// that an implementation detail.
///
/// Valid only while a handle is alive: the address may be reused after the handle is dropped.
///
/// Deliberately no `PartialOrd`/`Ord`, matching the other id types in this
/// crate: an opaque id's ordering would promise creation-order semantics nobody
/// asked for, and this API is frozen within the wlroots minor.
///
/// `Debug` is redacted for the same reason [`ToplevelId`](crate::ToplevelId)'s
/// siblings are: the wrapped value is a heap address, and printing it would hand
/// out an ASLR oracle.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForeignToplevelId(pub(crate) usize);

impl std::fmt::Debug for ForeignToplevelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ForeignToplevelId(..)")
    }
}

/// A snapshot of an exported toplevel's state, copied out of its live handle.
///
/// Returned by [`ForeignToplevelHandle::state`]. Every field is owned, so the
/// snapshot outlives the call and no wlroots pointer escapes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ForeignToplevelState {
    /// The window title the compositor last set, `None` until it sets one.
    pub title: Option<String>,
    /// The application id the compositor last set, `None` until it sets one.
    pub app_id: Option<String>,
    /// The compositor reports the window maximized.
    pub maximized: bool,
    /// The compositor reports the window minimized.
    pub minimized: bool,
    /// The compositor reports the window activated (focused).
    pub activated: bool,
    /// The compositor reports the window fullscreen.
    pub fullscreen: bool,
    /// The parent this handle names, if any.
    pub parent: Option<ForeignToplevelId>,
}

/// A client request against one exported toplevel, as the wlroots signal
/// carried it.
///
/// Crate-private: it is the erased payload between a handle's request listener
/// and the run that delivers it to a [`ToplevelHandler`](crate::ToplevelHandler).
#[derive(Clone, Copy)]
pub(crate) enum ForeignToplevelRequest {
    /// `zwlr_foreign_toplevel_handle_v1.activate`. The seat the client named is
    /// deliberately not carried: this crate has no seat id, and the compositor's
    /// focus policy is its own.
    Activate { id: ForeignToplevelId },
    /// `zwlr_foreign_toplevel_handle_v1.close`.
    Close { id: ForeignToplevelId },
    /// `set_maximized`/`unset_maximized`; the bool is the requested target.
    Maximize {
        id: ForeignToplevelId,
        maximized: bool,
    },
    /// `set_minimized`/`unset_minimized`; the bool is the requested target.
    Minimize {
        id: ForeignToplevelId,
        minimized: bool,
    },
    /// `set_fullscreen`/`unset_fullscreen`; the bool is the requested target.
    Fullscreen {
        id: ForeignToplevelId,
        fullscreen: bool,
    },
    /// `set_rectangle`: the surface and the rectangle the client wants the
    /// compositor to treat as the window's interactive area.
    SetRectangle {
        id: ForeignToplevelId,
        surface: Option<SurfaceId>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
}

/// The live run's ability to deliver a foreign-toplevel request to its handler.
///
/// The request listeners belong to the owned handle, which outlives any one
/// [`Backend::run_all`](crate::Backend::run_all) call — but the `&mut S` and the
/// delivery function exist only inside a run. This is the join: `run_all` plants
/// its erased `Session` pointer and an `S`-monomorphised delivery function on
/// the runtime for its own duration, and a request listener reads this hook when
/// it fires. `Copy`, so reading it out of the `Cell` leaves it installed.
#[derive(Clone, Copy)]
pub(crate) struct ForeignToplevelObserver {
    /// An erased `*const Session<'_, S>`, valid only while this value is
    /// installed. `run_inner`'s guard is what makes that true.
    pub(crate) session: *const (),
    /// Emits the event for `request` through that session's dispatcher.
    ///
    /// # Safety
    ///
    /// The caller must pass `session` verbatim, and that session must still be
    /// the one this hook was installed with.
    pub(crate) request: unsafe fn(*const (), ForeignToplevelRequest),
}

/// The listeners and liveness shared by an owned [`ForeignToplevelHandle`] and
/// its callbacks.
///
/// Heap-stable: the handle boxes it and never moves the box's contents, so every
/// listener may name its address for the registration's whole life. The destroy
/// callback reaches it through `bound_session`, clears both registration sets,
/// and is the only writer of `alive` besides the handle's own `Drop`.
struct HandleListeners {
    /// Keeps the runtime the handle was created against alive, and is where the
    /// request callbacks read the run's [`ForeignToplevelObserver`].
    runtime: Runtime,
    /// The live handle, until `alive` is cleared.
    raw: NonNull<sys::wlr_foreign_toplevel_handle_v1>,
    /// False once the manager has been destroyed (display teardown) or the
    /// handle's own `Drop` has run. Every accessor and mutator is a miss while
    /// it is false.
    alive: Cell<bool>,
    /// The six request listeners, unlinked by the manager-death watch or by the
    /// handle's own `Drop`.
    requests: RefCell<Vec<Registration>>,
    /// The manager's `destroy` watch, unlinked by its own callback or by the
    /// handle's `Drop`.
    manager_destroy: RefCell<Option<Registration>>,
}

/// An exported toplevel, owned by the compositor.
///
/// Created by [`Runtime::create_foreign_toplevel`]. Drop is the single release
/// path (see the module docs): it unlinks this handle's listeners and destroys
/// the wlroots object, telling every client that holds it the window is closed.
pub struct ForeignToplevelHandle {
    listeners: Box<HandleListeners>,
}

impl std::fmt::Debug for ForeignToplevelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignToplevelHandle")
            .field("id", &self.id())
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

impl ForeignToplevelHandle {
    /// Take ownership of a handle wlroots just created and link its listeners.
    ///
    /// # Safety
    ///
    /// `raw` must be a handle returned by `wlr_foreign_toplevel_handle_v1_create`
    /// that has not been destroyed, with every `events.*` signal initialised and
    /// its manager still alive, and the returned handle must be its only owner.
    pub(crate) unsafe fn from_non_null(
        runtime: Runtime,
        raw: NonNull<sys::wlr_foreign_toplevel_handle_v1>,
    ) -> ForeignToplevelHandle {
        let listeners = Box::new(HandleListeners {
            runtime,
            raw,
            alive: Cell::new(true),
            requests: RefCell::new(Vec::new()),
            manager_destroy: RefCell::new(None),
        });
        // The box address is stable from here on, so every listener may name it.
        let session: *const () = (&*listeners as *const HandleListeners).cast();
        // SAFETY: the caller guarantees `raw` is a live handle with initialised
        // signals; `listeners` outlives every registration stored in it.
        let requests = unsafe { link_requests(raw.as_ptr(), session) };
        *listeners.requests.borrow_mut() = requests;

        let manager = listeners.runtime.foreign_toplevel_manager_ptr();
        if let Some(manager) = manager {
            // SAFETY: the manager is live and its `destroy` signal is
            // initialised; `listeners` (session and its `alive` cell) outlives
            // this registration.
            let watch = unsafe {
                Registration::link_watched(
                    &raw mut (*manager.as_ptr()).events.destroy,
                    on_manager_destroy,
                    session,
                    &listeners.alive,
                )
            };
            *listeners.manager_destroy.borrow_mut() = Some(watch);
        }

        ForeignToplevelHandle { listeners }
    }

    /// This handle's stable identity, safe to store beyond a handler call.
    pub fn id(&self) -> ForeignToplevelId {
        ForeignToplevelId(self.listeners.raw.as_ptr() as usize)
    }

    /// Whether the handle is still live.
    ///
    /// `false` only once the manager has been destroyed (display teardown);
    /// every mutator and accessor then reports a miss rather than dereferencing
    /// freed memory.
    pub fn is_alive(&self) -> bool {
        self.listeners.alive.get()
    }

    /// The live wlroots handle, or `None` once this handle is inert.
    fn raw(&self) -> Option<NonNull<sys::wlr_foreign_toplevel_handle_v1>> {
        self.is_alive().then_some(self.listeners.raw)
    }

    /// The window title the compositor last set, if any.
    pub fn title(&self) -> Option<String> {
        let raw = self.raw()?;
        // SAFETY: `raw` is live; `title` is null or a NUL-terminated string
        // wlroots owns, copied out here.
        unsafe { copy_cstr((*raw.as_ptr()).title) }
    }

    /// The application id the compositor last set, if any.
    pub fn app_id(&self) -> Option<String> {
        let raw = self.raw()?;
        // SAFETY: as for `title`.
        unsafe { copy_cstr((*raw.as_ptr()).app_id) }
    }

    /// The handle's full state, copied out.
    ///
    /// Every field is owned, so the snapshot outlives the call and no wlroots
    /// pointer escapes. An inert handle reports [`ForeignToplevelState::default`].
    pub fn state(&self) -> ForeignToplevelState {
        let Some(raw) = self.raw() else {
            return ForeignToplevelState::default();
        };
        // SAFETY: `raw` is live; every field read is a plain scalar, pointer or
        // NUL-terminated string, and the parent pointer is only compared.
        unsafe {
            let handle = raw.as_ptr();
            let parent = (*handle).parent;
            ForeignToplevelState {
                title: copy_cstr((*handle).title),
                app_id: copy_cstr((*handle).app_id),
                maximized: (*handle).state
                    & sys::wlr_foreign_toplevel_handle_v1_state::WLR_FOREIGN_TOPLEVEL_HANDLE_V1_STATE_MAXIMIZED.0
                    != 0,
                minimized: (*handle).state
                    & sys::wlr_foreign_toplevel_handle_v1_state::WLR_FOREIGN_TOPLEVEL_HANDLE_V1_STATE_MINIMIZED.0
                    != 0,
                activated: (*handle).state
                    & sys::wlr_foreign_toplevel_handle_v1_state::WLR_FOREIGN_TOPLEVEL_HANDLE_V1_STATE_ACTIVATED.0
                    != 0,
                fullscreen: (*handle).state
                    & sys::wlr_foreign_toplevel_handle_v1_state::WLR_FOREIGN_TOPLEVEL_HANDLE_V1_STATE_FULLSCREEN.0
                    != 0,
                parent: if parent.is_null() {
                    None
                } else {
                    Some(ForeignToplevelId(parent as usize))
                },
            }
        }
    }

    /// Set the window title clients see. `None` for an inert handle or a title
    /// containing an interior NUL, which cannot be passed to wlroots.
    pub fn set_title(&self, title: &str) -> Option<()> {
        let raw = self.raw()?;
        let title = CString::new(title).ok()?;
        // SAFETY: `raw` is live and `title` is a NUL-terminated string copied
        // into wlroots' own storage by the call.
        unsafe { sys::wlr_foreign_toplevel_handle_v1_set_title(raw.as_ptr(), title.as_ptr()) };
        Some(())
    }

    /// Set the application id clients see. `None` for an inert handle or an id
    /// containing an interior NUL.
    pub fn set_app_id(&self, app_id: &str) -> Option<()> {
        let raw = self.raw()?;
        let app_id = CString::new(app_id).ok()?;
        // SAFETY: as for `set_title`.
        unsafe { sys::wlr_foreign_toplevel_handle_v1_set_app_id(raw.as_ptr(), app_id.as_ptr()) };
        Some(())
    }

    /// Report whether the window is maximized. `None` for an inert handle.
    pub fn set_maximized(&self, maximized: bool) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: `raw` is live; the call only writes a state bit and sends.
        unsafe { sys::wlr_foreign_toplevel_handle_v1_set_maximized(raw.as_ptr(), maximized) };
        Some(())
    }

    /// Report whether the window is minimized. `None` for an inert handle.
    pub fn set_minimized(&self, minimized: bool) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: as for `set_maximized`.
        unsafe { sys::wlr_foreign_toplevel_handle_v1_set_minimized(raw.as_ptr(), minimized) };
        Some(())
    }

    /// Report whether the window is activated (focused). `None` for an inert
    /// handle.
    pub fn set_activated(&self, activated: bool) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: as for `set_maximized`.
        unsafe { sys::wlr_foreign_toplevel_handle_v1_set_activated(raw.as_ptr(), activated) };
        Some(())
    }

    /// Report whether the window is fullscreen. `None` for an inert handle.
    pub fn set_fullscreen(&self, fullscreen: bool) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: as for `set_maximized`.
        unsafe { sys::wlr_foreign_toplevel_handle_v1_set_fullscreen(raw.as_ptr(), fullscreen) };
        Some(())
    }

    /// Set (or clear, with `None`) this window's parent. `None` for an inert
    /// handle, or when `parent` names one.
    pub fn set_parent(&self, parent: Option<&ForeignToplevelHandle>) -> Option<()> {
        let raw = self.raw()?;
        let parent = match parent {
            Some(parent) => parent.raw()?.as_ptr(),
            None => std::ptr::null_mut(),
        };
        // SAFETY: `raw` is live and `parent` is either null or a live handle;
        // wlroots only reads both and sends protocol events.
        unsafe { sys::wlr_foreign_toplevel_handle_v1_set_parent(raw.as_ptr(), parent) };
        Some(())
    }

    /// Report that the window entered `output`. Idempotent per output, matching
    /// wlroots. `None` for an inert handle.
    pub fn output_enter(&self, output: &Output<'_>) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: `raw` is live and `output`'s handle borrows a live output;
        // wlroots links the pair and sends to clients.
        unsafe { sys::wlr_foreign_toplevel_handle_v1_output_enter(raw.as_ptr(), output.as_ptr()) };
        Some(())
    }

    /// Report that the window left `output`. `None` for an inert handle.
    pub fn output_leave(&self, output: &Output<'_>) -> Option<()> {
        let raw = self.raw()?;
        // SAFETY: as for `output_enter`.
        unsafe { sys::wlr_foreign_toplevel_handle_v1_output_leave(raw.as_ptr(), output.as_ptr()) };
        Some(())
    }
}

impl Drop for ForeignToplevelHandle {
    fn drop(&mut self) {
        if !self.listeners.alive.get() {
            // The manager died first and its callback already unlinked every
            // listener; the wlroots handle must not be touched.
            return;
        }
        // Unlink while the handle is still alive, then release it. `Drop` is the
        // single release path: nothing else calls `wlr_..._destroy` on it.
        self.listeners.requests.borrow_mut().clear();
        let manager_destroy = self.listeners.manager_destroy.borrow_mut().take();
        drop(manager_destroy);
        self.listeners.alive.set(false);

        if crate::dispatch::in_delivery() {
            // A client request reaches a handler from inside wlroots'
            // `wlr_signal_emit_safe` on this handle's own request signal.
            // Freeing the handle here would free that signal mid-emission, so
            // the destroy is deferred to the turn's drain; the handle's
            // listeners are already unlinked above, which is safe while it is
            // still allocated.
            self.listeners
                .runtime
                .defer_foreign_toplevel_destroy(self.listeners.raw);
            return;
        }

        // SAFETY: `alive` was true, so the caller's sole-owner contract holds
        // and wlroots frees the handle exactly once.
        unsafe { sys::wlr_foreign_toplevel_handle_v1_destroy(self.listeners.raw.as_ptr()) };
    }
}

impl Runtime {
    /// Create the `zwlr_foreign_toplevel_manager_v1` global. Errors if called
    /// twice.
    ///
    /// The manager lives and dies with `display`; this crate never frees it.
    /// Handles are created against it with
    /// [`Runtime::create_foreign_toplevel`] and must be dropped before the
    /// display.
    pub fn create_foreign_toplevel_manager(&self, display: &Display) -> Result<()> {
        if self.inner.foreign_toplevel_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_foreign_toplevel_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; wlroots owns the returned
        // manager and frees it with the display.
        let raw = unsafe { sys::wlr_foreign_toplevel_manager_v1_create(display.as_ptr()) };
        let raw =
            NonNull::new(raw).ok_or(Error::Create("wlr_foreign_toplevel_manager_v1_create"))?;
        *self.inner.foreign_toplevel_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The manager pointer, once created.
    pub(crate) fn foreign_toplevel_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_foreign_toplevel_manager_v1>> {
        *self.inner.foreign_toplevel_manager.borrow()
    }

    /// Export a toplevel and return the owned handle for it.
    ///
    /// The handle is registered with the manager immediately, so every bound
    /// client sees it (and its current title/app id/state) at once; keep it for
    /// as long as the window lives and drive it through the `set_*` mutators.
    /// `None` when no manager was created, or when wlroots could not allocate
    /// the handle.
    ///
    /// Drop the handle before the display: see [`ForeignToplevelHandle`].
    pub fn create_foreign_toplevel(&self) -> Option<ForeignToplevelHandle> {
        let manager = self.foreign_toplevel_manager_ptr()?;
        // SAFETY: `manager` is live and owned by the display; the returned
        // handle is freshly allocated and linked into the manager's list.
        let raw = unsafe { sys::wlr_foreign_toplevel_handle_v1_create(manager.as_ptr()) };
        let raw = NonNull::new(raw)?;
        // SAFETY: `raw` is a fresh handle with initialised signals and this is
        // its only owner; `from_non_null` links the request listeners.
        Some(unsafe { ForeignToplevelHandle::from_non_null(self.clone(), raw) })
    }
}

/// Link the six request listeners on a freshly created handle.
///
/// # Safety
///
/// `raw` must be a live handle with initialised request signals, and `session`
/// must be the address of the [`HandleListeners`] that outlives them.
unsafe fn link_requests(
    raw: *mut sys::wlr_foreign_toplevel_handle_v1,
    session: *const (),
) -> Vec<Registration> {
    // SAFETY: forwarded from this function's contract. Every `link_bare` call
    // stores `session` and a notify function that recovers it with
    // `bound_session`; the `alive` flag is null (the strongest claim) because
    // the destroy watch clears these registrations before the handle is freed.
    unsafe {
        let events = &raw mut (*raw).events;
        vec![
            Registration::link_bare(
                &raw mut (*events).request_maximize,
                on_request_maximize,
                session,
                std::ptr::null(),
            ),
            Registration::link_bare(
                &raw mut (*events).request_minimize,
                on_request_minimize,
                session,
                std::ptr::null(),
            ),
            Registration::link_bare(
                &raw mut (*events).request_activate,
                on_request_activate,
                session,
                std::ptr::null(),
            ),
            Registration::link_bare(
                &raw mut (*events).request_fullscreen,
                on_request_fullscreen,
                session,
                std::ptr::null(),
            ),
            Registration::link_bare(
                &raw mut (*events).request_close,
                on_request_close,
                session,
                std::ptr::null(),
            ),
            Registration::link_bare(
                &raw mut (*events).set_rectangle,
                on_set_rectangle,
                session,
                std::ptr::null(),
            ),
        ]
    }
}

/// Recover the [`HandleListeners`] a request or destroy listener was linked
/// with.
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

/// Hand a request to the live run, or drop it when no run is on the stack.
///
/// A request can only fire from inside a run's event loop, so the observer is
/// always installed here; the null check covers a handle created before any run
/// (its listener fires only once a run is dispatching) and is defensive.
fn deliver(ctx: &HandleListeners, request: ForeignToplevelRequest) {
    let Some(observer) = ctx.runtime.inner.foreign_toplevel_observer.get() else {
        return;
    };
    // SAFETY: the observer was installed by the live run this callback is
    // running inside, so its session is valid and no handler is mid-dispatch.
    unsafe { (observer.request)(observer.session, request) };
}

/// A client asked to (un)maximize the exported window.
unsafe extern "C" fn on_request_maximize(l: *mut sys::wl_listener, data: *mut c_void) {
    // SAFETY: linked into the handle's `request_maximize`, whose event carries a
    // live `..._maximized_event` for this call; `ctx_of` recovers the session.
    unsafe {
        let Some(ctx) = ctx_of(l) else { return };
        let event = data.cast::<sys::wlr_foreign_toplevel_handle_v1_maximized_event>();
        deliver(
            ctx,
            ForeignToplevelRequest::Maximize {
                id: ctx_id(ctx),
                maximized: (*event).maximized,
            },
        );
    }
}

/// A client asked to (un)minimize the exported window.
unsafe extern "C" fn on_request_minimize(l: *mut sys::wl_listener, data: *mut c_void) {
    // SAFETY: as for `on_request_maximize`, with the minimized event.
    unsafe {
        let Some(ctx) = ctx_of(l) else { return };
        let event = data.cast::<sys::wlr_foreign_toplevel_handle_v1_minimized_event>();
        deliver(
            ctx,
            ForeignToplevelRequest::Minimize {
                id: ctx_id(ctx),
                minimized: (*event).minimized,
            },
        );
    }
}

/// A client asked to activate the exported window. The seat the event names is
/// deliberately not forwarded — see [`ForeignToplevelRequest::Activate`].
unsafe extern "C" fn on_request_activate(l: *mut sys::wl_listener, data: *mut c_void) {
    // SAFETY: linked into the handle's `request_activate`, whose event carries a
    // live `..._activated_event` for this call.
    unsafe {
        let Some(ctx) = ctx_of(l) else { return };
        let event = data.cast::<sys::wlr_foreign_toplevel_handle_v1_activated_event>();
        // The seat the client named is deliberately not forwarded — this crate
        // has no seat id, and focus policy is the compositor's.
        let _ = (*event).seat;
        deliver(ctx, ForeignToplevelRequest::Activate { id: ctx_id(ctx) });
    }
}

/// A client asked to (un)fullscreen the exported window.
unsafe extern "C" fn on_request_fullscreen(l: *mut sys::wl_listener, data: *mut c_void) {
    // SAFETY: as for `on_request_maximize`, with the fullscreen event.
    unsafe {
        let Some(ctx) = ctx_of(l) else { return };
        let event = data.cast::<sys::wlr_foreign_toplevel_handle_v1_fullscreen_event>();
        deliver(
            ctx,
            ForeignToplevelRequest::Fullscreen {
                id: ctx_id(ctx),
                fullscreen: (*event).fullscreen,
            },
        );
    }
}

/// A client asked to close the exported window. wlroots emits this signal with
/// the handle itself as the payload, not an event struct.
unsafe extern "C" fn on_request_close(l: *mut sys::wl_listener, _data: *mut c_void) {
    // SAFETY: linked into the handle's `request_close`.
    unsafe {
        let Some(ctx) = ctx_of(l) else { return };
        deliver(ctx, ForeignToplevelRequest::Close { id: ctx_id(ctx) });
    }
}

/// A client set a rectangle on one of the exported window's surfaces.
unsafe extern "C" fn on_set_rectangle(l: *mut sys::wl_listener, data: *mut c_void) {
    // SAFETY: linked into the handle's `set_rectangle`, whose event carries a
    // live `..._set_rectangle_event` for this call.
    unsafe {
        let Some(ctx) = ctx_of(l) else { return };
        let event = data.cast::<sys::wlr_foreign_toplevel_handle_v1_set_rectangle_event>();
        let surface = (*event).surface;
        let surface = if surface.is_null() {
            None
        } else {
            find_surface_id(&raw const (*surface).addons).map(SurfaceId)
        };
        deliver(
            ctx,
            ForeignToplevelRequest::SetRectangle {
                id: ctx_id(ctx),
                surface,
                x: (*event).x,
                y: (*event).y,
                width: (*event).width,
                height: (*event).height,
            },
        );
    }
}

/// The manager is being destroyed (display teardown, before the manager itself
/// is freed).
///
/// Marks every owned handle inert and unlinks its listeners while the handle
/// memory is still valid, so a later `Drop` never touches the freed manager.
unsafe extern "C" fn on_manager_destroy(l: *mut sys::wl_listener, _data: *mut c_void) {
    // SAFETY: linked with a `HandleListeners` session and its `alive` cell; the
    // registrations below are dropped from inside this emission, while their
    // own signals (the handle's) are still alive.
    unsafe {
        let Some(ctx) = ctx_of(l) else { return };
        ctx.alive.set(false);
        ctx.requests.borrow_mut().clear();
        remove_listener(l);
        let registration = ctx.manager_destroy.borrow_mut().take();
        drop(registration);
    }
}

/// The id of the handle a recovered [`HandleListeners`] owns.
fn ctx_id(ctx: &HandleListeners) -> ForeignToplevelId {
    ForeignToplevelId(ctx.raw.as_ptr() as usize)
}

/// Copy a wlroots-owned C string out, or `None` if it is null.
///
/// # Safety
///
/// `p` must be null or a live, NUL-terminated C string owned by wlroots.
unsafe fn copy_cstr(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `p` is a live NUL-terminated string; this
    // copies it out and never frees it.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}
