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
//!    primary DRM device is unplugged)". So the whole `wlr_backend` can be freed
//!    *first*, mid-dispatch, while a `Registration` still holds a link into it —
//!    at which point unlinking is the use-after-free.
//!
//!    [`Backend::alive`] is the one fact that resolves this, and it is a field
//!    of the `Backend` rather than of any one call, because the backend dying is
//!    permanent: once it is clear, `self.raw` is dangling forever, so *every*
//!    later entry point has to refuse. `Backend` therefore watches
//!    `events.destroy` from the moment it is created — not merely while a
//!    handler loop is running, since [`crate::EventLoop::dispatch`] is public
//!    and a consumer can drive the loop (and so kill the backend) without ever
//!    calling [`Backend::run`].
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
/// Has no `Drop` *impl*, deliberately. `wlr_backend_destroy` is, in wlroots' own
/// words, "normally called automatically when the event loop is destroyed", and
/// `wlr_backend_autocreate` registers the backend with the loop for exactly
/// that. The loop belongs to the [`Display`], so `wl_display_destroy` in
/// `Display`'s `Drop` is what tears the backend down; calling
/// `wlr_backend_destroy` here as well would be a double free. That ordering is
/// guaranteed by the type system rather than by discipline: `'d` comes from the
/// borrowed [`EventLoop`], which itself borrows the `Display`, so a live
/// `Backend` keeps the display borrowed and `Display::drop` cannot run first.
///
/// It does have drop glue, from the destroy watch below.
pub struct Backend<'d> {
    raw: NonNull<sys::wlr_backend>,

    /// Listens on `events.destroy` for the backend's whole life, and does
    /// nothing but clear `alive`.
    ///
    /// Declared before `alive` because struct fields drop in declaration order
    /// and this one's `Drop` reads the flag.
    _death_watch: Registration,

    /// Whether `raw` still points at a live `wlr_backend`.
    ///
    /// Boxed so the flag has an address that survives this struct being moved —
    /// `autocreate` returns a `Backend` by value, and `_death_watch` holds a
    /// pointer to the flag. Same reason [`Bound`] is boxed.
    ///
    /// Cleared, permanently, by [`on_backend_destroy`]. wlroots emits
    /// `events.destroy` from `wlr_backend_finish` *before* freeing anything, so
    /// the flag is always brought down while the struct is still valid — there
    /// is no window in which it reads `true` for a freed backend.
    alive: Box<Cell<bool>>,

    _loop: PhantomData<&'d ()>,
}

/// A listener plus the context its callback needs.
///
/// `#[repr(C)]` with `listener` first so the C callback can recover this struct
/// from the `*mut wl_listener` wlroots passes it.
///
/// Deliberately *not* generic over the handler state `S`. The destroy watch is
/// created by [`Backend::autocreate`], which has no `S` to speak of, and making
/// this type generic would force a second listener type and a second
/// `Registration` to go with it. Instead `dispatcher` is type-erased and only
/// the notify function that installed it — which does know `S` — casts it back.
#[repr(C)]
struct Bound {
    listener: sys::wl_listener,

    /// An erased `*const Dispatcher<S>`, or null for listeners that route no
    /// events (the destroy watch).
    ///
    /// The obligation this creates is the same one [`bound_of`] already carried:
    /// a notify function must be paired with the `S` its `Bound` was built for.
    /// It is discharged in exactly two places — `Backend::autocreate` pairs null
    /// with [`on_backend_destroy`], which never reads it, and `Backend::run`
    /// pairs a `*const Dispatcher<S>` with `on_new_output::<S>` for the same
    /// `S`.
    dispatcher: *const (),

    /// The owning [`Backend`]'s liveness flag; see [`Registration::drop`].
    alive: *const Cell<bool>,
}

// `bound_of`'s cast is sound only while `listener` is `Bound`'s first field, at
// offset 0. This fails to compile if the field order ever changes.
const _: () = assert!(std::mem::offset_of!(Bound, listener) == 0);

/// A [`Bound`] currently linked into a signal, unlinked when it drops — unless
/// the backend owning the signal died first.
///
/// The box is what keeps the listener's address stable: a `wl_list` is
/// intrusive, so the signal stores a pointer *into* this allocation and moving
/// the `Registration` (which moves only the `Box`, not its contents) must not
/// disturb it.
struct Registration {
    bound: Box<Bound>,
}

impl Registration {
    /// Link a fresh listener for `notify` into `signal`.
    ///
    /// # Safety
    ///
    /// * `signal` must point at an initialised `wl_signal` owned by the backend
    ///   whose liveness `alive` tracks, and that backend must either outlive the
    ///   returned `Registration` or clear `alive` before freeing itself.
    /// * `alive` must outlive the returned `Registration`.
    /// * `dispatcher` must be either null, or a `*const Dispatcher<S>` for the
    ///   same `S` that `notify` casts it back to, valid for as long as the
    ///   returned `Registration` lives and satisfying `Dispatcher::emit`'s
    ///   contract for every call `notify` makes through it.
    /// * `notify` must be prepared to recover a `Bound` from the listener it is
    ///   handed, which is what [`bound_of`] does.
    unsafe fn link(
        signal: *mut sys::wl_signal,
        notify: sys::wl_notify_func_t,
        dispatcher: *const (),
        alive: *const Cell<bool>,
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
            dispatcher,
            alive,
        });

        // SAFETY: the caller guarantees `signal` is an initialised `wl_signal`,
        // and the listener is a freshly boxed one that nothing else has linked
        // anywhere. Its address stays put until this `Registration` drops,
        // which unlinks it before the box is freed.
        unsafe { sys::wl_signal_add(signal, &raw mut bound.listener) };

        Registration { bound }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: `link` required `alive` to outlive this `Registration`, so it
        // is live here. Only a shared read of a `Cell<bool>`, and every access
        // to it is on the event loop's single thread.
        let alive = unsafe { (*self.bound.alive).get() };

        if !alive {
            // The backend owning the signal was destroyed while this listener
            // was linked into it, so the neighbours this listener names are
            // freed and there is nothing valid to unlink from. Dropping the box
            // without unlinking is correct *and* complete: the list head died
            // with its owner, so nothing can walk back into this allocation.
            return;
        }

        // SAFETY: `link` put this listener into a signal's list and nothing
        // else ever unlinks it, so it is still linked; and the flag checked
        // above says the backend that owns the signal is still alive, so the
        // neighbours `wl_list_remove` writes through are valid. This runs
        // before the box is freed, which is the whole reason the unlink lives
        // in `Drop` rather than at the end of `run`: an early `?` return there
        // would free a still-linked listener.
        unsafe { remove_listener(&raw mut self.bound.listener) };
    }
}

/// The liveness gate every entry point that touches `raw` passes through.
///
/// A free function rather than a method so it is unit-testable without a real
/// backend — constructing a [`Backend`] needs wlroots and a display, which
/// would put the one branch that decides whether a dangling pointer gets
/// dereferenced beyond the reach of a cheap test.
fn alive_or_err(alive: &Cell<bool>) -> Result<()> {
    if alive.get() {
        Ok(())
    } else {
        Err(Error::Destroyed("wlr_backend"))
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

        let alive = Box::new(Cell::new(true));

        // Registered here, not in `run`, so the flag is trustworthy for the
        // backend's whole life rather than only while a handler loop happens to
        // be running. A consumer can drive `EventLoop::dispatch` directly, and
        // the DRM unplug that kills a multi-backend does not wait for `run`.
        //
        // SAFETY: `wlr_backend_autocreate` returned non-null, so `events.destroy`
        // is an initialised signal on a live backend. The backend is not
        // required to outlive this registration, because `on_backend_destroy`
        // clears `alive` before wlroots frees it. `alive` is boxed, so moving
        // the `Backend` returned below does not move the flag, and the
        // `_death_watch` field is declared before `alive` so it drops — and so
        // reads the flag — while the flag is still there. `on_backend_destroy`
        // never reads `dispatcher`, so null is the honest value for it.
        let death_watch = unsafe {
            Registration::link(
                &raw mut (*raw.as_ptr()).events.destroy,
                on_backend_destroy,
                std::ptr::null(),
                &raw const *alive,
            )
        };

        Ok(Backend {
            raw,
            _death_watch: death_watch,
            alive,
            _loop: PhantomData,
        })
    }

    /// Start the backend. New outputs are announced after this returns.
    ///
    /// # Errors
    ///
    /// [`Error::Destroyed`] if the backend has already destroyed itself — see
    /// [`Backend::run`].
    pub fn start(&self) -> Result<()> {
        alive_or_err(&self.alive)?;

        // SAFETY: `raw` is `NonNull`, is only ever set by `autocreate` from a
        // successful `wlr_backend_autocreate`, and is never reassigned — so the
        // only question is whether the backend has since been freed, and the
        // check above is what answers it. wlroots emits `events.destroy` before
        // freeing, `autocreate` has been listening on it since the backend
        // existed, and the flag is cleared and read on the same single thread,
        // so nothing can free the backend between that check and this call.
        //
        // Note the display's lifetime is *not* the justification: `'d` bounds
        // when teardown happens at the latest, but a primary DRM device being
        // unplugged frees the backend independently of the display (see rule 2
        // in the module header).
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
    ///
    /// # Errors
    ///
    /// [`Error::Destroyed`] if the backend destroyed itself — either before
    /// this call, or during one of the dispatch turns. wlroots' multi-backend
    /// does that when a primary underlying backend goes away, so it is an
    /// ordinary hardware event rather than a programming error. Once it
    /// happens, every later call on this `Backend` fails the same way; the
    /// value is inert and should be dropped.
    pub fn run<S: OutputHandler>(
        &self,
        display: &Display,
        state: &mut S,
        iterations: u32,
    ) -> Result<()> {
        alive_or_err(&self.alive)?;

        // `state` is consumed into a raw pointer here and never touched as a
        // reference again for the rest of this function, so no `&mut S` is live
        // while a callback delivers through the dispatcher.
        let dispatcher = Dispatcher::new(&raw mut *state);

        // Declared after `dispatcher`, so it drops — and therefore decides
        // about unlinking — while the dispatcher it names is still alive. Bound
        // to a named `_new_output` rather than to `_`, which would drop it at
        // the end of its own statement and unregister before the loop ran.
        //
        // SAFETY: the check above establishes the backend is live, so
        // `events.new_output` is an initialised signal; and it is not required
        // to outlive this registration, because the destroy watch installed in
        // `autocreate` clears `self.alive` before wlroots frees it. That flag
        // lives in a box owned by `self`, which outlives this call. `S` never
        // learns of the dispatcher, so nothing can hold a reference to it
        // across `emit` — the aliasing condition `Dispatcher::emit` requires —
        // and `on_new_output::<S>` casts the erased pointer back to the very
        // `Dispatcher<S>` paired with it here.
        let _new_output = unsafe {
            Registration::link(
                &raw mut (*self.raw.as_ptr()).events.new_output,
                on_new_output::<S>,
                (&raw const dispatcher).cast::<()>(),
                &raw const *self.alive,
            )
        };

        let loop_ = display.event_loop();
        for _ in 0..iterations {
            loop_.dispatch(0)?;
            // Checked every turn rather than only on entry: a backend that dies
            // mid-loop leaves nothing useful to dispatch, and reporting it is
            // the difference between a consumer learning about an unplugged GPU
            // and silently spinning out the remaining iterations.
            alive_or_err(&self.alive)?;
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
/// `l` must be the `listener` field of a live [`Bound`].
unsafe fn bound_of(l: *mut sys::wl_listener) -> *mut Bound {
    // `Bound` is `#[repr(C)]` with `listener` first, so the offset is zero and
    // this is the `container_of` pattern with nothing to subtract. The `const _`
    // assertion next to the struct pins the field order.
    l.cast::<Bound>()
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
unsafe extern "C" fn on_backend_destroy(l: *mut sys::wl_listener, _data: *mut std::ffi::c_void) {
    // SAFETY: wlroots invokes this only for the listener `Backend::autocreate`
    // linked into `events.destroy`, which is the `listener` field of a live
    // `Bound` whose `alive` flag outlives it.
    //
    // Nothing here can unwind: a `Cell<bool>` write has no failure mode and
    // calls no user code, which matters because this is an `extern "C"` frame.
    // In particular `dispatcher` is never read, so its being null is fine.
    unsafe {
        let bound = bound_of(l);
        (*(*bound).alive).set(false);
    }
}

unsafe extern "C" fn on_new_output<S: OutputHandler>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener `Backend::run` linked
    // into `events.new_output`, which is the `listener` field of a live `Bound`
    // whose `dispatcher` was paired with this very instantiation — same `S` —
    // and is valid for as long as that registration exists. The `new_output`
    // signal carries a `*mut wlr_output`, so the cast of `data` matches what
    // wlroots documents it to pass.
    unsafe {
        let bound = bound_of(l);
        let dispatcher = (*bound).dispatcher.cast::<Dispatcher<S>>();
        let output = data.cast::<sys::wlr_output>();

        // Give the output an identity before anyone can ask for one.
        let id = ensure_id(&raw mut (*output).addons);

        (*dispatcher).emit(Event::NewOutput(id), deliver::<S>);
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

    /// A listener that is never emitted to by most of these tests; they
    /// exercise linking and unlinking, not delivery.
    unsafe extern "C" fn noop(_l: *mut sys::wl_listener, _data: *mut std::ffi::c_void) {}

    /// A bare `wl_signal` plus the liveness flag a real [`Backend`] would own.
    ///
    /// Neither a display, a backend, nor an id addon is involved, so these tests
    /// cannot perturb `id.rs`'s `DESTROY_COUNT`.
    struct Harness {
        signal: sys::wl_signal,
        alive: Cell<bool>,
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
                alive: Cell::new(true),
            });
            // SAFETY: `h.signal` is live and exclusively owned, `wl_signal_init`
            // only writes the two `wl_list` fields it owns, and the box's
            // contents do not move again for the rest of the harness's life.
            unsafe { sys::wl_signal_init(&raw mut h.signal) };
            h
        }
    }

    /// Whether a listener for `notify` is currently linked into `h`'s signal.
    ///
    /// Takes a raw pointer, and every test below reaches its harness only
    /// through the single raw pointer it derives up front. Materialising a
    /// `&Harness` or `&mut Harness` here would, under both Stacked and Tree
    /// Borrows, invalidate the provenance the intrusive list was built under —
    /// and `Registration::drop` later writes through pointers derived from it.
    /// This is the same discipline `dispatch::tests` uses for its state.
    ///
    /// # Safety
    ///
    /// `h` must point at a live `Harness` whose signal is initialised, and every
    /// listener linked into that signal must still be alive.
    unsafe fn is_linked(h: *mut Harness, notify: sys::wl_notify_func_t) -> bool {
        // SAFETY: the caller guarantees the list is walkable.
        unsafe { !sys::wl_signal_get(&raw mut (*h).signal, notify).is_null() }
    }

    /// The guard's whole purpose: a `Registration` is linked for exactly its
    /// own lifetime, and leaves the signal a valid, walkable, empty list.
    #[test]
    fn a_registration_links_on_construction_and_unlinks_on_drop() {
        let mut h = Harness::new();
        let hp = &raw mut *h;

        // SAFETY: `hp` is a live, exclusively-derived pointer to the boxed
        // harness, whose signal and flag both outlive the registration. `noop`
        // never touches the `Bound`, so a null dispatcher is fine.
        unsafe {
            assert!(!is_linked(hp, noop), "a fresh signal has no listeners");

            let reg = Registration::link(
                &raw mut (*hp).signal,
                noop,
                std::ptr::null(),
                &raw const (*hp).alive,
            );
            assert!(is_linked(hp, noop), "link must register the listener");

            drop(reg);
            assert!(
                !is_linked(hp, noop),
                "drop must unlink, and leave a walkable empty list rather than a dangling one"
            );
        }
    }

    /// The bug that motivated making this a guard at all. `run` unlinks nothing
    /// explicitly; a fallible dispatch call returning `Err` through `?` used to
    /// skip the trailing unlink and free a still-linked listener. The next
    /// slice's integration test drives only the happy path, so nothing else
    /// covers this.
    #[test]
    fn an_early_return_still_unlinks() {
        /// Stands in for `loop_.dispatch(0)` failing mid-loop.
        fn failing_dispatch() -> Result<()> {
            Err(Error::Operation("simulated dispatch failure"))
        }

        fn link_then_fail(signal: *mut sys::wl_signal, alive: *const Cell<bool>) -> Result<()> {
            // SAFETY: the caller owns a live signal and flag that both outlive
            // this call.
            let _reg = unsafe { Registration::link(signal, noop, std::ptr::null(), alive) };
            failing_dispatch()?;
            Ok(())
        }

        let mut h = Harness::new();
        let hp = &raw mut *h;

        // SAFETY: as in the test above.
        unsafe {
            let err = link_then_fail(&raw mut (*hp).signal, &raw const (*hp).alive);
            assert!(err.is_err(), "the harness function must take the `?` path");
            assert!(
                !is_linked(hp, noop),
                "the guard must unlink on the early-return path, not just the fall-through one"
            );
        }
    }

    /// The converse hazard: a multi-backend can free itself mid-dispatch, at
    /// which point unlinking is the use-after-free. A clear liveness flag must
    /// suppress the unlink entirely.
    ///
    /// Asserted by comparing the list head's `next` pointer before and after the
    /// drop. Nothing dereferences it — after a real backend death it would be
    /// dangling, which is exactly the point.
    #[test]
    fn a_dead_signal_is_not_unlinked_from() {
        let mut h = Harness::new();
        let hp = &raw mut *h;

        // SAFETY: as in the tests above.
        unsafe {
            let reg = Registration::link(
                &raw mut (*hp).signal,
                noop,
                std::ptr::null(),
                &raw const (*hp).alive,
            );
            let linked_next = (*hp).signal.listener_list.next;
            assert_ne!(
                linked_next.cast_const(),
                &raw const (*hp).signal.listener_list,
                "the list is non-empty while the registration is linked"
            );

            // Stands in for `on_backend_destroy` having run.
            (*hp).alive.set(false);
            drop(reg);

            assert_eq!(
                (*hp).signal.listener_list.next,
                linked_next,
                "dropping a registration whose signal is already dead must not touch the list"
            );
        }
        // `h`'s signal now names a freed listener. Nothing reads it again, and
        // dropping the box only deallocates — exactly the state a real backend
        // death leaves behind.
    }

    /// The destroy watch end to end, against real libwayland: emitting the
    /// signal must run `on_backend_destroy`, which must recover its `Bound` via
    /// `bound_of` and clear the flag the whole design hangs on.
    #[test]
    fn emitting_destroy_clears_the_liveness_flag() {
        let mut h = Harness::new();
        let hp = &raw mut *h;

        // SAFETY: `hp` is a live, exclusively-derived pointer to the boxed
        // harness. The listener linked below stays alive across the emission,
        // and `wl_signal_emit_mutable` tolerates handlers that unlink — this one
        // does not even do that. `on_backend_destroy` ignores its `data`, so
        // null is safe to pass here in a way it would not be for a signal whose
        // handlers dereference it.
        unsafe {
            let reg = Registration::link(
                &raw mut (*hp).signal,
                on_backend_destroy,
                std::ptr::null(),
                &raw const (*hp).alive,
            );
            assert!((*hp).alive.get(), "the flag starts set");

            sys::wl_signal_emit_mutable(&raw mut (*hp).signal, std::ptr::null_mut());

            assert!(
                !(*hp).alive.get(),
                "the destroy watch must clear the flag when wlroots emits destroy"
            );

            // And having done so, the registration must now skip its unlink —
            // the two halves have to agree or the mechanism is worse than
            // useless.
            let linked_next = (*hp).signal.listener_list.next;
            drop(reg);
            assert_eq!(
                (*hp).signal.listener_list.next,
                linked_next,
                "after the flag is cleared, dropping must not touch the dead signal"
            );
        }
    }

    /// A destroyed backend must be refused, and refused *distinguishably*:
    /// naming a C function that was never called would be a lie, and a consumer
    /// cannot tell "retry is pointless" from "the call failed" without this.
    #[test]
    fn the_liveness_gate_reports_destruction_rather_than_a_failed_call() {
        let alive = Cell::new(true);
        assert_eq!(
            alive_or_err(&alive),
            Ok(()),
            "a live backend passes the gate"
        );

        alive.set(false);
        assert_eq!(
            alive_or_err(&alive),
            Err(Error::Destroyed("wlr_backend")),
            "a dead backend is refused before anything dereferences it"
        );
        assert_ne!(
            alive_or_err(&alive),
            Err(Error::Operation("wlr_backend_start")),
            "and is not reported as a C call that failed, since none was made"
        );
    }
}
