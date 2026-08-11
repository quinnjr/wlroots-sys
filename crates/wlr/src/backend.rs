//! Backend creation and the wiring from wlroots signals to handler traits.
//!
//! This is where C calls back into Rust. Three rules govern everything below and
//! are worth stating once rather than repeating at every call site:
//!
//! 1. A `wl_listener` is linked into an intrusive list owned by the signal. The
//!    memory holding its `link` must not be freed while it is still linked, so
//!    every registration is an RAII guard ([`Registration`]) that unlinks in
//!    `Drop` — not an unlink statement at the end of a function, which the `?`
//!    on a fallible dispatch call would skip.
//! 2. The converse also bites, and wlroots documents the case that causes it:
//!    `wlr_backend_autocreate` returns a multi-backend that "will be destroyed
//!    if one of the primary underlying backends is destroyed (e.g. if the
//!    primary DRM device is unplugged)". So the signals themselves can be freed
//!    *first*, mid-dispatch, while a `Registration` still holds a link into
//!    them — at which point unlinking is the use-after-free. [`Context`] carries
//!    the liveness flag that decides between the two.
//! 3. Nothing in a callback may unwind. A panic escaping an `extern "C"` fn has
//!    aborted the process since Rust 1.81, so the code reached from one avoids
//!    panicking paths where the condition is recoverable; see [`ensure_id`].

use std::cell::Cell;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::dispatch::{Dispatcher, Event};
use crate::id::{attach_id, find_id};
use crate::{Display, Error, EventLoop, OutputHandler, OutputId, Result, sys};

/// A wlroots backend.
///
/// Deliberately has no `Drop`. `wlr_backend_destroy` is, in wlroots' own words,
/// "normally called automatically when the event loop is destroyed", and
/// `wlr_backend_autocreate` registers the backend with the loop for exactly
/// that. The loop belongs to the [`Display`], so `wl_display_destroy` in
/// `Display`'s `Drop` is what tears the backend down; calling
/// `wlr_backend_destroy` here as well would be a double free. The ordering is
/// guaranteed by the type system rather than by discipline: `'d` comes from the
/// borrowed [`EventLoop`], which itself borrows the `Display`, so a live
/// `Backend` keeps the display borrowed and `Display::drop` cannot run first.
pub struct Backend<'d> {
    raw: NonNull<sys::wlr_backend>,
    _loop: PhantomData<&'d ()>,
}

/// State shared by every listener registered against one backend.
///
/// Lives as a local of [`Backend::run`], reached from callbacks only through a
/// raw pointer, and outlives every [`Registration`] that points at it.
struct Context<S> {
    dispatcher: *const Dispatcher<S>,

    /// Whether the backend's signals are still valid to touch.
    ///
    /// Starts true and is cleared, permanently, by [`on_backend_destroy`]. It
    /// exists because a multi-backend can free itself during dispatch when a
    /// primary underlying backend goes away (an unplugged DRM device is the
    /// documented example), taking `events.new_output` and `events.destroy`
    /// with it. After that point the listeners are still *in* those lists as
    /// far as their own `link` fields are concerned, but the neighbours those
    /// fields name are freed, so `wl_list_remove` would write through dangling
    /// pointers. `Registration::drop` reads this to decide whether unlinking
    /// is the right thing or the fatal thing.
    signals_alive: Cell<bool>,
}

/// A listener plus the context needed to route its event.
///
/// `#[repr(C)]` with `listener` first so the C callback can recover this struct
/// from the `*mut wl_listener` wlroots passes it.
#[repr(C)]
struct Bound<S> {
    listener: sys::wl_listener,
    context: *const Context<S>,
}

// `bound_of`'s cast is sound only while `listener` is `Bound`'s first field, at
// offset 0. `#[repr(C)]` makes that offset independent of `S`, so checking one
// instantiation checks them all. This fails to compile if the field order ever
// changes.
const _: () = assert!(std::mem::offset_of!(Bound<()>, listener) == 0);

/// A [`Bound`] currently linked into a signal, unlinked when it drops — unless
/// the signal died first.
///
/// The box is what keeps the listener's address stable: a `wl_list` is
/// intrusive, so the signal stores a pointer *into* this allocation and moving
/// the `Registration` (which moves only the `Box`, not its contents) must not
/// disturb it.
struct Registration<S> {
    bound: Box<Bound<S>>,
}

impl<S> Registration<S> {
    /// Link a fresh listener for `notify` into `signal`.
    ///
    /// # Safety
    ///
    /// * `signal` must point at an initialised `wl_signal` belonging to an
    ///   object that either outlives the returned `Registration`, or clears
    ///   `context`'s `signals_alive` flag before freeing itself.
    /// * `context` must outlive the returned `Registration`, and its
    ///   `dispatcher` must satisfy `Dispatcher::emit`'s contract for every call
    ///   `notify` makes through it.
    /// * `notify` must be prepared to recover a `Bound<S>` from the listener it
    ///   is handed, which is what [`bound_of`] does.
    unsafe fn link(
        signal: *mut sys::wl_signal,
        notify: sys::wl_notify_func_t,
        context: *const Context<S>,
    ) -> Self {
        let mut bound = Box::new(Bound {
            listener: sys::wl_listener {
                // `wl_signal_add` overwrites both ends via `wl_list_insert`, so
                // these nulls are never read.
                link: sys::wl_list {
                    prev: std::ptr::null_mut(),
                    next: std::ptr::null_mut(),
                },
                notify,
            },
            context,
        });

        // SAFETY: the caller guarantees `signal` is an initialised `wl_signal`,
        // and the listener is a freshly boxed one that nothing else has linked
        // anywhere. Its address stays put until this `Registration` drops,
        // which unlinks it before the box is freed.
        unsafe { sys::wl_signal_add(signal, &raw mut bound.listener) };

        Registration { bound }
    }
}

impl<S> Drop for Registration<S> {
    fn drop(&mut self) {
        // SAFETY: `link` required `context` to outlive this `Registration`, so
        // it is live here. Only a shared read of a `Cell<bool>`, and every
        // access to it is on the event loop's single thread.
        let signals_alive = unsafe { (*self.bound.context).signals_alive.get() };

        if !signals_alive {
            // The object owning the signal was destroyed while this listener
            // was linked into it, so the neighbours this listener names are
            // freed and there is nothing valid to unlink from. Dropping the box
            // without unlinking is correct and complete: the list head died
            // with its owner, so no one can walk back into this allocation.
            return;
        }

        // SAFETY: `link` put this listener into a signal's list and nothing
        // else ever unlinks it, so it is still linked; and the flag checked
        // above says the signal is still alive, so the neighbours
        // `wl_list_remove` writes through are valid. This runs before the box
        // is freed, which is the whole reason the unlink lives in `Drop`
        // rather than at the end of `run`: an early `?` return there would
        // free a still-linked listener.
        unsafe { remove_listener(&raw mut self.bound.listener) };
    }
}

impl<'d> Backend<'d> {
    /// Create whichever backend suits the environment.
    pub fn autocreate(loop_: &EventLoop<'d>) -> Result<Self> {
        // SAFETY: the borrow guarantees the loop is live. Null for
        // `session_ptr` only suppresses the out-parameter: wlroots still
        // creates and owns a session when the chosen backend needs one, so the
        // DRM and libinput backends are unaffected and nothing is leaked or
        // left un-torn-down. The only consequence is that this crate holds no
        // session handle, which it has no API to use yet.
        let raw = unsafe { sys::wlr_backend_autocreate(loop_.as_ptr(), std::ptr::null_mut()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_backend_autocreate"))?;
        Ok(Backend {
            raw,
            _loop: PhantomData,
        })
    }

    /// Start the backend. New outputs are announced after this returns.
    pub fn start(&self) -> Result<()> {
        // SAFETY: `raw` is `NonNull`, is only ever set by `autocreate` from a
        // successful `wlr_backend_autocreate`, and is never reassigned; and
        // `'d` keeps the display — and therefore the event loop that owns the
        // backend's teardown — alive for at least as long as `self`.
        if unsafe { sys::wlr_backend_start(self.raw.as_ptr()) } {
            Ok(())
        } else {
            Err(Error::Operation("wlr_backend_start"))
        }
    }

    /// Wire up handlers and dispatch `iterations` turns of the event loop.
    ///
    /// Takes a count rather than blocking forever so tests terminate; a
    /// blocking loop belongs with signal handling in a later slice.
    pub fn run<S: OutputHandler>(
        &self,
        display: &Display,
        state: &mut S,
        iterations: u32,
    ) -> Result<()> {
        // `state` is consumed into a raw pointer here and never touched as a
        // reference again for the rest of this function, so no `&mut S` is live
        // while a callback delivers through the dispatcher.
        let dispatcher = Dispatcher::new(&raw mut *state);

        let context = Context {
            dispatcher: &raw const dispatcher,
            signals_alive: Cell::new(true),
        };

        // Both registrations are declared after `context` and `dispatcher`, so
        // they drop — and therefore decide about unlinking — while both are
        // still alive. They are bound to named `_`-prefixed locals rather than
        // to `_`, which would drop them at the end of their own statement and
        // unregister before the loop ran.
        //
        // The destroy listener is registered first so it is in place before any
        // dispatch can occur. It does not need to unlink anything itself: it
        // only clears the flag, and each `Registration` consults that flag when
        // it drops. That is what keeps this from reintroducing the same problem
        // one level up — the destroy listener is an ordinary `Registration`
        // linked into a signal in the very struct being freed, so it is subject
        // to the identical hazard and is protected by the identical check.
        //
        // SAFETY: `self` is live for this call, so `events.destroy` and
        // `events.new_output` are initialised signals. Neither is required to
        // outlive the registrations, because `on_backend_destroy` clears
        // `signals_alive` before wlroots frees them. `context` is a local
        // declared above, so it outlives both registrations, and the dispatcher
        // it names is a local declared above that. `S` never learns of the
        // dispatcher, so nothing can hold a reference to it across `emit` — the
        // aliasing condition `Dispatcher::emit` requires. Both notify functions
        // are written against `Bound<S>`, which is what `link` builds.
        let (_destroy, _new_output) = unsafe {
            let destroy = Registration::link(
                &raw mut (*self.raw.as_ptr()).events.destroy,
                on_backend_destroy::<S>,
                &raw const context,
            );
            let new_output = Registration::link(
                &raw mut (*self.raw.as_ptr()).events.new_output,
                on_new_output::<S>,
                &raw const context,
            );
            (destroy, new_output)
        };

        let loop_ = display.event_loop();
        for _ in 0..iterations {
            loop_.dispatch(0)?;
        }

        Ok(())
    }
}

/// # Safety
///
/// `listener` must be a listener currently linked into a signal's list, and the
/// list's other members must still be live.
unsafe fn remove_listener(listener: *mut sys::wl_listener) {
    use sys::wayland_sys::ffi_dispatch;
    // `wl_list_remove` is not re-exported by wlr-sys; it lives in wayland-sys
    // and must go through `ffi_dispatch!` so this works whether or not
    // wayland-sys was built with its `dlopen` feature.
    //
    // `allow`, not `expect`: this glob is unused only under the `dlopen`
    // expansion of `ffi_dispatch!`, which calls through a function-pointer
    // table instead of the bare name this import brings into scope. See the
    // identical comment on `Display::new` for the full explanation.
    #[allow(unused_imports)]
    use sys::wayland_sys::server::*;

    // SAFETY: caller guarantees the listener is linked and its neighbours live.
    unsafe {
        ffi_dispatch!(
            sys::wayland_sys::server::wayland_server_handle(),
            wl_list_remove,
            &raw mut (*listener).link
        );
    }
}

/// # Safety
///
/// `l` must be the `listener` field of a live `Bound<S>`, with the same `S`.
unsafe fn bound_of<S>(l: *mut sys::wl_listener) -> *mut Bound<S> {
    // `Bound<S>` is `#[repr(C)]` with `listener` first, so the offset is zero
    // and this is the `container_of` pattern with nothing to subtract. The
    // `const _` assertion next to the struct pins the field order.
    l.cast::<Bound<S>>()
}

/// Give the object owning `set` an identity, reusing one already attached.
///
/// Unlike [`attach_id`] this is idempotent, and that is deliberate rather than
/// incidental. `attach_id` asserts that no id addon is present; this function's
/// only callers are `extern "C"` callbacks, where a panic aborts the process
/// (defined behaviour since Rust 1.81, not UB — but an abort out of a
/// compositor's event loop all the same). Reusing the existing id is also the
/// semantically correct answer: an id is meant to be stable for the object's
/// whole life, so "already has one" is a satisfied postcondition, not a fault.
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live
/// object.
unsafe fn ensure_id(set: *mut sys::wlr_addon_set) -> OutputId {
    // SAFETY: the caller's guarantee is exactly what both calls below require.
    // `attach_id`'s additional precondition — that no id addon is attached yet
    // — is discharged by the `find_id` check immediately preceding it, and
    // nothing can attach one in between: wlroots' event loop, and therefore
    // every caller of this function, is single-threaded.
    unsafe {
        match find_id(set.cast_const()) {
            Some(id) => OutputId(id),
            None => OutputId(attach_id(set)),
        }
    }
}

/// The backend is about to free itself, and its signals with it.
unsafe extern "C" fn on_backend_destroy<S>(l: *mut sys::wl_listener, _data: *mut std::ffi::c_void) {
    // SAFETY: wlroots invokes this only for the listener `Backend::run` linked
    // into `events.destroy`, which is the `listener` field of a live
    // `Bound<S>` whose `context` outlives it.
    //
    // Nothing here can unwind: a `Cell<bool>` write has no failure mode and
    // calls no user code, which matters because this is an `extern "C"` frame.
    unsafe {
        let bound = bound_of::<S>(l);
        (*(*bound).context).signals_alive.set(false);
    }
}

unsafe extern "C" fn on_new_output<S: OutputHandler>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener `Backend::run` linked
    // into `events.new_output`, which is the `listener` field of a live
    // `Bound<S>` whose `context`, and the dispatcher it names, are valid for as
    // long as that registration exists. The `new_output` signal carries a
    // `*mut wlr_output`, so the cast of `data` matches what wlroots documents
    // it to pass.
    unsafe {
        let bound = bound_of::<S>(l);
        let output = data.cast::<sys::wlr_output>();

        // Give the output an identity before anyone can ask for one.
        let id = ensure_id(&raw mut (*output).addons);

        (*(*(*bound).context).dispatcher).emit(Event::NewOutput(id), deliver::<S>);
    }
}

/// Route an event to the matching handler method.
fn deliver<S: OutputHandler>(state: &mut S, ev: Event) {
    match ev {
        Event::NewOutput(id) | Event::OutputFrame(id) => {
            // Incomplete on purpose. Re-resolving from an id is what makes
            // deferral safe — an object destroyed between queueing and
            // delivery simply is not found — so these arms wait on the
            // id-to-output registry that the output-registration slice adds.
            // Until then a `NewOutput`/`OutputFrame` event is accepted and
            // dropped rather than delivered.
            let _ = id;
        }
        Event::OutputDestroyed(id) => state.destroyed(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listener that is never emitted to in these tests; they exercise
    /// linking and unlinking, not delivery.
    unsafe extern "C" fn noop(_l: *mut sys::wl_listener, _data: *mut std::ffi::c_void) {}

    /// A bare `wl_signal` on the stack, with a `Context` pointing at no
    /// dispatcher. Neither a display, a backend, nor an id addon is involved,
    /// so these tests cannot perturb `id.rs`'s `DESTROY_COUNT`.
    struct Harness {
        signal: sys::wl_signal,
        context: Context<()>,
    }

    impl Harness {
        /// Boxed, and initialised only *after* boxing, for the same reason
        /// `Bound` is boxed: `wl_signal_init` makes the list head point at
        /// itself, so moving the signal afterwards would leave those
        /// self-pointers naming the old address.
        fn new() -> Box<Self> {
            let mut h = Box::new(Harness {
                signal: sys::wl_signal {
                    listener_list: sys::wl_list {
                        prev: std::ptr::null_mut(),
                        next: std::ptr::null_mut(),
                    },
                },
                context: Context {
                    // Never dereferenced: `noop` is the only notify these tests
                    // register, and nothing emits to the signal.
                    dispatcher: std::ptr::null(),
                    signals_alive: Cell::new(true),
                },
            });
            // SAFETY: `h.signal` is live and exclusively owned, `wl_signal_init`
            // only writes the two `wl_list` fields it owns, and the box's
            // contents do not move again for the rest of the harness's life.
            unsafe { sys::wl_signal_init(&raw mut h.signal) };
            h
        }

        /// Whether a listener for `noop` is currently linked into the signal.
        fn is_linked(&mut self) -> bool {
            // SAFETY: `signal` was initialised in `new` and every listener ever
            // linked into it by these tests is still alive at each call site,
            // so walking the list is sound.
            !unsafe { sys::wl_signal_get(&raw mut self.signal, noop) }.is_null()
        }
    }

    /// The guard's whole purpose: a `Registration` is linked for exactly its
    /// own lifetime, and leaves the signal a valid, walkable, empty list.
    #[test]
    fn a_registration_links_on_construction_and_unlinks_on_drop() {
        let mut h = Harness::new();
        assert!(!h.is_linked(), "a fresh signal has no listeners");

        // SAFETY: `h.signal` is initialised and outlives the registration, and
        // `h.context` outlives it too. `noop` never touches the `Bound`.
        let reg = unsafe { Registration::link(&raw mut h.signal, noop, &raw const h.context) };
        assert!(h.is_linked(), "link must register the listener");

        drop(reg);
        assert!(
            !h.is_linked(),
            "drop must unlink, and leave a walkable empty list rather than a dangling one"
        );
    }

    /// The bug that motivated making this a guard at all. `run` unlinks nothing
    /// explicitly; a fallible dispatch call returning `Err` through `?` used to
    /// skip the trailing unlink and free a still-linked listener. The next
    /// slice's integration test drives only the happy path, so nothing else
    /// covers this.
    #[test]
    fn an_early_return_still_unlinks() {
        let mut h = Harness::new();

        /// Stands in for `loop_.dispatch(0)` failing mid-loop.
        fn failing_dispatch() -> Result<()> {
            Err(Error::Operation("simulated dispatch failure"))
        }

        fn link_then_fail(signal: *mut sys::wl_signal, context: *const Context<()>) -> Result<()> {
            // SAFETY: the caller owns a live signal and context that both
            // outlive this call.
            let _reg = unsafe { Registration::link(signal, noop, context) };
            failing_dispatch()?;
            Ok(())
        }

        let err = link_then_fail(&raw mut h.signal, &raw const h.context);
        assert!(err.is_err(), "the harness function must take the `?` path");
        assert!(
            !h.is_linked(),
            "the guard must unlink on the early-return path, not just the fall-through one"
        );
    }

    /// The converse hazard: a multi-backend can free itself mid-dispatch, at
    /// which point unlinking is the use-after-free. Clearing `signals_alive`
    /// must suppress the unlink entirely.
    ///
    /// Asserted by comparing the list head's `next` pointer before and after
    /// the drop. Nothing dereferences it — after a real backend death it would
    /// be dangling, which is exactly the point.
    #[test]
    fn a_dead_signal_is_not_unlinked_from() {
        let mut h = Harness::new();

        // SAFETY: as in the test above.
        let reg = unsafe { Registration::link(&raw mut h.signal, noop, &raw const h.context) };
        let linked_next = h.signal.listener_list.next;
        assert_ne!(
            linked_next.cast_const(),
            &raw const h.signal.listener_list,
            "the list is non-empty while the registration is linked"
        );

        // Stands in for `on_backend_destroy` having run.
        h.context.signals_alive.set(false);
        drop(reg);

        assert_eq!(
            h.signal.listener_list.next, linked_next,
            "dropping a registration whose signal is already dead must not touch the list"
        );
    }
}
