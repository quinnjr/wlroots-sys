//! Shared owned-handle listener boilerplate.
//!
//! [`OwnedHandle`] is the listener context for an owned wlroots object whose
//! owner (a manager or list, itself freed with the display) may die first: the
//! handle boxes it, links the owner's `destroy` signal as a death watch, and
//! becomes inert when that fires so a later `Drop` never touches freed memory.
//!
//! It covers [`ExtForeignToplevelHandle`](crate::ExtForeignToplevelHandle),
//! [`WorkspaceGroupHandle`](crate::WorkspaceGroupHandle) and
//! [`WorkspaceHandle`](crate::WorkspaceHandle). Those three share one ordering
//! exactly — box the context, link the watch, and on either path (callback or
//! `Drop`) unlink-then-mark-dead before any `destroy` call — so one generic
//! implementation serves all three. The callback marks dead *first* (`alive =
//! false`, unlink the firing listener, drop the stored registration) while
//! `Drop` unlinks first (`take` the registration, drop it, *then* mark dead
//! and destroy); [`take_live_raw`](OwnedHandle::take_live_raw) and
//! [`on_watched_destroy`] each pin one of those two orderings, and neither
//! must be "simplified" into the other.
//!
//! Two owned-handle modules are deliberately **not** migrated here:
//!
//! * `foreign_toplevel`: its context carries a second registration set (the six
//!   per-handle request listeners) that `Drop` and the destroy callback unlink
//!   *before* the manager watch, and its `Drop` defers the wlroots destroy via
//!   `defer_foreign_toplevel_destroy` when run inside a delivery. Folding that
//!   into [`OwnedHandle`] would reorder the unlink sequence or hide the
//!   deferral branch behind a flag.
//! * `xdg_activation`: its token watches its *own* `destroy` signal via
//!   `Registration::link_owner_destroy` (a token dies by itself — timeout or
//!   redemption — not by its manager dying), and its `Drop` destroys first and
//!   lets the signal flip the flag, the reverse of the unlink-then-destroy
//!   order here.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::NonNull;

use crate::backend::Registration;
use crate::backend::{bound_session, remove_listener};
use crate::{Runtime, sys};

/// The owner-death watch shared by an owned handle and its callback.
///
/// Generic over the wlroots object so several handle kinds share one
/// implementation. Heap-stable: the handle boxes it and never moves the box's
/// contents, so the listener may name its address for the registration's whole
/// life.
pub(crate) struct OwnedHandle<T> {
    /// The runtime the handle was created against, read once at creation to
    /// look up the display-owned owner and link the watch. Holding this clone
    /// does not keep the owner (or any object) alive; it is a handle to the
    /// runtime, not the object being watched.
    pub(crate) runtime: Runtime,
    /// The live object, until `alive` is cleared.
    pub(crate) raw: NonNull<T>,
    /// False once the owner has been destroyed (display teardown) or the
    /// handle's own `Drop` has run. Every accessor and mutator is a miss while
    /// it is false.
    pub(crate) alive: Cell<bool>,
    /// The owner's `destroy` watch, unlinked by its own callback or by the
    /// handle's `Drop`.
    pub(crate) watched_destroy: RefCell<Option<Registration>>,
}

impl<T> OwnedHandle<T> {
    /// Box a fresh context for `raw`, with the watch not yet linked.
    ///
    /// The box address is stable from here on, so the caller may take
    /// [`session`](Self::session) and link the watch against it immediately.
    pub(crate) fn boxed(runtime: Runtime, raw: NonNull<T>) -> Box<Self> {
        Box::new(OwnedHandle {
            runtime,
            raw,
            alive: Cell::new(true),
            watched_destroy: RefCell::new(None),
        })
    }

    /// The stable session pointer a watch names for this context.
    ///
    /// May only be taken once the context is boxed (addresses before that are
    /// not stable); the box is never moved afterwards.
    ///
    /// Takes `&Box<Self>` rather than `&Self` deliberately: only a boxed
    /// context has a stable address, and the type proves it at the call site.
    #[allow(clippy::borrowed_box)]
    pub(crate) fn session(boxed: &Box<Self>) -> *const () {
        (boxed.as_ref() as *const Self).cast()
    }

    /// Whether the handle is still live.
    pub(crate) fn is_alive(&self) -> bool {
        self.alive.get()
    }

    /// The live wlroots object, or `None` once this handle is inert.
    pub(crate) fn live_raw(&self) -> Option<NonNull<T>> {
        self.is_alive().then_some(self.raw)
    }

    /// Unlink the owner watch for `Drop` and return the object to destroy.
    ///
    /// `None` when the owner died first (its callback already unlinked the
    /// watch): the caller must not touch the wlroots object. `Some` preserves
    /// the `Drop` ordering exactly — `take` the registration, drop it, *then*
    /// mark dead — so the wlroots `destroy` that follows runs with no watch
    /// linked and no second destroy possible.
    pub(crate) fn take_live_raw(&self) -> Option<NonNull<T>> {
        if !self.alive.get() {
            return None;
        }
        let watch = self.watched_destroy.borrow_mut().take();
        drop(watch);
        self.alive.set(false);
        Some(self.raw)
    }
}

/// Recover the [`OwnedHandle`] a watch was linked with.
///
/// # Safety
///
/// `l` must be a listener linked with an `OwnedHandle<T>` address as its
/// session, and that box must still be alive.
pub(crate) unsafe fn ctx_of<'a, T>(l: *mut sys::wl_listener) -> Option<&'a OwnedHandle<T>> {
    // SAFETY: the caller guarantees `l` is a `Registration` listener, so
    // `bound_session` recovers its live `session`.
    let session = unsafe { bound_session(l) };
    if session.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees the session names a live `OwnedHandle`.
    Some(unsafe { &*session.cast::<OwnedHandle<T>>() })
}

/// The watched owner is being destroyed (display teardown, before the owner
/// itself is freed).
///
/// Marks the owned handle inert and unlinks the watch while the handle memory
/// is still valid, so a later `Drop` never touches the freed owner. The
/// callback ordering — mark dead, unlink the firing listener, drop the stored
/// registration — is the reverse image of
/// [`take_live_raw`](OwnedHandle::take_live_raw) and must stay that way: the
/// registration is dropped from inside its own signal's emission here.
pub(crate) unsafe extern "C" fn on_watched_destroy<T>(
    l: *mut sys::wl_listener,
    _data: *mut c_void,
) {
    // SAFETY: linked with an `OwnedHandle<T>` session and its `alive` cell;
    // the registration below is dropped from inside this emission, while its
    // own owner's signal is still alive.
    unsafe {
        let Some(ctx) = ctx_of::<T>(l) else {
            // Observability only: an unresolvable watch context means the
            // listener outlived its box (or was never linked with one), which
            // must never happen — but the release path stays a silent return,
            // since the use-after-free chain this would imply is unproven and
            // trapping the compositor over it is not warranted.
            debug_assert!(false, "unresolvable watch context for listener {l:p}");
            return;
        };
        ctx.alive.set(false);
        remove_listener(l);
        let registration = ctx.watched_destroy.borrow_mut().take();
        drop(registration);
    }
}

/// An id that names no heap-address-keyed object, for negative tests.
///
/// One shared policy for every `usize` id whose live values are heap (or
/// listener) addresses: `usize::MAX - n` can never be an address handed out
/// here, since heap addresses never sit at the top of the address space.
/// Identical wording on every user so the policy reads as one, not one copy
/// per type.
///
/// This is deliberately *not* the [`dangling_test_id`](crate::id::dangling_test_id)
/// band: that band serves `u64` ids drawn from the process-wide monotonic
/// counter, where an unclamped subtraction could in principle collide with a
/// live value for a very large `n`. Address-keyed ids cannot collide that way
/// — no allocation lives at `usize::MAX` — so no banding is needed.
///
/// For new internal-only users: call this helper rather than spelling out
/// `usize::MAX - n` again. The identical inline bodies on the
/// `#[doc(hidden)] pub dangling_nth_for_test` constructors in `runtime.rs`
/// (and the `usize::MAX` singletons like `CursorId`'s, which are that shape's
/// `n = 0`) predate this helper and are frozen public API within the 0.20.x
/// line, so they are intentionally left as-is; the `usize::MAX - 7` argument
/// at the `forget_transient_seat` call site and the `mapped_len` bound probe
/// in `buffer.rs` are uses of the same guarantee, not new spellings of it.
pub(crate) fn dangling_usize_test_id(n: usize) -> usize {
    usize::MAX - n
}
