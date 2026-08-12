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
//!    panicking paths where the condition is recoverable; see [`ensure_id_raw`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::rc::Rc;

use crate::dispatch::{Dispatcher, Event};
use crate::id::{SourceId, attach_id, find_id};
use crate::seat::{KeyEvent, Modifiers};
use crate::{
    Display, Error, EventLoop, Handlers, LoopHandler, Output, OutputHandler, OutputId, Result,
    Runtime, Toplevel, ToplevelId, sys,
};

/// `wl_seat.capability` bit values from `wayland.xml`. Not bound by
/// `wlr-sys`: `wlr_seat_set_capabilities` takes a bare `u32` (see its own
/// doc), so bindgen never had a reason to generate this enum from the
/// wlroots headers it processes, and the core `wl_seat` protocol enum is not
/// among them either. ABI-stable — it is core Wayland protocol, not
/// wlroots — and checked by `capability_bits_match_the_wayland_protocol`
/// below rather than trusted from this comment alone.
const WL_SEAT_CAPABILITY_POINTER: u32 = 1;
const WL_SEAT_CAPABILITY_KEYBOARD: u32 = 2;

/// A wlroots backend.
///
/// Has no `Drop` *impl*, deliberately. `wlr_backend_destroy` is, in wlroots' own
/// words, "normally called automatically when the event loop is destroyed", and
/// `wlr_backend_autocreate` registers the backend with the loop for exactly
/// that. The loop belongs to the [`Display`](crate::Display), so `wl_display_destroy` in
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
    /// Heap-allocated so the flag has an address that survives this struct being
    /// moved — `autocreate` returns a `Backend` by value, and `_death_watch`
    /// holds a raw pointer to the flag.
    ///
    /// `Rc`, not `Box`, and that is a soundness distinction rather than a
    /// stylistic one. A `Box` asserts unique ownership of its pointee, so moving
    /// the `Backend` re-tags the boxed `Cell` — which, under Stacked Borrows
    /// with `retag-fields`, pops the raw pointer `_death_watch` derived from it
    /// and makes the callback's later write model-level UB. `Rc`'s pointee is
    /// shared and carries no such assertion, so moving the handle leaves
    /// pointers into the allocation valid. `Backend` is already `!Send`/`!Sync`
    /// through its `NonNull` fields, so the non-atomic refcount costs nothing.
    ///
    /// Cleared, permanently, by [`on_backend_destroy`]. wlroots emits
    /// `events.destroy` from `wlr_backend_finish` *before* freeing anything, so
    /// the flag is always brought down while the struct is still valid — there
    /// is no window in which it reads `true` for a freed backend.
    alive: Rc<Cell<bool>>,

    /// Whether `wlr_backend_start` has already been called.
    ///
    /// Not merely bookkeeping: wlroots documents that starting a backend "may
    /// signal new_input or new_output immediately", and the headless backend
    /// does exactly that — it announces every output it already has from inside
    /// `wlr_backend_start`. Starting twice would announce them twice.
    started: Cell<bool>,

    /// The event loop this backend was created on.
    ///
    /// Kept rather than discarded so that [`Backend::run`] can dispatch the
    /// loop the backend's signals actually arrive on. Asking the caller to
    /// re-supply it as a `&Display` was the alternative, and it was
    /// unenforceable: nothing related the two, so passing a second display's
    /// loop compiled, dispatched the wrong loop, and produced silence.
    loop_: NonNull<sys::wl_event_loop>,

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
/// `Registration` to go with it. Instead `session` is type-erased and only
/// the notify function that installed it — which does know `S` — casts it back.
#[repr(C)]
struct Bound {
    listener: sys::wl_listener,

    /// An erased `*const Session<S>`, or null for listeners that route no
    /// events (the destroy watch).
    ///
    /// The obligation this creates is the same one [`bound_of`] already carried:
    /// a notify function must be paired with the `S` its `Bound` was built for.
    /// Every site that creates one pairs the two in a single expression, so the
    /// `S` cannot drift apart from the callback:
    ///
    /// * [`Backend::autocreate`] pairs null with [`on_backend_destroy`], which
    ///   never reads it.
    /// * [`Backend::run`] pairs its `*const Session<S>` with
    ///   `on_new_output::<S>`.
    /// * [`on_new_output`] — already instantiated at one `S` — forwards *its
    ///   own* `session` to `on_frame::<S>` and `on_output_destroy::<S>` at the
    ///   same `S`, so the pairing is inherited rather than re-derived.
    session: *const (),

    /// The liveness flag of whatever owns the signal this listener is linked
    /// into, or null when that owner cannot die first; see
    /// [`Registration::drop`].
    alive: *const Cell<bool>,

    /// The output this listener belongs to, for the per-output listeners; `None`
    /// for the two backend-level ones.
    ///
    /// Carried here rather than looked up from the output's addon set at
    /// callback time so that delivery cannot depend on a lookup that might miss.
    /// A miss in [`on_output_destroy`] would leave the registry holding a
    /// pointer to an output wlroots is about to free — the single failure this
    /// whole design exists to prevent.
    id: Option<OutputId>,

    /// The toplevel this listener belongs to, for the five per-toplevel
    /// listeners `on_new_toplevel` links; `None` for every other listener in
    /// this file.
    ///
    /// This is what makes `on_toplevel_map`, `on_toplevel_unmap`,
    /// `on_toplevel_set_title` and `on_toplevel_destroy` sound at all: wlroots
    /// 0.20 emits `wlr_surface.events.map`/`.unmap` and
    /// `wlr_xdg_toplevel.events.set_title`/`.destroy` with a **null** `data`
    /// argument (confirmed against the C sources — `wlr_compositor.c` and
    /// `wlr_xdg_toplevel.c`), so a callback that read the id out of `data`
    /// would dereference a null pointer on the first real client. `Bound` is
    /// private to this module, so widening it with a second id field costs
    /// nothing outside it — unlike `Bound::id`, which stays `Option<OutputId>`
    /// rather than being generalised, because every output-side call site
    /// still wants that exact type and a shared enum would cost every one of
    /// them a match arm for a case that can never apply to it.
    toplevel: Option<ToplevelId>,
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
    /// * `signal` must point at an initialised `wl_signal` whose owner either
    ///   outlives the returned `Registration` or clears `alive` before freeing
    ///   itself. Passing null for `alive` asserts the former outright.
    /// * `alive`, if non-null, must outlive the returned `Registration`.
    /// * `session` must be either null, or a `*const Session<S>` for the
    ///   same `S` that `notify` casts it back to, valid for as long as the
    ///   returned `Registration` lives and satisfying `Dispatcher::emit`'s
    ///   contract for every call `notify` makes through it.
    /// * `notify` must be prepared to recover a `Bound` from the listener it is
    ///   handed, which is what [`bound_of`] does.
    unsafe fn link(
        signal: *mut sys::wl_signal,
        notify: sys::wl_notify_func_t,
        session: *const (),
        alive: *const Cell<bool>,
        id: Option<OutputId>,
        toplevel: Option<ToplevelId>,
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
            session,
            alive,
            id,
            toplevel,
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
        // A null flag means the signal's owner cannot predecease this
        // registration, so there is nothing to consult. That is how the
        // per-output listeners are registered, and it is a stronger claim than
        // the flag rather than a weaker one: wlroots emits an output's
        // `events.destroy` *before* it frees the output, and
        // [`on_output_destroy`] drops these two registrations from inside that
        // emission — so a per-output registration reaching this point at all
        // proves its output is still alive. Reusing the *backend*'s flag here
        // would be actively wrong: the multi-backend emits its own destroy
        // before tearing its sub-backends (and so their outputs) down, so the
        // flag can already read false while the output is very much alive and
        // still needs unlinking from.
        let owner_alive = self.bound.alive.is_null() || {
            // SAFETY: `link` required a non-null `alive` to outlive this
            // `Registration`, so it is live here. Only a shared read of a
            // `Cell<bool>`, and every access to it is on the event loop's
            // single thread.
            unsafe { (*self.bound.alive).get() }
        };

        if !owner_alive {
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

/// Everything one [`Backend::run`] call owns and its callbacks reach.
///
/// The registry is the point of it. A deferred [`Event`] carries an
/// [`OutputId`] rather than a pointer, because the object may be destroyed
/// between queueing and delivery — so delivery has to turn the id back into a
/// pointer, and that is only sound if a destroyed output cannot still be found
/// here. [`on_output_destroy`] removes the entry synchronously, from inside the
/// emission wlroots performs *before* freeing the output, so a lookup after
/// destruction misses and the event is dropped.
///
/// Owned by `run` rather than by the [`Backend`], and that is forced rather than
/// chosen: the per-output listeners in an entry must call handlers on `S`, and
/// `S` is chosen per `run` call. A registry outliving the call would outlive the
/// `Dispatcher<S>` and the `&mut S` its listeners name, and any output signal
/// firing afterwards — a consumer driving [`crate::EventLoop::dispatch`]
/// themselves is enough — would dereference both. A process-wide or
/// thread-local registry has the same defect and adds another: two backends, or
/// two successive `run` calls, would share one table and hand out each other's
/// outputs.
///
/// The visible cost is that outputs announced during one `run` are not
/// re-announced by the next one; wlroots offers no way to enumerate a backend's
/// existing outputs, so there is nothing to replay from.
struct Session<'r, S> {
    dispatcher: Dispatcher<S>,
    outputs: RefCell<HashMap<OutputId, OutputEntry>>,

    /// This run's listeners on every live toplevel: the id addon is what
    /// resolves an event back to a [`ToplevelId`], and these are what keep
    /// the callbacks that read it linked. Removed, and so unlinked, from
    /// `on_toplevel_destroy` — before the toplevel is freed, mirroring
    /// `outputs` above.
    toplevels: RefCell<HashMap<ToplevelId, ToplevelListeners>>,

    /// This run's listeners on every announced input device, one
    /// [`InputDevice`] per device. Unlike `outputs` and `toplevels` this is
    /// not keyed — nothing in this release needs to name one device's
    /// listeners individually, only to keep every one of them linked for the
    /// run and drop them (and so unlink) when it ends.
    inputs: RefCell<Vec<InputDevice>>,

    /// Whether the most recently delivered [`Event::Key`] was consumed by the
    /// handler. `on_key` cannot get a return value back through
    /// `Dispatcher::emit` directly — an `extern "C"` callback has no way to
    /// receive one — so `deliver_all` records the answer here instead, and
    /// `on_key` reads it back once `emit` returns to decide whether to
    /// forward the key to the client. See `SeatHandler::key`'s own doc for
    /// what "consumed" means to a compositor.
    last_key_consumed: Cell<bool>,

    /// The runtime this run is serving, borrowed for the call.
    ///
    /// Borrowed rather than cloned so that `Session`'s lifetime, and the
    /// registrations it owns, cannot outlive the `&Runtime` the caller passed
    /// — the same argument that keeps the registry itself run-local.
    runtime: &'r Runtime,

    /// How this run routes an [`Event`]. Stored rather than chosen at the
    /// callback, because `run` and `run_all` route differently and the
    /// callbacks are shared.
    deliver: fn(&Session<'r, S>, &mut S, Event),
}

/// One live output, and this session's listeners on it.
///
/// Field order is load-bearing in the same way [`Backend`]'s is: the two
/// registrations must drop — and so unlink from the output's signals — as part
/// of removing the entry, which happens while the output is still alive.
struct OutputEntry {
    raw: *mut sys::wlr_output,
    _frame: Registration,
    _destroy: Registration,
}

/// One live toplevel's listeners: the surface's commit/map/unmap and the
/// toplevel's own set_title/destroy. Field order is not load-bearing here —
/// unlike [`OutputEntry`], nothing here owns the `Bound` any of the others
/// recover their session from — but all five must drop, and so unlink, as
/// part of removing the entry, which happens while the toplevel is still
/// alive.
struct ToplevelListeners {
    _commit: Registration,
    _map: Registration,
    _unmap: Registration,
    _set_title: Registration,
    _destroy: Registration,
}

/// Clears `Runtime`'s toplevel tables when the `run_inner` call holding this
/// guard returns, on every exit path. See `Runtime::clear_toplevels`'s own
/// doc for why a `ToplevelId` must not outlive the `run_all` call that
/// announced it, and `run_inner`'s call site for why this is a `Drop` guard
/// rather than a statement at the end of the function (an early `?` return
/// would skip it).
struct ToplevelTableGuard<'r>(&'r Runtime);

impl Drop for ToplevelTableGuard<'_> {
    fn drop(&mut self) {
        self.0.clear_toplevels();
    }
}

thread_local! {
    /// Set while a [`Backend::run`] call is on the stack.
    ///
    /// Thread-scoped rather than a field of the [`Backend`], because the hazard
    /// is two dispatchers over one `&mut S` and nothing ties the second `run` to
    /// the same backend: a handler holding a `&Backend` for a *different*
    /// backend reaches it just as easily, and the aliasing is identical. A
    /// per-backend flag would miss that. Thread-scoped is as wide as this can
    /// usefully go — wlroots' event loop is single-threaded, and two threads
    /// each driving their own display are genuinely independent.
    static RUNNING: Cell<bool> = const { Cell::new(false) };
}

/// Marks [`Backend::run`] as being on the stack, and clears the mark on the way
/// out — including the `?` paths, which is why it is a guard and not a pair of
/// assignments.
///
/// The sibling of `dispatch.rs`'s handler guard, and deliberately not merged
/// with it: this one spans a whole `run` call and refuses a second `run`, while
/// that one spans a single handler delivery and refuses the event loop being
/// driven from inside it. `run` itself drives the loop, so one flag could not
/// serve both — it would have to be clear for the very call the other exists to
/// forbid.
struct ReentryGuard {
    /// Nothing to hold; the field exists only so the guard cannot be
    /// constructed without going through [`ReentryGuard::acquire`].
    _private: (),
}

impl ReentryGuard {
    fn acquire() -> Result<Self> {
        RUNNING.with(|running| {
            if running.get() {
                return Err(Error::Reentrant("Backend::run"));
            }
            running.set(true);
            Ok(ReentryGuard { _private: () })
        })
    }
}

impl Drop for ReentryGuard {
    fn drop(&mut self) {
        // Only ever reached for a guard that `acquire` handed out, so this
        // never clears a flag it did not set: the failing path returns before
        // constructing one.
        RUNNING.with(|running| running.set(false));
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

        let alive = Rc::new(Cell::new(true));

        // Registered here, not in `run`, so the flag is trustworthy for the
        // backend's whole life rather than only while a handler loop happens to
        // be running. A consumer can drive `EventLoop::dispatch` directly, and
        // the DRM unplug that kills a multi-backend does not wait for `run`.
        //
        // SAFETY: `wlr_backend_autocreate` returned non-null, so `events.destroy`
        // is an initialised signal on a live backend. The backend is not
        // required to outlive this registration, because `on_backend_destroy`
        // clears `alive` before wlroots frees it. `alive` is heap-allocated
        // behind an `Rc`, so moving the `Backend` returned below neither moves
        // the flag nor re-tags it (see the field's own comment), and the
        // `_death_watch` field is declared before `alive` so it drops — and so
        // reads the flag — while the flag is still there. `on_backend_destroy`
        // never reads `session` or `id`, so null and `None` are the honest
        // values for them.
        let death_watch = unsafe {
            Registration::link(
                &raw mut (*raw.as_ptr()).events.destroy,
                on_backend_destroy,
                std::ptr::null(),
                &raw const *alive,
                None,
                None,
            )
        };

        Ok(Backend {
            raw,
            _death_watch: death_watch,
            alive,
            started: Cell::new(false),
            // Kept from the loop we were created on, rather than asked for
            // again at `run` time — see the field's own comment.
            loop_: loop_.as_non_null(),
            _loop: PhantomData,
        })
    }

    /// The raw backend, for the in-crate callers that pass it to wlroots.
    ///
    /// Callers must have established liveness themselves (`alive_or_err`);
    /// this deliberately does not check, because its callers are inside
    /// operations that already did. [`Runtime::init_graphics`](crate::Runtime::init_graphics)
    /// is one of those callers: it runs before any run, when the backend is
    /// necessarily alive (nothing has had the chance to kill it yet).
    pub(crate) fn as_ptr(&self) -> *mut sys::wlr_backend {
        self.raw.as_ptr()
    }

    /// Start the backend, at most once.
    ///
    /// Deliberately **not** public, and that is a decision rather than an
    /// oversight. wlroots' own header warns that starting "may signal new_input
    /// or new_output immediately", and it means it: the headless backend
    /// announces every output it already holds from *inside*
    /// `wlr_backend_start`. Handlers are installed by [`Backend::run`], so a
    /// consumer who could start the backend first would install them one call
    /// too late and never hear about those outputs — and the symptom would be
    /// silence, not an error. Only `run` may start the backend, so that
    /// sequence cannot be written.
    ///
    /// Returning an error from a public `start` was the alternative and is
    /// worse: it would make the *harmless* `start(); run();` fail loudly for
    /// something `run` goes on to do correctly.
    ///
    /// The idempotence is load-bearing, not tidiness: a second
    /// `wlr_backend_start` re-announces every existing output, so `run` called
    /// twice would deliver a duplicate `new_output` for each.
    fn ensure_started(&self) -> Result<()> {
        alive_or_err(&self.alive)?;
        if self.started.get() {
            return Ok(());
        }

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
            self.started.set(true);
            Ok(())
        } else {
            Err(Error::Operation("wlr_backend_start"))
        }
    }

    /// Wire up handlers, start the backend, and dispatch `iterations` turns of
    /// the event loop.
    ///
    /// This is the only thing that starts a backend, and it starts it *after*
    /// installing handlers, because `wlr_backend_start` announces the outputs a
    /// backend already has synchronously, before it returns. Starting is done
    /// once however many times this is called.
    ///
    /// The loop dispatched is the one this backend was created on, which is
    /// why nothing here asks for a display: the backend keeps that pointer, so
    /// there is no second value that could disagree with it.
    ///
    /// `iterations` is a count rather than a block-forever loop, and that is a
    /// deliberate interim shape rather than the final API: a blocking
    /// `run_forever` needs signal handling to be interruptible, which belongs
    /// with a later slice, and adding it then is a new method rather than a
    /// change to this one. So do **not** build around `u32::MAX` as a way to
    /// spell "run until quit" — it will spin the loop the same way any other
    /// count does, and the blocking entry point is coming.
    ///
    /// # Panics
    ///
    /// Never directly, but a panic escaping one of `state`'s handler methods
    /// aborts the process; see [`OutputHandler`]'s own documentation.
    ///
    /// Handlers are installed for the duration of this call only: the `frame`
    /// and `destroy` listeners for every output announced during it live in a
    /// `Session` local to the call, and unlink when that `Session` drops at
    /// the end of it. A later `run` does not relink them — and
    /// `ensure_started` short-circuits once the backend has started, so
    /// nothing re-announces those outputs either — so no further event,
    /// including `destroyed`, is ever delivered for an output announced by an
    /// earlier `run`. In practice that means one `run`.
    ///
    /// # Errors
    ///
    /// [`Error::Destroyed`] if the backend destroyed itself — either before
    /// this call, or during one of the dispatch turns. wlroots' multi-backend
    /// does that when a primary underlying backend goes away, so it is an
    /// ordinary hardware event rather than a programming error. Once it
    /// happens, every later call on this `Backend` fails the same way; the
    /// value is inert and should be dropped.
    ///
    /// [`Error::Reentrant`] if called from inside one of its own handlers.
    /// A consumer whose state holds a `&Backend` can reach this — nothing in
    /// the signature stops them — and it has to be refused rather than
    /// tolerated: a second `run` would build a second dispatcher over the same
    /// `&mut S`, and two dispatchers cannot see each other's reentrancy
    /// guard, so the `&mut S` the outer call is holding would be aliased. (It
    /// would also install a second `new_output` listener and duplicate every
    /// announcement, but that is only the visible symptom.)
    ///
    /// [`Error::Operation`] if `wlr_backend_start` fails, or if a dispatch
    /// turn's `wl_event_loop_dispatch` fails; the two are distinguished by the
    /// error's payload but not by which variant is returned.
    pub fn run<S: OutputHandler>(&self, state: &mut S, iterations: u32) -> Result<()> {
        // `run` predates `Runtime` and its signature is frozen at `S:
        // OutputHandler`, so it makes a private, empty one: it declares no fd
        // sources, `no_fd_sources` registers nothing from it, and `deliver`
        // never reaches it for an output event. This is what lets one
        // `Session` type, and one `run_inner`, serve both entry points
        // without widening `run`'s own bound.
        let runtime = Runtime::new()?;
        self.run_inner::<S>(
            None,
            state,
            &runtime,
            Until::Turns(iterations),
            RunHooks {
                deliver: deliver::<S>,
                should_stop: never_stop::<S>,
                register_sources: no_fd_sources::<S>,
                register_extra: no_extra::<S>,
            },
        )
    }

    /// Wire up every handler trait, start the backend, and dispatch until
    /// `until` says otherwise.
    ///
    /// The superset of [`run`](Backend::run): it delivers output events
    /// identically, and additionally registers the [`Runtime`]'s fd sources —
    /// and, from 0.20.2 and 0.20.3, its xdg shell and its seat — for the
    /// duration of this call. Registration lives with the call and is torn
    /// down when it returns, which is why the runtime holds *declarations*
    /// and this holds the registrations.
    ///
    /// `display` must own this backend's event loop. Nothing in the type
    /// system relates them, so it is checked: the alternative is dispatching
    /// a different display's loop and flushing a different display's clients,
    /// which produces silence rather than an error.
    ///
    /// Every turn ends with [`Display::flush_clients`], because a loop driven
    /// through `wl_event_loop_dispatch` has nothing else that would.
    ///
    /// # Panics
    ///
    /// Never directly. A panic escaping one of `state`'s handler methods
    /// aborts the process; see [`OutputHandler`]. A panic escaping
    /// [`LoopHandler::should_stop`] unwinds normally, since that one is not
    /// called from C.
    ///
    /// # Errors
    ///
    /// [`Error::Mismatch`] if `display` does not own this backend's loop.
    /// [`Error::Destroyed`], [`Error::Reentrant`] and [`Error::Operation`]
    /// exactly as [`run`](Backend::run) documents them.
    pub fn run_all<S: Handlers>(
        &self,
        display: &Display,
        state: &mut S,
        runtime: &Runtime,
        until: Until,
    ) -> Result<()> {
        use sys::wayland_sys::ffi_dispatch;
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: `display` is live for the call.
        let their_loop = unsafe {
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_display_get_event_loop,
                display.as_ptr()
            )
        };
        if their_loop != self.loop_.as_ptr() {
            return Err(Error::Mismatch("Backend::run_all"));
        }

        self.run_inner::<S>(
            Some(display),
            state,
            runtime,
            until,
            RunHooks {
                deliver: deliver_all::<S>,
                should_stop: should_stop_of::<S>,
                register_sources: Backend::register_fd_sources::<S>,
                register_extra: Backend::register_toplevel_and_input::<S>,
            },
        )
    }

    /// The body both entry points share.
    ///
    /// `hooks` bundles what `run` and `run_all` cannot share directly:
    /// `run`'s bound is `OutputHandler` and `run_all`'s is `Handlers`, so one
    /// function cannot name `S::should_stop` or instantiate a
    /// `Handlers`-bound callback without the stricter bound leaking onto
    /// `run`, which is frozen. Each entry point supplies the version that
    /// matches its own bound, and `run`'s versions (`never_stop`,
    /// `no_fd_sources`) are bound by nothing at all.
    fn run_inner<S: OutputHandler>(
        &self,
        display: Option<&Display>,
        state: &mut S,
        runtime: &Runtime,
        until: Until,
        hooks: RunHooks<'d, S>,
    ) -> Result<()> {
        alive_or_err(&self.alive)?;
        let _reentry = ReentryGuard::acquire()?;

        // `state` is consumed into a raw pointer here and never touched as a
        // reference again for the rest of this function, so no `&mut S` is live
        // while a callback delivers through the dispatcher.
        let session = Session {
            dispatcher: Dispatcher::new(&raw mut *state),
            outputs: RefCell::new(HashMap::new()),
            toplevels: RefCell::new(HashMap::new()),
            inputs: RefCell::new(Vec::new()),
            last_key_consumed: Cell::new(false),
            runtime,
            deliver: hooks.deliver,
        };

        // Clears `runtime`'s toplevel tables on every exit from this
        // function — normal return, an early `?`, or a panic — because the
        // per-toplevel destroy listener that would otherwise keep them
        // truthful is itself torn down with `session` at the end of this
        // call (see `Runtime::clear_toplevels`'s own doc for the hazard this
        // closes: a `ToplevelId` resolving to memory wlroots already freed,
        // reachable once this function has returned). Declared before the
        // registrations below so it drops *after* they do, though nothing
        // here depends on that relative order — clearing the table touches
        // no signal and nothing any `Registration::drop` reads.
        let _toplevel_table_guard = ToplevelTableGuard(runtime);

        // Declared after `session`, so it drops — and therefore decides
        // about unlinking — while the session it names is still alive. Bound
        // to a named `_new_output` rather than to `_`, which would drop it at
        // the end of its own statement and unregister before the loop ran.
        //
        // SAFETY: the check above establishes the backend is live, so
        // `events.new_output` is an initialised signal; and it is not required
        // to outlive this registration, because the destroy watch installed in
        // `autocreate` clears `self.alive` before wlroots frees it. That flag
        // lives in a box owned by `self`, which outlives this call. `S` never
        // learns of the session, so nothing can hold a reference to it across
        // `emit` — the aliasing condition `Dispatcher::emit` requires — and
        // `on_new_output::<S>` casts the erased pointer back to the very
        // `Session<'_, S>` paired with it here. `session` is a local that is
        // never moved after this point, so the address stays valid for the
        // call.
        let _new_output = unsafe {
            Registration::link(
                &raw mut (*self.raw.as_ptr()).events.new_output,
                on_new_output::<S>,
                (&raw const session).cast::<()>(),
                &raw const *self.alive,
                None,
                None,
            )
        };

        // Sources are registered here, for this call only. `_sources` is a
        // named binding rather than `_`: binding to `_` would drop the vector
        // — and so remove every source — at the end of this statement.
        let _sources = (hooks.register_sources)(self, runtime, &session)?;

        // Same reasoning as `_sources`, for whatever `run_all` needs beyond
        // fd sources — today just the xdg shell's `new_toplevel` listener,
        // registered only if `create_xdg_shell` was called before this run.
        let _extra = (hooks.register_extra)(self, runtime, &session)?;

        // Only now, with the listeners in place. `wlr_backend_start` announces
        // the backend's existing outputs synchronously, so starting before this
        // point would emit them into an empty signal; see `ensure_started`.
        self.ensure_started()?;

        // SAFETY: `self.loop_` was taken from the `EventLoop<'d>` handed to
        // `autocreate`, so it names a live loop belonging to a display that
        // outlives `'d` — and `&self` keeps that borrow alive for this call.
        //
        // Dispatching here is not the re-entry `EventLoop::dispatch` refuses:
        // `run_inner` is not a handler, and the flag that refusal consults is
        // set only for the duration of a delivery, strictly inside the
        // `loop_.dispatch(timeout)` call below rather than around it.
        let loop_ = unsafe { EventLoop::from_raw(self.loop_) };
        let (timeout, mut remaining) = match until {
            Until::Turns(n) => (0, Some(n)),
            Until::Stop => (-1, None),
        };

        loop {
            if let Some(0) = remaining {
                return Ok(());
            }
            loop_.dispatch(timeout)?;
            if let Some(display) = display {
                display.flush_clients();
            }
            // Checked every turn rather than only on entry: a backend that dies
            // mid-loop leaves nothing useful to dispatch, and reporting it is
            // the difference between a consumer learning about an unplugged GPU
            // and silently spinning out the remaining iterations.
            alive_or_err(&self.alive)?;

            // SAFETY: `session.dispatcher.state_ptr()` is the pointer `session`
            // was built with above, from `&raw mut *state` — a live `&mut S`
            // this call itself is still holding, so it is valid and
            // non-dangling regardless of what `hooks.should_stop` does with
            // it. No handler is running at this point — `Dispatcher::emit`
            // clears its guard before returning, and the only calls into
            // `emit` happen inside `loop_.dispatch` above, which has already
            // returned — so `hooks.should_stop` (which derefs the pointer) is
            // the sole reader here and sees no other `&mut S` live. This is
            // the discharge site `state_ptr`'s own doc requires: the caller
            // states, right here, why no handler is on the stack.
            let stop = unsafe { (hooks.should_stop)(session.dispatcher.state_ptr()) };
            if stop {
                return Ok(());
            }
            if let Some(n) = remaining.as_mut() {
                *n -= 1;
            }
        }
    }

    /// Register every declared fd source with the loop, for this run.
    fn register_fd_sources<S: Handlers>(
        &self,
        runtime: &Runtime,
        session: &Session<'_, S>,
    ) -> Result<Vec<FdRegistration>> {
        use sys::wayland_sys::ffi_dispatch;
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        let mut out = Vec::new();
        // The borrow is released before anything can call back into the
        // runtime: `wl_event_loop_add_fd` does not dispatch, but the rule is
        // absolute here rather than case-by-case, so collect first.
        let declared: Vec<(std::os::fd::RawFd, u32, SourceId)> = {
            let sources = runtime.inner.sources.borrow();
            sources
                .iter()
                .map(|s| (s.fd.as_raw_fd(), s.interest.mask(), s.id))
                .collect()
        };

        for (fd, mask, id) in declared {
            let ctx = Box::new(FdCtx {
                session: (session as *const Session<'_, S>).cast::<()>(),
                id,
            });
            // SAFETY: the loop is live for this call; `ctx` stays boxed and
            // unmoved until the `FdRegistration` below drops, which removes
            // the source first; and `on_fd_ready::<S>` is instantiated at the
            // same `S` the session belongs to.
            let source = unsafe {
                ffi_dispatch!(
                    sys::wayland_sys::server::wayland_server_handle(),
                    wl_event_loop_add_fd,
                    self.loop_.as_ptr(),
                    fd,
                    mask,
                    on_fd_ready::<S>,
                    (&raw const *ctx).cast::<std::ffi::c_void>().cast_mut()
                )
            };
            if source.is_null() {
                return Err(Error::Operation("wl_event_loop_add_fd"));
            }
            out.push(FdRegistration { source, _ctx: ctx });
        }
        Ok(out)
    }

    /// Link the xdg shell's `new_toplevel` listener, if
    /// [`Runtime::create_xdg_shell`](crate::Runtime::create_xdg_shell) was
    /// called before this run, and the backend's `new_input` listener, if
    /// [`Runtime::create_seat`](crate::Runtime::create_seat) was. Either or
    /// both may be absent — a consumer who never creates a shell gets no
    /// toplevels, and one who never creates a seat gets no input, and
    /// neither is an error.
    fn register_toplevel_and_input<S: Handlers>(
        &self,
        runtime: &Runtime,
        session: &Session<'_, S>,
    ) -> Result<Vec<Registration>> {
        let mut regs = Vec::new();

        if let Some(shell) = runtime.xdg_shell_ptr() {
            // SAFETY: `create_xdg_shell` returned a non-null `wlr_xdg_shell`
            // owned by the display, which this call requires to outlive it
            // (see `run_all`'s own doc); `session` is a local that never
            // moves again for the rest of `run_inner`, so the erased pointer
            // stays valid for as long as this registration lives. No
            // liveness flag is needed: the shell's owner (the display)
            // cannot predecease this call.
            regs.push(unsafe {
                Registration::link(
                    &raw mut (*shell.as_ptr()).events.new_toplevel,
                    on_new_toplevel::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                    None,
                    None,
                )
            });
        }

        if runtime.seat_ptr().is_some() {
            // SAFETY: the backend is live (`run_inner` checked `alive_or_err`
            // on entry, before this hook runs), so `events.new_input` is an
            // initialised signal; and the registration is not required to
            // outlive the backend, because `self.alive` — cleared by
            // `on_backend_destroy` before wlroots frees anything — is passed
            // as the liveness flag. `session` is paired with `on_new_input`
            // at the same `S`, as above.
            regs.push(unsafe {
                Registration::link(
                    &raw mut (*self.raw.as_ptr()).events.new_input,
                    on_new_input::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    &raw const *self.alive,
                    None,
                    None,
                )
            });
        }

        Ok(regs)
    }
}

/// How long [`Backend::run_all`] keeps dispatching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Until {
    /// Dispatch exactly this many turns without blocking (a zero timeout),
    /// the shape [`Backend::run`] has always had. Use this in tests: a run
    /// that nothing ever stops still returns.
    Turns(u32),
    /// Block on each turn until [`LoopHandler::should_stop`] returns `true`.
    ///
    /// This is the shape a real compositor wants — the process sleeps between
    /// events instead of spinning — and it is why `should_stop` exists at all.
    /// A consumer with no way to set their stop condition will hang here.
    Stop,
}

/// The callbacks `run_inner` needs but cannot choose itself, bundled so
/// `run_inner` takes one struct instead of trailing `fn` parameters (which is
/// what it used to do, and what tripped `clippy::too_many_arguments` — a real
/// signal here, not noise to silence: unrelated callbacks are easy to pass in
/// the wrong order, and a named field cannot be).
///
/// See `run_inner`'s own doc comment for why these are parameters at all
/// rather than being chosen inside it.
struct RunHooks<'d, S> {
    deliver: fn(&Session<'_, S>, &mut S, Event),

    /// # Safety
    ///
    /// The caller must pass a pointer that is valid to dereference as `&mut S`
    /// for the duration of the call, and must not have any other live
    /// reference to the same `S` at the time of the call — the same
    /// obligation [`Dispatcher::state_ptr`] itself declines to discharge.
    /// `unsafe fn` rather than a safe one taking the same pointer, precisely
    /// so that obligation cannot be met silently: every caller of a value in
    /// this field writes its own `unsafe` block and its own justification —
    /// see `run_inner`'s call site — rather than inheriting one written here
    /// that may not hold for a pointer this field was never shown.
    should_stop: unsafe fn(*mut S) -> bool,
    register_sources: fn(&Backend<'d>, &Runtime, &Session<'_, S>) -> Result<Vec<FdRegistration>>,

    /// Whatever `run_all` needs registered beyond fd sources — the xdg
    /// shell's `new_toplevel` listener today, the seat and the backend's own
    /// `new_input` in later releases. `run`'s slot registers nothing, for the
    /// same reason `register_sources`'s does not: `run`'s bound is
    /// `OutputHandler`, which cannot instantiate a `Handlers`-bound callback.
    register_extra: fn(&Backend<'d>, &Runtime, &Session<'_, S>) -> Result<Vec<Registration>>,
}

/// One boxed fd-source registration: the `wl_event_source` libwayland handed
/// back, plus the context its callback needs.
///
/// Removed from the loop by `Drop`, for the same reason [`Registration`]
/// unlinks there: an early `?` return in the middle of `run_inner` must not
/// leave a live source pointing at a freed context.
struct FdRegistration {
    source: *mut sys::wl_event_source,
    // Declared after `source` so it is dropped after the source is removed,
    // never while libwayland could still dispatch through it.
    _ctx: Box<FdCtx>,
}

#[repr(C)]
struct FdCtx {
    /// An erased `*const Session<'_, S>`, for the `S` this context's
    /// `on_fd_ready::<S>` was instantiated at — the same pairing
    /// [`Bound::session`] documents.
    session: *const (),
    id: SourceId,
}

impl Drop for FdRegistration {
    fn drop(&mut self) {
        use sys::wayland_sys::ffi_dispatch;
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: `source` came from `wl_event_loop_add_fd` on a loop that
        // outlives this run (the `Display` borrow guarantees it), and is
        // removed exactly once, here.
        let _ = unsafe {
            ffi_dispatch!(
                sys::wayland_sys::server::wayland_server_handle(),
                wl_event_source_remove,
                self.source
            )
        };
    }
}

unsafe extern "C" fn on_fd_ready<S: Handlers>(
    _fd: std::os::raw::c_int,
    mask: u32,
    data: *mut std::ffi::c_void,
) -> std::os::raw::c_int {
    // SAFETY: libwayland invokes this only for a source `register_fd_sources`
    // registered, whose `data` is the `FdCtx` boxed alongside it — alive
    // because the `FdRegistration` owning both is dropped (removing the
    // source first) before the box is freed. Its `session` is the
    // `*const Session<'_, S>` paired with this instantiation at the same `S`.
    unsafe {
        let ctx = data.cast::<FdCtx>();
        let session = (*ctx).session.cast::<Session<'_, S>>();
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::FdReady((*ctx).id, mask), deliver);
    }
    // libwayland ignores the return value for fd sources; 0 is what every
    // in-tree compositor returns.
    0
}

/// Never stops a run. `run`'s `should_stop` slot: `run`'s bound is
/// `OutputHandler`, which says nothing about `LoopHandler`, so there is no
/// `S::should_stop` to call.
///
/// # Safety
///
/// None beyond what [`RunHooks::should_stop`] documents — this particular
/// implementation happens not to dereference `_state` at all, but it is
/// `unsafe fn` because the *slot* is, and a safe body here would be a
/// misleading precedent for the next fn written to fill it.
unsafe fn never_stop<S>(_state: *mut S) -> bool {
    false
}

/// `run_all`'s `should_stop` slot, deferring to the handler's own answer.
///
/// # Safety
///
/// `state` must be valid to dereference as `&mut S` for the duration of this
/// call, with no other live reference to the same `S` — see
/// [`RunHooks::should_stop`]. This function only reborrows and calls through
/// it; it does not, and cannot, establish that on its own.
unsafe fn should_stop_of<S: LoopHandler>(state: *mut S) -> bool {
    // SAFETY: the caller just discharged exactly this obligation, per this
    // function's own `# Safety` section.
    unsafe { (*state).should_stop() }
}

/// `run`'s `register_sources` slot: `run` makes its own empty [`Runtime`]
/// (see its own doc comment) that nothing ever adds a source to, so there is
/// nothing to register and no need for a `Handlers` bound to do it with.
fn no_fd_sources<S>(
    _backend: &Backend<'_>,
    _runtime: &Runtime,
    _session: &Session<'_, S>,
) -> Result<Vec<FdRegistration>> {
    Ok(Vec::new())
}

/// `run`'s `register_extra` slot: for the same reason as `no_fd_sources`,
/// there is nothing to register.
fn no_extra<S>(
    _backend: &Backend<'_>,
    _runtime: &Runtime,
    _session: &Session<'_, S>,
) -> Result<Vec<Registration>> {
    Ok(Vec::new())
}

/// Delivery for `run_all`: every event kind, including the ones `deliver`
/// (which is bound only by `OutputHandler`) cannot route.
fn deliver_all<S: Handlers>(session: &Session<'_, S>, state: &mut S, ev: Event) {
    match ev {
        Event::NewOutput(id) => with_output(session, id, |output| state.new_output(output)),
        Event::OutputFrame(id) => with_output(session, id, |output| state.frame(output)),
        Event::OutputDestroyed(id) => state.destroyed(id),
        Event::FdReady(id, mask) => {
            // Resolving through the runtime rather than carrying the fd in
            // the event is what makes deferral sound here too: a source the
            // runtime no longer knows about simply misses, and the event is
            // dropped instead of naming a closed descriptor.
            let readiness = crate::Readiness::from_mask(mask);
            session
                .runtime
                .with_fd(id, |fd| state.fd_ready(id, fd, readiness));
        }
        Event::NewToplevel(id) => with_toplevel(session, id, |t| state.new_toplevel(t)),
        Event::ToplevelInitialCommit(id) => with_toplevel(session, id, |t| state.initial_commit(t)),
        Event::ToplevelMapped(id) => with_toplevel(session, id, |t| state.mapped(t)),
        Event::ToplevelUnmapped(id) => state.unmapped(id),
        Event::ToplevelTitleChanged(id) => with_toplevel(session, id, |t| state.title_changed(t)),
        Event::ToplevelDestroyed(id) => state.toplevel_destroyed(id),
        Event::Key {
            keysym,
            modifiers_raw,
            pressed,
            time_msec,
        } => {
            // Not a safety comment: `modifiers_raw` is the mask read at
            // emission time in `on_key`, carried through the event rather
            // than re-read here, because a deferred key must report the
            // modifiers that were held *when it was pressed* — see
            // `Modifiers::from_mask`'s own doc.
            let modifiers = Modifiers::from_mask(modifiers_raw);
            let ev = KeyEvent::new(keysym, modifiers, pressed, time_msec);
            let consumed = state.key(&ev);
            session.last_key_consumed.set(consumed);
        }
        Event::PointerMotion {
            x_milli,
            y_milli,
            time_msec,
        } => {
            state.pointer_motion(x_milli as f64 / 1000.0, y_milli as f64 / 1000.0, time_msec);
        }
        Event::PointerButton {
            x_milli,
            y_milli,
            button,
            pressed,
            time_msec,
        } => {
            state.pointer_button(
                x_milli as f64 / 1000.0,
                y_milli as f64 / 1000.0,
                button,
                pressed,
                time_msec,
            );
        }
    }
}

/// Borrow the toplevel `id` names, if this runtime still knows of one.
///
/// The table borrow is released before `f` runs: a handler can re-enter
/// wlroots (staging a configure, say), which can fire a signal, which can
/// take the borrow mutably.
///
/// The same obligation `with_output` carries applies here and is worth
/// restating: `f` must not be able to reach anything that frees the toplevel
/// mid-call. `Toplevel` exposes `id`, `title`, `app_id` and `pid`, none of
/// which can; whoever adds a method that can owes this line an answer,
/// because the handle would name freed memory for the rest of `f`.
fn with_toplevel<S>(session: &Session<'_, S>, id: ToplevelId, f: impl FnOnce(&Toplevel<'_>)) {
    let Some(entry) = session.runtime.toplevel_entry(id) else { return };
    // SAFETY: an entry is removed by `on_toplevel_destroy`, which wlroots runs
    // before it frees the toplevel, so a present entry names a live one. The
    // handle is created and dropped inside this call, so it cannot outlive
    // the handler `f` passes it to, and `f` cannot drive the loop (the
    // dispatcher's handler flag is set for exactly this window).
    let toplevel = unsafe { Toplevel::from_raw_with_id(entry.raw.as_ptr(), id) };
    f(&toplevel);
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
/// Returns the bare `u64` rather than a typed id, so both [`OutputId`] and
/// [`crate::ToplevelId`] can share one implementation instead of each having
/// its own copy of this logic.
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live
/// object.
unsafe fn ensure_id_raw(set: *mut sys::wlr_addon_set) -> u64 {
    // SAFETY: the caller's guarantee is exactly what both calls below require.
    // `attach_id`'s additional precondition — that no id addon is attached yet
    // — is discharged by the `find_id` check immediately preceding it, and
    // nothing can attach one in between: wlroots' event loop, and therefore
    // every caller of this function, is single-threaded.
    //
    // `attach_id` re-walks the addon list itself, via its own `assert!`
    // (deliberately not `debug_assert!`, since it is a safety net on a
    // `pub(crate)` unsafe fn, not merely a debugging aid), so the `None` arm
    // below costs two walks rather than one. Left as-is rather than factored
    // into an unchecked variant: this is the object-announcement path, not
    // the frame path, so the cost is small and bounded by object count, and
    // it is not worth a second entry point to `attach_id` for it.
    unsafe {
        match find_id(set.cast_const()) {
            Some(id) => id,
            None => attach_id(set),
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
    // In particular `session` is never read, so its being null is fine.
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
    // whose `session` was paired with this very instantiation — same `S` — and
    // is valid for as long as that registration exists. The `new_output` signal
    // carries a `*mut wlr_output`, so the cast of `data` matches what wlroots
    // documents it to pass, and the output is live and fully initialised at the
    // point wlroots announces it — including its addon set and its own signals.
    // The two registrations below name that output's signals and are dropped
    // while it is still alive (in `on_output_destroy`, or with the session at
    // the end of `run`), which is what lets them pass a null liveness flag.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let output = data.cast::<sys::wlr_output>();

        // Give the output an identity before anyone can ask for one.
        let id = OutputId(ensure_id_raw(&raw mut (*output).addons));

        // `(*bound).session` is forwarded verbatim rather than re-derived, so
        // these two callbacks are instantiated at the `S` that pointer already
        // belongs to — the pairing `Bound::session` documents.
        let frame = Registration::link(
            &raw mut (*output).events.frame,
            on_frame::<S>,
            (*bound).session,
            std::ptr::null(),
            Some(id),
            None,
        );
        let destroy = Registration::link(
            &raw mut (*output).events.destroy,
            on_output_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
            Some(id),
            None,
        );

        // Registered before the handler is told, so that a handler asking about
        // this output — or anything deferred behind it — can resolve the id.
        // The borrow ends with the statement, before any handler runs.
        //
        // Any displaced entry is bound and dropped *after* that statement, so
        // its two `Registration::drop`s run with the `RefMut` released. They
        // only call `wl_list_remove` today, so it would be harmless either way;
        // binding it keeps the ordering true for a future `Drop` that touches
        // the registry, which would otherwise panic on a double borrow inside
        // an `extern "C"` frame — that is, abort.
        let displaced = (*session).outputs.borrow_mut().insert(
            id,
            OutputEntry {
                raw: output,
                _frame: frame,
                _destroy: destroy,
            },
        );
        drop(displaced);

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::NewOutput(id), deliver);
    }
}

unsafe extern "C" fn on_frame<S: OutputHandler>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener `on_new_output` linked
    // into an output's `events.frame`, which is the `listener` field of a live
    // `Bound` — live because the registration owning it is removed from the
    // session's registry, and so unlinked from this signal, before the output
    // is freed. Its `session` is the same erased `*const Session<S>` that
    // `on_new_output::<S>` was itself paired with.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        // Cannot be `None`: `on_new_output` is the only site that installs this
        // callback and it always supplies an id. Handled rather than unwrapped
        // because this is an `extern "C"` frame, where a panic aborts.
        let Some(id) = (*bound).id else { return };

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::OutputFrame(id), deliver);
    }
}

/// An output is about to be freed. Forget it *now*, whatever the handler does.
unsafe extern "C" fn on_output_destroy<S: OutputHandler>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: as for `on_frame` — wlroots invokes this only for the listener
    // `on_new_output` linked into this output's `events.destroy`, and the
    // output is still alive for the duration of the emission.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).id else { return };

        // Both values this frame still needs have been copied out of `*bound`
        // above, because the next statement frees it: removing the entry drops
        // its two registrations, one of which owns this very `Bound`. Unlinking
        // the currently-firing listener is what `wl_signal_emit_mutable`
        // exists to tolerate — it advances its cursor past this listener before
        // calling us — so the emission walking the rest of the list is
        // unaffected. `bound` is dangling from here on and is not touched again.
        //
        // This happens before the event is emitted rather than in `deliver`,
        // and that ordering is the whole soundness argument for deferral: an
        // `OutputDestroyed` arriving while a handler is already running gets
        // queued, and wlroots frees the output long before the queue drains. If
        // the entry were removed at delivery time, any event queued behind it
        // — including this output's own `NewOutput` — would resolve the id to a
        // freed output. Removing it here means the lookup simply misses.
        let entry = (*session).outputs.borrow_mut().remove(&id);
        drop(entry);

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::OutputDestroyed(id), deliver);
    }
}

/// A client created a toplevel. Give it an id and a scene tree before anyone
/// is told about it.
unsafe extern "C" fn on_new_toplevel<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener
    // `register_toplevel_shell` linked into `wlr_xdg_shell.events.new_toplevel`,
    // whose `session` is the `*const Session<'_, S>` paired with this
    // instantiation. The signal carries a `*mut wlr_xdg_toplevel`, live and
    // fully initialised — its `base` and `base->surface` included — at the
    // point wlroots announces it.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let toplevel = data.cast::<sys::wlr_xdg_toplevel>();
        let base = (*toplevel).base;
        if base.is_null() {
            return;
        }
        let surface = (*base).surface;
        if surface.is_null() {
            return;
        }

        // The id lives on the surface's addon set, which is the only one that
        // exists here and the one that dies with the toplevel.
        let id = ToplevelId(ensure_id_raw(&raw mut (*surface).addons));

        // No scene to insert into if `init_graphics` was never called —
        // possible if a consumer creates the shell without it, and not this
        // callback's mistake to recover from. Drop the announcement rather
        // than dereference a null tree.
        let Some(scene) = (*session).runtime.scene_ptr() else { return };

        // Insert into the scene before the handler runs, so that a handler
        // positioning the window by id finds a node to position.
        let tree = sys::wlr_scene_xdg_surface_create(&raw mut (*scene.as_ptr()).tree, base);
        let Some(tree) = NonNull::new(tree) else { return };
        let Some(raw) = NonNull::new(toplevel) else { return };

        // Five listeners, all with a null liveness flag: each is dropped from
        // inside the destroy emission, while the object is still alive, which
        // is a stronger guarantee than any flag (see `Registration::drop`).
        //
        // Every one of them carries `id` in its own `Bound::toplevel` rather
        // than recovering it from `data` at callback time: wlroots emits
        // `wlr_surface.events.map`/`.unmap` and
        // `wlr_xdg_toplevel.events.set_title`/`.destroy` with a **null**
        // `data` argument (confirmed against the 0.20 C sources), so a
        // callback that read the id out of `data` would dereference a null
        // pointer on the first real client. Only `commit` (which does carry
        // the surface) and `on_new_toplevel` itself (which carries the
        // toplevel) get to use `data` for identity; the other four use the
        // `Bound` instead. See `Bound::toplevel`'s own doc for the fuller
        // argument.
        let commit = Registration::link(
            &raw mut (*surface).events.commit,
            on_surface_commit::<S>,
            (*bound).session,
            std::ptr::null(),
            None,
            Some(id),
        );
        let map = Registration::link(
            &raw mut (*surface).events.map,
            on_toplevel_map::<S>,
            (*bound).session,
            std::ptr::null(),
            None,
            Some(id),
        );
        let unmap = Registration::link(
            &raw mut (*surface).events.unmap,
            on_toplevel_unmap::<S>,
            (*bound).session,
            std::ptr::null(),
            None,
            Some(id),
        );
        let set_title = Registration::link(
            &raw mut (*toplevel).events.set_title,
            on_toplevel_set_title::<S>,
            (*bound).session,
            std::ptr::null(),
            None,
            Some(id),
        );
        let destroy = Registration::link(
            &raw mut (*toplevel).events.destroy,
            on_toplevel_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
            None,
            Some(id),
        );

        // The registrations own the callbacks' backing memory, so they live in
        // the session's table alongside the entry, and are dropped when it is
        // removed. The `Bound`s carry no id — the callbacks recover it from
        // the surface's addon set, which is where `ensure_id_raw` put it,
        // because `Bound::id` is typed as `Option<OutputId>` and widening a
        // published private field's type is churn for no gain.
        let displaced = (*session).toplevels.borrow_mut().insert(
            id,
            ToplevelListeners {
                _commit: commit,
                _map: map,
                _unmap: unmap,
                _set_title: set_title,
                _destroy: destroy,
            },
        );
        drop(displaced);

        (*session).runtime.record_toplevel(id, raw, tree);

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::NewToplevel(id), deliver);
    }
}

/// Recover the [`ToplevelId`] a surface's id addon carries, if any.
///
/// # Safety
///
/// `surface` must be a live `wlr_surface` with an initialised addon set.
unsafe fn toplevel_id_of_surface(surface: *mut sys::wlr_surface) -> Option<ToplevelId> {
    // SAFETY: the caller guarantees the surface is live.
    unsafe { find_id(&raw const (*surface).addons).map(ToplevelId) }
}

unsafe extern "C" fn on_surface_commit<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_toplevel` into this surface's `events.commit`
    // and unlinked (from `on_toplevel_destroy`) before the surface is freed;
    // `data` is the `wlr_surface`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let surface = data.cast::<sys::wlr_surface>();
        let Some(id) = toplevel_id_of_surface(surface) else { return };
        let Some(entry) = (*session).runtime.toplevel_entry(id) else { return };
        let base = (*entry.raw.as_ptr()).base;
        if base.is_null() || !(*base).initial_commit {
            return;
        }

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::ToplevelInitialCommit(id), deliver);

        // xdg-shell requires an answer to the first commit. The handler may
        // already have staged one through `Runtime::set_toplevel_*`, in which
        // case wlroots coalesces this into that same configure; if it staged
        // nothing, this is what stops the client waiting forever.
        sys::wlr_xdg_surface_schedule_configure(base);
    }
}

unsafe extern "C" fn on_toplevel_map<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_toplevel` into this surface's `events.map`.
    // `_data` is deliberately unused: wlroots 0.20 emits `wlr_surface.events.
    // map` with a **null** `data` argument (`wlr_compositor.c`), so the id
    // must come from `Bound::toplevel`, which `on_new_toplevel` set at link
    // time — see that field's own doc for the full argument.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        // Cannot be `None`: `on_new_toplevel` is the only site that installs
        // this callback and it always supplies a toplevel id. Handled rather
        // than unwrapped because this is an `extern "C"` frame, where a
        // panic aborts.
        let Some(id) = (*bound).toplevel else { return };
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::ToplevelMapped(id), deliver);
    }
}

unsafe extern "C" fn on_toplevel_unmap<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: as for `on_toplevel_map` — `wlr_surface.events.unmap` is the
    // other signal `wlr_compositor.c` emits with a null `data`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).toplevel else { return };
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::ToplevelUnmapped(id), deliver);
    }
}

unsafe extern "C" fn on_toplevel_set_title<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_toplevel` into `wlr_xdg_toplevel.events.
    // set_title`. `_data` is deliberately unused: wlroots 0.20 emits this
    // signal with a **null** `data` argument (`wlr_xdg_toplevel.c`), so the
    // id must come from `Bound::toplevel` rather than from `data`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).toplevel else { return };
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::ToplevelTitleChanged(id), deliver);
    }
}

/// A toplevel is about to be freed. Forget it *now*, whatever the handler does.
unsafe extern "C" fn on_toplevel_destroy<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_toplevel` into `wlr_xdg_toplevel.events.
    // destroy`; the toplevel is still alive for the duration of the
    // emission. `_data` is deliberately unused: wlroots 0.20 emits this
    // signal with a **null** `data` argument too (`wlr_xdg_toplevel.c`), so
    // — as for `on_toplevel_set_title` — the id comes from `Bound::toplevel`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).toplevel else { return };

        // Both tables are cleared before the event is emitted, and that
        // ordering is the whole soundness argument for deferral: a destroy
        // queued behind a running handler is delivered long after wlroots
        // freed the object, so a lookup at delivery time would resolve the id
        // to freed memory. Clearing here means it simply misses.
        //
        // Removing the entry drops its five registrations, one of which owns
        // this very `Bound`. `wl_signal_emit_mutable` advances past the
        // firing listener before calling us, so that is what it exists to
        // tolerate; `bound` is dangling from here on and is not touched again.
        (*session).runtime.forget_toplevel(id);
        let listeners = (*session).toplevels.borrow_mut().remove(&id);
        drop(listeners);

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::ToplevelDestroyed(id), deliver);
    }
}

/// One live input device: its own liveness flag, and every registration
/// linked against it.
///
/// A device can be unplugged mid-run independently of the backend dying —
/// nothing else in this crate ties a keyboard or pointer's lifetime to
/// anything longer-lived — so each one gets the same watch-your-own-destroy
/// treatment [`Backend::alive`] gives the backend itself: `_destroy` clears
/// `alive` from `events.destroy`, which wlroots emits before freeing
/// anything, and every other registration below carries `alive` as its own
/// liveness flag so [`Registration::drop`] knows to skip unlinking from a
/// signal that no longer exists.
struct InputDevice {
    /// `Rc`, not `Box`, for the same reason [`Backend::alive`] is: the flag's
    /// address must survive this struct moving — `Session::inputs` is a
    /// `Vec`, which reallocates — because every other field's `Registration`
    /// holds a raw pointer into it.
    ///
    /// Never read through this field directly (hence `#[allow(dead_code)]`):
    /// every access is through the raw pointers handed to `Registration::link`
    /// at construction time. Its only job here is to keep the `Cell`'s
    /// allocation alive for as long as this `InputDevice` is, which the type
    /// system already guarantees without reading it.
    #[allow(dead_code)]
    alive: Rc<Cell<bool>>,
    _destroy: Registration,
    /// The device's own signals: two for a keyboard (`key`, `modifiers`),
    /// three for a pointer (`motion`, `motion_absolute`, `button`), zero for
    /// a device type this crate does not yet wire up (the destroy watch
    /// above is still linked, so `has_keyboard`/capabilities stay correct
    /// even for an ignored device type).
    _listeners: Vec<Registration>,
}

/// The layout-agnostic, unshifted keysym for `keycode` (evdev numbering) on
/// `kb`'s **compiled keymap** — fixed at layout 0, level 0, deliberately not
/// `kb`'s live `xkb_state`. See [`KeyEvent::keysym`] for why: reading through
/// `xkb_state` would report whatever layout group the user last switched to,
/// which is exactly the layout-*dependence* this function exists to avoid.
///
/// `0` (`XKB_KEY_NoSymbol`) if `kb` has no compiled keymap, or the key has no
/// symbol at layout 0 level 0.
///
/// # Safety
///
/// `kb` must be a live `wlr_keyboard`.
unsafe fn keysym_for_keycode(kb: *mut sys::wlr_keyboard, keycode: u32) -> u32 {
    // SAFETY: the caller guarantees `kb` is live, so `kb.keymap` (when
    // non-null) is a live, immutable-for-the-process-of-reading `xkb_keymap`
    // — wlroots only replaces it wholesale via `wlr_keyboard_set_keymap`,
    // never mutates it in place, and that call cannot run concurrently with
    // this one (wlroots' event loop is single-threaded). `syms` is a live
    // stack local for the out-parameter.
    unsafe {
        let keymap = (*kb).keymap;
        if keymap.is_null() {
            return 0;
        }
        let xkb_keycode = keycode + 8; // evdev -> xkb numbering
        let mut syms: *const xkbcommon_sys::xkb_keysym_t = std::ptr::null();
        let count =
            xkbcommon_sys::xkb_keymap_key_get_syms_by_level(keymap, xkb_keycode, 0, 0, &raw mut syms);
        if count <= 0 || syms.is_null() {
            return 0;
        }
        // SAFETY: `count > 0` and a non-null `syms` together mean
        // `xkb_keymap_key_get_syms_by_level` wrote a valid array of at least
        // one `xkb_keysym_t`, per its own contract; reading the first
        // element is always in bounds.
        *syms
    }
}

/// A new input device was announced. Give a keyboard a keymap and hook it to
/// the seat, attach a pointer to the cursor, and recompute the seat's
/// capabilities either way.
unsafe extern "C" fn on_new_input<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `Backend::register_toplevel_and_input` into
    // `wlr_backend.events.new_input`, whose data is a live `wlr_input_device`
    // — live and fully initialised, including its own `events.destroy`, at
    // the point wlroots announces it. The device outlives this call; the
    // listeners linked below outlive it too, up to the device's own destroy
    // (watched via `InputDevice::_destroy`) or the end of this run,
    // whichever comes first.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let device = data.cast::<sys::wlr_input_device>();
        let runtime = (*session).runtime;

        let alive = Rc::new(Cell::new(true));
        // Registered before anything else on this device, mirroring
        // `Backend::autocreate`'s own death watch: wlroots emits a device's
        // `events.destroy` synchronously before freeing it, so this is
        // linked while the device is certainly still live, and nothing
        // between here and the device's eventual removal can observe it
        // freed without this flag having already been cleared first.
        let destroy = Registration::link(
            &raw mut (*device).events.destroy,
            on_backend_destroy,
            std::ptr::null(),
            &raw const *alive,
            None,
            None,
        );

        let mut listeners = Vec::new();

        match (*device).type_ {
            sys::wlr_input_device_type::WLR_INPUT_DEVICE_KEYBOARD => {
                let kb = sys::wlr_keyboard_from_input_device(device);
                if !kb.is_null() {
                    // A default keymap from the environment (XKB_DEFAULT_LAYOUT
                    // and friends), because a keyboard with no keymap produces
                    // no keysyms at all and the symptom is silence.
                    let ctx = xkbcommon_sys::xkb_context_new(xkbcommon_sys::XKB_CONTEXT_NO_FLAGS);
                    if !ctx.is_null() {
                        let keymap = xkbcommon_sys::xkb_keymap_new_from_names(
                            ctx,
                            std::ptr::null(),
                            xkbcommon_sys::XKB_KEYMAP_COMPILE_NO_FLAGS,
                        );
                        if !keymap.is_null() {
                            sys::wlr_keyboard_set_keymap(kb, keymap);
                            xkbcommon_sys::xkb_keymap_unref(keymap);
                        }
                        xkbcommon_sys::xkb_context_unref(ctx);
                    }
                    sys::wlr_keyboard_set_repeat_info(kb, 25, 600);

                    if let Some(seat) = runtime.seat_ptr() {
                        sys::wlr_seat_set_keyboard(seat.as_ptr(), kb);
                    }
                    runtime.record_keyboard(NonNull::new_unchecked(kb));

                    listeners.push(Registration::link(
                        &raw mut (*kb).events.key,
                        on_key::<S>,
                        (*bound).session,
                        &raw const *alive,
                        None,
                        None,
                    ));
                    listeners.push(Registration::link(
                        &raw mut (*kb).events.modifiers,
                        on_modifiers::<S>,
                        (*bound).session,
                        &raw const *alive,
                        None,
                        None,
                    ));
                }
            }
            sys::wlr_input_device_type::WLR_INPUT_DEVICE_POINTER => {
                if let Some(cursor) = runtime.cursor_ptr() {
                    sys::wlr_cursor_attach_input_device(cursor.as_ptr(), device);
                }
                let pointer = sys::wlr_pointer_from_input_device(device);
                if !pointer.is_null() {
                    listeners.push(Registration::link(
                        &raw mut (*pointer).events.motion,
                        on_pointer_motion::<S>,
                        (*bound).session,
                        &raw const *alive,
                        None,
                        None,
                    ));
                    listeners.push(Registration::link(
                        &raw mut (*pointer).events.motion_absolute,
                        on_pointer_motion_absolute::<S>,
                        (*bound).session,
                        &raw const *alive,
                        None,
                        None,
                    ));
                    listeners.push(Registration::link(
                        &raw mut (*pointer).events.button,
                        on_pointer_button::<S>,
                        (*bound).session,
                        &raw const *alive,
                        None,
                        None,
                    ));
                }
            }
            _ => {}
        }

        (*session).inputs.borrow_mut().push(InputDevice {
            alive,
            _destroy: destroy,
            _listeners: listeners,
        });

        // Capabilities are recomputed rather than accumulated: a seat that
        // advertises a keyboard it does not have makes clients wait for a
        // keymap that never arrives.
        if let Some(seat) = runtime.seat_ptr() {
            let mut caps = WL_SEAT_CAPABILITY_POINTER;
            if runtime.has_keyboard() {
                caps |= WL_SEAT_CAPABILITY_KEYBOARD;
            }
            sys::wlr_seat_set_capabilities(seat.as_ptr(), caps);
        }
    }
}

unsafe extern "C" fn on_key<S: Handlers>(l: *mut sys::wl_listener, data: *mut std::ffi::c_void) {
    // SAFETY: linked by `on_new_input` into a live keyboard's `events.key`,
    // whose data is a `wlr_keyboard_key_event`; the keyboard is live for the
    // duration of the emission.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let ev = data.cast::<sys::wlr_keyboard_key_event>();
        let Some(seat) = (*session).runtime.seat_ptr() else { return };
        // The seat's *active* keyboard, not necessarily the one that fired
        // this signal: with more than one keyboard attached, every key
        // event is still funnelled through one logical keyboard identity at
        // the seat, which is what `wlr_seat_set_keyboard` in `on_new_input`
        // established. A client sees one keyboard regardless of how many
        // physical ones are plugged in.
        let kb = sys::wlr_seat_get_keyboard(seat.as_ptr());
        if kb.is_null() {
            return;
        }

        let keysym = keysym_for_keycode(kb, (*ev).keycode);
        let modifiers_raw = sys::wlr_keyboard_get_modifiers(kb);
        let pressed = (*ev).state == sys::wl_keyboard_key_state::WL_KEYBOARD_KEY_STATE_PRESSED;

        let deliver = (*session).deliver;
        (*session).dispatcher.emit(
            &*session,
            Event::Key {
                keysym,
                modifiers_raw,
                pressed,
                time_msec: (*ev).time_msec,
            },
            deliver,
        );

        // Forwarding is decided by the handler's return value, which
        // `deliver_all` records on the session (an `extern "C"` callback
        // cannot get one back through `emit` directly). A deferred key —
        // one queued behind another handler already running — is forwarded,
        // because the compositor's answer is not known yet and dropping a
        // keystroke is worse than forwarding one.
        if !(*session).last_key_consumed.get() {
            sys::wlr_seat_set_keyboard(seat.as_ptr(), kb);
            sys::wlr_seat_keyboard_notify_key(
                seat.as_ptr(),
                (*ev).time_msec,
                (*ev).keycode,
                (*ev).state.0,
            );
        }
    }
}

unsafe extern "C" fn on_modifiers<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_input` into a live keyboard's
    // `events.modifiers`. No handler is called; this only forwards state the
    // focused client needs to interpret the keys it is being sent.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(seat) = (*session).runtime.seat_ptr() else { return };
        let kb = sys::wlr_seat_get_keyboard(seat.as_ptr());
        if kb.is_null() {
            return;
        }
        sys::wlr_seat_keyboard_notify_modifiers(seat.as_ptr(), &raw mut (*kb).modifiers);
    }
}

/// Move the pointer focus and forward a motion/button to whatever the cursor
/// is over, or clear pointer focus if it is over nothing. Shared by
/// `on_pointer_motion`, `on_pointer_motion_absolute` and `on_pointer_button`
/// rather than factored differently: each caller already has the seat and
/// the cursor's current position in hand, and this only names the repeated
/// "find the surface under the cursor and enter/clear-focus it" step, not
/// the forwarding call itself (which differs per caller: motion sends a
/// motion, a button sends nothing here at all).
///
/// # Safety
///
/// `seat` must be a live `wlr_seat`.
unsafe fn enter_surface_under_cursor<S>(
    session: &Session<'_, S>,
    seat: *mut sys::wlr_seat,
    x: f64,
    y: f64,
    time_msec: u32,
) {
    match session.runtime.toplevel_at(x, y) {
        Some((id, sx, sy)) => {
            let Some(entry) = session.runtime.toplevel_entry(id) else {
                // SAFETY: the seat is live, per this function's contract.
                unsafe { sys::wlr_seat_pointer_notify_clear_focus(seat) };
                return;
            };
            // SAFETY: a present entry names a live toplevel (its destroy
            // callback removes the entry before wlroots frees it), so
            // `base->surface` is a live surface. The seat is live per this
            // function's own contract.
            unsafe {
                let base = (*entry.raw.as_ptr()).base;
                let surface = if base.is_null() { std::ptr::null_mut() } else { (*base).surface };
                if surface.is_null() {
                    sys::wlr_seat_pointer_notify_clear_focus(seat);
                    return;
                }
                sys::wlr_seat_pointer_notify_enter(seat, surface, sx, sy);
                sys::wlr_seat_pointer_notify_motion(seat, time_msec, sx, sy);
            }
        }
        // SAFETY: as above.
        None => unsafe { sys::wlr_seat_pointer_notify_clear_focus(seat) },
    }
}

unsafe extern "C" fn on_pointer_motion<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_input` into a live pointer's `events.motion`,
    // whose data is a `wlr_pointer_motion_event`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let ev = data.cast::<sys::wlr_pointer_motion_event>();
        let runtime = (*session).runtime;
        let Some(cursor) = runtime.cursor_ptr() else { return };

        let device = &raw mut (*(*ev).pointer).base;
        sys::wlr_cursor_move(cursor.as_ptr(), device, (*ev).delta_x, (*ev).delta_y);
        runtime.ensure_cursor_image();

        let (x, y) = ((*cursor.as_ptr()).x, (*cursor.as_ptr()).y);
        let deliver = (*session).deliver;
        (*session).dispatcher.emit(
            &*session,
            Event::PointerMotion {
                x_milli: (x * 1000.0) as i64,
                y_milli: (y * 1000.0) as i64,
                time_msec: (*ev).time_msec,
            },
            deliver,
        );

        if let Some(seat) = runtime.seat_ptr() {
            enter_surface_under_cursor(&*session, seat.as_ptr(), x, y, (*ev).time_msec);
            sys::wlr_seat_pointer_notify_frame(seat.as_ptr());
        }
    }
}

unsafe extern "C" fn on_pointer_motion_absolute<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_input` into a live pointer's
    // `events.motion_absolute`, whose data is a
    // `wlr_pointer_motion_absolute_event`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let ev = data.cast::<sys::wlr_pointer_motion_absolute_event>();
        let runtime = (*session).runtime;
        let Some(cursor) = runtime.cursor_ptr() else { return };

        let device = &raw mut (*(*ev).pointer).base;
        sys::wlr_cursor_warp_absolute(cursor.as_ptr(), device, (*ev).x, (*ev).y);
        runtime.ensure_cursor_image();

        let (x, y) = ((*cursor.as_ptr()).x, (*cursor.as_ptr()).y);
        let deliver = (*session).deliver;
        (*session).dispatcher.emit(
            &*session,
            Event::PointerMotion {
                x_milli: (x * 1000.0) as i64,
                y_milli: (y * 1000.0) as i64,
                time_msec: (*ev).time_msec,
            },
            deliver,
        );

        if let Some(seat) = runtime.seat_ptr() {
            enter_surface_under_cursor(&*session, seat.as_ptr(), x, y, (*ev).time_msec);
            sys::wlr_seat_pointer_notify_frame(seat.as_ptr());
        }
    }
}

unsafe extern "C" fn on_pointer_button<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_input` into a live pointer's `events.button`,
    // whose data is a `wlr_pointer_button_event`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let ev = data.cast::<sys::wlr_pointer_button_event>();
        let runtime = (*session).runtime;
        let Some(cursor) = runtime.cursor_ptr() else { return };
        runtime.ensure_cursor_image();

        let (x, y) = ((*cursor.as_ptr()).x, (*cursor.as_ptr()).y);
        let pressed = (*ev).state == sys::wl_pointer_button_state::WL_POINTER_BUTTON_STATE_PRESSED;

        let deliver = (*session).deliver;
        (*session).dispatcher.emit(
            &*session,
            Event::PointerButton {
                x_milli: (x * 1000.0) as i64,
                y_milli: (y * 1000.0) as i64,
                button: (*ev).button,
                pressed,
                time_msec: (*ev).time_msec,
            },
            deliver,
        );

        // Unconditional, unlike a key: there is no interception for a
        // button, so this always runs after the handler has had its say —
        // see `SeatHandler::pointer_button`'s own doc for why.
        if let Some(seat) = runtime.seat_ptr() {
            // Enter/focus the surface under the cursor first, exactly as a
            // motion would, so a click on a window that never got a prior
            // motion event (the pointer warped there, say) still has pointer
            // focus before the button reaches it.
            enter_surface_under_cursor(&*session, seat.as_ptr(), x, y, (*ev).time_msec);
            sys::wlr_seat_pointer_notify_button(seat.as_ptr(), (*ev).time_msec, (*ev).button, (*ev).state);
            sys::wlr_seat_pointer_notify_frame(seat.as_ptr());
        }
    }
}

/// Borrow the output `id` names, if this session still knows of one.
///
/// The registry borrow is released before `f` runs: a handler can re-enter
/// wlroots (committing an output, say), which can fire a signal, which can take
/// the borrow mutably.
fn with_output<S>(session: &Session<'_, S>, id: OutputId, f: impl FnOnce(&Output<'_>)) {
    let raw = session.outputs.borrow().get(&id).map(|entry| entry.raw);
    let Some(raw) = raw else { return };

    // SAFETY: an entry is removed by `on_output_destroy`, which wlroots runs
    // from `events.destroy` before it frees the output, so a present entry
    // names a live output. The handle is created and dropped inside this call,
    // so it cannot outlive the handler `f` passes it to.
    //
    // The handle must also stay valid *for* the whole of `f`, which the entry
    // being present at this instant does not establish on its own — an output
    // is freed by wlroots, and wlroots only runs when something drives the
    // event loop. So the invariant is: no wlroots code *that can destroy an
    // output* runs between here and `f` returning. Stated that way on purpose —
    // `f` can reach wlroots, via `Output::commit`; what it cannot reach is
    // anything that frees the output. Two facts together give it, and both are
    // enforced rather than hoped for.
    //
    // First, `f` cannot drive the loop. `Dispatcher::emit` sets the thread's
    // handler flag for exactly this window and `EventLoop::dispatch` refuses
    // while it is set, so the one safe public route into wlroots' own
    // dispatching is shut — including via a `&Display` or a `&Backend` that
    // `f`'s state happens to hold, since neither offers another way in.
    // `Backend::run` is refused by its own `ReentryGuard` for the same reason.
    //
    // Second, `f` cannot reach wlroots directly. `Output` exposes only `id`,
    // `name` and `commit`, none of which can destroy an output. Whoever adds a
    // method that *can* — a `destroy`, or anything that lets wlroots tear the
    // output down mid-call — owes this line an answer, because that route
    // bypasses the flag entirely: the handle would still name freed memory for
    // the rest of `f`, and no re-lookup here can help, because `f` holds it.
    let output = unsafe { Output::from_raw_with_id(raw, id) };
    f(&output);
}

/// Route an event to the matching handler method.
///
/// Ids are resolved here rather than carried as handles, which is what makes
/// deferral sound: an output destroyed between queueing and delivery is simply
/// absent from the registry and the event is dropped.
fn deliver<S: OutputHandler>(session: &Session<'_, S>, state: &mut S, ev: Event) {
    match ev {
        Event::NewOutput(id) => with_output(session, id, |output| state.new_output(output)),
        Event::OutputFrame(id) => with_output(session, id, |output| state.frame(output)),
        // No resolution to do, and nothing to drop the event for: the id
        // outlives the object on purpose, and the handler is told about the
        // destruction even though nothing is left to hand it.
        Event::OutputDestroyed(id) => state.destroyed(id),
        // Unreachable: `run` registers no fd sources. Dropped rather than
        // unreachable!() because this is on the path from an `extern "C"`
        // frame, where a panic aborts the process.
        Event::FdReady(..) => {}
        // Unreachable: `run` never registers an xdg shell, so it cannot
        // produce one of these either. Dropped for the same reason as
        // `FdReady` above.
        //
        // Same for every input event: `run` never registers a seat either
        // (`Backend::register_toplevel_and_input` is `run_all`'s hook; `run`
        // uses `no_extra`), so these cannot be produced on this path.
        Event::NewToplevel(..)
        | Event::ToplevelInitialCommit(..)
        | Event::ToplevelMapped(..)
        | Event::ToplevelUnmapped(..)
        | Event::ToplevelTitleChanged(..)
        | Event::ToplevelDestroyed(..)
        | Event::Key { .. }
        | Event::PointerMotion { .. }
        | Event::PointerButton { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{Layout, alloc_zeroed, dealloc};

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
        // never touches the `Bound`, so a null session is fine.
        unsafe {
            assert!(!is_linked(hp, noop), "a fresh signal has no listeners");

            let reg = Registration::link(
                &raw mut (*hp).signal,
                noop,
                std::ptr::null(),
                &raw const (*hp).alive,
                None,
                None,
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
            let _reg =
                unsafe { Registration::link(signal, noop, std::ptr::null(), alive, None, None) };
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
                None,
                None,
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
                None,
                None,
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

    /// A `run` already on the stack must refuse a second one.
    ///
    /// Reachable from safe code: `run` takes `&self`, `&Backend` is `Copy`, and
    /// a handler's state may hold one — so a handler can call `run` again. Two
    /// `run`s means two [`Dispatcher`]s over one `&mut S`, and neither can see
    /// the other's reentrancy guard, so the aliasing the guard exists to
    /// prevent happens anyway.
    #[test]
    fn a_second_run_is_refused_while_the_first_is_on_the_stack() {
        let outer = ReentryGuard::acquire().expect("the first run may proceed");
        assert_eq!(
            ReentryGuard::acquire().err(),
            Some(Error::Reentrant("Backend::run")),
            "a run entered from inside a handler must be refused, and named as \
             re-entry rather than as a C call that failed"
        );

        drop(outer);
        assert!(
            ReentryGuard::acquire().is_ok(),
            "a completed run must leave the next one free to start"
        );
    }

    /// The guard has to survive the `?` paths too — `run` returns early on a
    /// failed start and on every dispatch turn — or one error would lock the
    /// backend out of ever running again.
    #[test]
    fn an_early_return_releases_the_re_entry_guard() {
        fn acquire_then_fail() -> Result<()> {
            let _guard = ReentryGuard::acquire()?;
            Err(Error::Operation("simulated failure"))
        }

        assert!(acquire_then_fail().is_err());
        assert!(
            ReentryGuard::acquire().is_ok(),
            "the guard must clear the flag on the early-return path, or one \
             failed run would lock the backend out of ever running again"
        );
    }

    /// A zeroed, heap-allocated `wlr_output` with its addon set and the two
    /// signals this crate listens on initialised; freed on drop.
    ///
    /// Allocated rather than `std::mem::zeroed`-ed for the reason `output.rs`'s
    /// copy documents: `wlr_output` embeds `wl_listener`s whose bare function
    /// pointers are UB to *materialise* as a zero value, so the bytes are only
    /// ever touched through a raw pointer.
    struct ScratchOutput(*mut sys::wlr_output);

    impl ScratchOutput {
        fn new() -> Self {
            let layout = Layout::new::<sys::wlr_output>();
            // SAFETY: `wlr_output` has fields, so the layout is non-zero-sized
            // and `alloc_zeroed` returns either null (checked) or a suitably
            // aligned, zeroed allocation of exactly that size.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_output>();
            assert!(!ptr.is_null(), "allocation failed");
            // SAFETY: `ptr` is a fresh, exclusively-owned, zeroed allocation
            // sized for a whole `wlr_output`, so all three of these fields are
            // in bounds; each initialiser writes only the `wl_list` fields it
            // owns. The allocation does not move again, which matters because
            // `wl_signal_init` makes each list head point at itself.
            unsafe {
                sys::wlr_addon_set_init(&raw mut (*ptr).addons);
                sys::wl_signal_init(&raw mut (*ptr).events.frame);
                sys::wl_signal_init(&raw mut (*ptr).events.destroy);
            }
            Self(ptr)
        }
    }

    impl Drop for ScratchOutput {
        fn drop(&mut self) {
            // SAFETY: the addon set was initialised in `new`, so finishing it
            // undoes exactly that — and runs the id addon's destroy hook, which
            // is why every test using this type holds `id_test_lock`.
            unsafe { sys::wlr_addon_set_finish(&raw mut (*self.0).addons) };
            // SAFETY: allocated by `alloc_zeroed` with this same layout in
            // `new`, and not used again after this point.
            unsafe { dealloc(self.0.cast::<u8>(), Layout::new::<sys::wlr_output>()) };
        }
    }

    /// Whether `signal` has no listeners left.
    ///
    /// # Safety
    ///
    /// `signal` must point at an initialised `wl_signal`.
    unsafe fn signal_is_empty(signal: *mut sys::wl_signal) -> bool {
        // SAFETY: the caller guarantees the head is initialised, and a `wl_list`
        // head is empty exactly when it points at itself. Only the head is read;
        // nothing walks into a listener.
        unsafe { (*signal).listener_list.next.cast_const() == &raw const (*signal).listener_list }
    }

    /// A handler state that records what it was told, and can be asked to
    /// destroy an output from inside `new_output`.
    #[derive(Default)]
    struct Recorder {
        new_outputs: Vec<OutputId>,
        names: Vec<Option<String>>,
        frames: Vec<OutputId>,
        destroyed: Vec<OutputId>,

        /// If set, `new_output` emits this output's `destroy` signal — standing
        /// in for wlroots destroying an output from underneath a handler, which
        /// is the case deferral exists for.
        destroy_from_new_output: Option<*mut sys::wlr_output>,

        /// Whether the output's `frame` listener had already been unlinked by
        /// the time that destroy emission returned.
        frame_unlinked_during_destroy: Option<bool>,
    }

    impl OutputHandler for Recorder {
        fn new_output(&mut self, output: &Output<'_>) {
            self.new_outputs.push(output.id());
            self.names.push(output.name());

            if let Some(out) = self.destroy_from_new_output.take() {
                // SAFETY: `out` is the live `ScratchOutput` the enclosing test
                // owns, and its `destroy` signal was initialised there. wlroots
                // passes the output as the destroy signal's data, so this
                // mirrors what it does. `on_output_destroy` unlinks itself from
                // this very list, which `wl_signal_emit_mutable` tolerates.
                unsafe {
                    sys::wl_signal_emit_mutable(&raw mut (*out).events.destroy, out.cast());
                    self.frame_unlinked_during_destroy =
                        Some(signal_is_empty(&raw mut (*out).events.frame));
                }
            }
        }

        fn frame(&mut self, output: &Output<'_>) {
            self.frames.push(output.id());
        }

        fn destroyed(&mut self, id: OutputId) {
            self.destroyed.push(id);
        }
    }

    /// Drive `on_new_output` for `output` exactly as wlroots would, then hand
    /// the resulting session and the output's new id to `body`.
    ///
    /// # Safety
    ///
    /// `state` and `output` must point at a live `Recorder` and a live
    /// `wlr_output` (addon set and both signals initialised) that outlive the
    /// call, and `state` must not be aliased by any live reference.
    unsafe fn announce(
        state: *mut Recorder,
        output: *mut sys::wlr_output,
        body: impl FnOnce(&Session<'_, Recorder>, OutputId),
    ) {
        // SAFETY: the caller's guarantees are what `Dispatcher::emit` and
        // `Registration::link` require. `session` is a local that never moves
        // after its address is taken, and the harness signal and its flag both
        // outlive the registration below. `Session<'_, Recorder>` is not
        // reachable from `Recorder`, so no reference to it can be live across
        // `emit`.
        unsafe {
            let runtime = Runtime::new().expect("runtime");
            let session = Session {
                dispatcher: Dispatcher::new(state),
                outputs: RefCell::new(HashMap::new()),
                toplevels: RefCell::new(HashMap::new()),
                inputs: RefCell::new(Vec::new()),
                last_key_consumed: Cell::new(false),
                runtime: &runtime,
                deliver: deliver::<Recorder>,
            };
            let mut h = Harness::new();
            let hp = &raw mut *h;

            // Declared after `session`, so it unlinks while the session it
            // names is still alive — the ordering `run` uses.
            let _reg = Registration::link(
                &raw mut (*hp).signal,
                on_new_output::<Recorder>,
                (&raw const session).cast::<()>(),
                &raw const (*hp).alive,
                None,
                None,
            );

            sys::wl_signal_emit_mutable(&raw mut (*hp).signal, output.cast());

            let id = find_id(&raw const (*output).addons)
                .map(OutputId)
                .expect("on_new_output must attach an id before announcing");
            body(&session, id);
        }
    }

    /// The announcement path end to end: an id is attached, the output is
    /// registered, both per-output listeners are linked, and the handler gets a
    /// working handle rather than the event being dropped.
    #[test]
    fn announcing_an_output_registers_it_and_delivers_a_handle() {
        let _serialised = crate::id::id_test_lock();

        let out = ScratchOutput::new();
        let mut state = Recorder::default();
        // One pointer, derived once and used throughout, so the intrusive
        // structures below keep a single provenance; see `dispatch::tests`.
        let p = &raw mut state;

        // SAFETY: `p` and `out.0` are live for the whole call and `state` is
        // reached only through `p` from here on.
        unsafe {
            announce(p, out.0, |session, id| {
                assert_eq!(
                    (*p).new_outputs,
                    vec![id],
                    "the handler must be told about the output, with the id \
                     that was attached to it"
                );
                assert_eq!(
                    (*p).names,
                    vec![None],
                    "the handle must be usable, not merely present"
                );
                assert_eq!(
                    session.outputs.borrow().get(&id).map(|entry| entry.raw),
                    Some(out.0),
                    "the id must resolve back to this output, or nothing \
                     deferred could ever be delivered"
                );
                assert!(
                    !signal_is_empty(&raw mut (*out.0).events.frame),
                    "the frame listener must be linked"
                );
                assert!(
                    !signal_is_empty(&raw mut (*out.0).events.destroy),
                    "the destroy listener must be linked"
                );
            });
        }
    }

    /// The frame path. wlroots' `frame` signal carries no data, so this also
    /// pins that the id comes from the listener rather than from the callback's
    /// arguments.
    #[test]
    fn a_frame_signal_reaches_the_frame_handler() {
        let _serialised = crate::id::id_test_lock();

        let out = ScratchOutput::new();
        let mut state = Recorder::default();
        let p = &raw mut state;

        // SAFETY: as above; the frame signal was initialised by `ScratchOutput`
        // and its only listener is the one `on_new_output` linked.
        unsafe {
            announce(p, out.0, |_session, id| {
                sys::wl_signal_emit_mutable(&raw mut (*out.0).events.frame, std::ptr::null_mut());
                assert_eq!(
                    (*p).frames,
                    vec![id],
                    "a frame must reach the handler, naming the output it is for"
                );
            });
        }
    }

    /// The property the whole deferral design rests on: an output is forgotten
    /// *during* its destroy emission, not when the resulting event is
    /// delivered.
    ///
    /// The two are indistinguishable when nothing is deferred, so this destroys
    /// the output from inside `new_output` — exactly the reentrant case — which
    /// queues `OutputDestroyed` behind the running handler. wlroots frees the
    /// output as soon as the emission returns, long before that queue drains,
    /// so an implementation that unregistered at delivery time would hold a
    /// pointer to freed memory in between. `frame_unlinked_during_destroy` is
    /// what tells the two apart: it reads the output's `frame` listener list
    /// the instant the emission returns, and it can only be empty if the
    /// registry entry — which owns that listener — was already gone.
    #[test]
    fn a_destroyed_output_is_forgotten_before_the_handler_is_told() {
        let _serialised = crate::id::id_test_lock();

        let out = ScratchOutput::new();
        let mut state = Recorder::default();
        let p = &raw mut state;

        // SAFETY: as above. The write goes through `p` rather than through
        // `state` so that deriving `p` stays the last thing to touch `state`.
        unsafe {
            (*p).destroy_from_new_output = Some(out.0);

            announce(p, out.0, |session, id| {
                assert_eq!(
                    (*p).frame_unlinked_during_destroy,
                    Some(true),
                    "the output must be unregistered synchronously, inside its \
                     destroy emission — wlroots frees it as soon as that \
                     returns"
                );
                assert!(
                    session.outputs.borrow().is_empty(),
                    "and the registry must be left with nothing to resolve"
                );
                assert_eq!(
                    (*p).destroyed,
                    vec![id],
                    "the handler is still told, by id, once the outer handler \
                     has returned"
                );
                assert_eq!(
                    (*p).new_outputs,
                    vec![id],
                    "and the announcement it was destroyed from arrived first"
                );
                assert!(
                    signal_is_empty(&raw mut (*out.0).events.destroy),
                    "the destroy listener must unlink itself too, since \
                     wlroots is about to free the list it is in"
                );
            });
        }
    }

    /// The other half of the same property: once the entry is gone, an event
    /// still naming it resolves to nothing and is dropped, rather than
    /// dereferencing a freed output or panicking out of a dispatch.
    #[test]
    fn an_event_for_an_unknown_output_is_dropped_rather_than_delivered() {
        let mut state = Recorder::default();
        let ghost = OutputId(u64::MAX);
        let runtime = Runtime::new().expect("runtime");
        let session: Session<'_, Recorder> = Session {
            // Never dereferenced: these tests call `deliver` directly and pass
            // the state alongside, rather than going through `emit`.
            dispatcher: Dispatcher::new(std::ptr::null_mut()),
            outputs: RefCell::new(HashMap::new()),
            toplevels: RefCell::new(HashMap::new()),
            inputs: RefCell::new(Vec::new()),
            last_key_consumed: Cell::new(false),
            runtime: &runtime,
            deliver: deliver::<Recorder>,
        };

        deliver(&session, &mut state, Event::NewOutput(ghost));
        deliver(&session, &mut state, Event::OutputFrame(ghost));
        assert!(
            state.new_outputs.is_empty() && state.frames.is_empty(),
            "an event naming an output the registry has forgotten must be \
             dropped, since there is no object left to borrow"
        );

        deliver(&session, &mut state, Event::OutputDestroyed(ghost));
        assert_eq!(
            state.destroyed,
            vec![ghost],
            "destruction is the one event that needs no object, so it is \
             delivered even though the lookup would miss"
        );
    }
}
