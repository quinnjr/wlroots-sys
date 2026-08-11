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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;

use crate::dispatch::{Dispatcher, Event};
use crate::id::{attach_id, find_id};
use crate::{Error, EventLoop, Output, OutputHandler, OutputId, Result, sys};

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

    /// The display that loop belongs to.
    ///
    /// Carried alongside `loop_` because [`EventLoop`] on this branch is the
    /// pair of them (see its own doc comment), so reconstructing one in
    /// [`Backend::run`] needs both. Storing only `loop_` and passing a
    /// placeholder display would build an `EventLoop` whose `display` names
    /// nothing — inert today, since `run` only dispatches, but a latent
    /// use-after-free the moment anything reached for it.
    ///
    /// Declared after `loop_`, and after `alive`, so the field-order rule the
    /// destroy watch depends on above is untouched: neither this field nor
    /// `loop_` has drop glue at all.
    display: NonNull<sys::wl_display>,

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
struct Session<S> {
    dispatcher: Dispatcher<S>,
    outputs: RefCell<HashMap<OutputId, OutputEntry>>,
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
        // wlroots 0.15 takes the *display* here, and has no session
        // out-parameter at all — `wlr_session` is created and owned internally
        // by whichever backend needs one, and there is nothing to opt out of.
        // The newer lines pass the event loop and a null session pointer
        // instead. The signature above is identical across all of them because
        // `EventLoop` privately carries its display on this branch; see its own
        // doc comment for why that is the only place the pointer can come from.
        //
        // SAFETY: the borrow guarantees the loop — and so the display it was
        // derived from, which outlives it — is live. `display_ptr` returns the
        // display `Display::event_loop` took this loop from, so wlroots gets
        // the display its own loop belongs to rather than an unrelated one.
        let raw = unsafe { sys::wlr_backend_autocreate(loop_.display_ptr()) };
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
            )
        };

        Ok(Backend {
            raw,
            _death_watch: death_watch,
            alive,
            started: Cell::new(false),
            // Kept from the loop we were created on, rather than asked for
            // again at `run` time — see the field's own comment. Both halves
            // come from the same `EventLoop`, so they cannot disagree.
            loop_: loop_.as_non_null(),
            display: loop_.display_non_null(),
            _loop: PhantomData,
        })
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
        alive_or_err(&self.alive)?;
        // Taken before anything else is built, so a refused re-entry perturbs
        // nothing at all — in particular it does not register a listener or
        // start the backend on its way to returning the error.
        let _reentry = ReentryGuard::acquire()?;

        // `state` is consumed into a raw pointer here and never touched as a
        // reference again for the rest of this function, so no `&mut S` is live
        // while a callback delivers through the dispatcher.
        let session = Session {
            dispatcher: Dispatcher::new(&raw mut *state),
            outputs: RefCell::new(HashMap::new()),
        };

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
        // `Session<S>` paired with it here. `session` is a local that is never
        // moved after this point, so the address stays valid for the call.
        let _new_output = unsafe {
            Registration::link(
                &raw mut (*self.raw.as_ptr()).events.new_output,
                on_new_output::<S>,
                (&raw const session).cast::<()>(),
                &raw const *self.alive,
                None,
            )
        };

        // Only now, with the listener in place. `wlr_backend_start` announces
        // the backend's existing outputs synchronously, so starting before this
        // point would emit them into an empty signal; see `ensure_started`.
        self.ensure_started()?;

        // SAFETY: `self.loop_` and `self.display` were taken together from the
        // one `EventLoop<'d>` handed to `autocreate`, so they name a live loop
        // and the live display it belongs to — not merely two live pointers —
        // and that display outlives `'d`, which `&self` keeps borrowed for
        // this call. Reconstructing the pair rather than only the loop is what
        // keeps the rebuilt `EventLoop` honest on this branch, where the
        // display is part of the value.
        //
        // Dispatching here is not the re-entry `EventLoop::dispatch` refuses:
        // `run` is not a handler, and the flag that refusal consults is set
        // only for the duration of a delivery, strictly inside the
        // `loop_.dispatch(0)` call below rather than around it.
        let loop_ = unsafe { EventLoop::from_raw(self.loop_, self.display) };
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
    //
    // `attach_id` re-walks the addon list itself, via its own `assert!`
    // (deliberately not `debug_assert!`, since it is a safety net on a
    // `pub(crate)` unsafe fn, not merely a debugging aid), so the `None` arm
    // below costs two walks rather than one. Left as-is rather than factored
    // into an unchecked variant: this is the output-announcement path, not
    // the frame path, so the cost is small and bounded by output count, and
    // it is not worth a second entry point to `attach_id` for it.
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
        let session = (*bound).session.cast::<Session<S>>();
        let output = data.cast::<sys::wlr_output>();

        // Give the output an identity before anyone can ask for one.
        let id = ensure_id(&raw mut (*output).addons);

        // `(*bound).session` is forwarded verbatim rather than re-derived, so
        // these two callbacks are instantiated at the `S` that pointer already
        // belongs to — the pairing `Bound::session` documents.
        let frame = Registration::link(
            &raw mut (*output).events.frame,
            on_frame::<S>,
            (*bound).session,
            std::ptr::null(),
            Some(id),
        );
        let destroy = Registration::link(
            &raw mut (*output).events.destroy,
            on_output_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
            Some(id),
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

        (*session)
            .dispatcher
            .emit(&*session, Event::NewOutput(id), deliver::<S>);
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
        let session = (*bound).session.cast::<Session<S>>();
        // Cannot be `None`: `on_new_output` is the only site that installs this
        // callback and it always supplies an id. Handled rather than unwrapped
        // because this is an `extern "C"` frame, where a panic aborts.
        let Some(id) = (*bound).id else { return };

        (*session)
            .dispatcher
            .emit(&*session, Event::OutputFrame(id), deliver::<S>);
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
        let session = (*bound).session.cast::<Session<S>>();
        let Some(id) = (*bound).id else { return };

        // Both values this frame still needs have been copied out of `*bound`
        // above, because the next statement frees it: removing the entry drops
        // its two registrations, one of which owns this very `Bound`.
        //
        // Unlinking the currently-firing listener is safe under the emit this
        // branch has. wlroots 0.15 predates `wl_signal_emit_mutable` — it was
        // added in libwayland 1.22 and 0.15 targets 1.20, so `wlr-sys` on this
        // branch re-exports `wl_signal_emit` instead (see its `signal` module).
        // That one walks the list with `list_for_each_safe`, advancing its
        // cursor past this listener *before* calling us, so a handler removing
        // *itself* is exactly the case it survives. What it does not survive is
        // a handler removing the listener after it in the list, and nothing
        // here does: the other registration this drop unlinks lives in the
        // output's `frame` signal, a different list entirely, and this crate
        // never links two listeners into one signal.
        //
        // `bound` is dangling from here on and is not touched again.
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

        (*session)
            .dispatcher
            .emit(&*session, Event::OutputDestroyed(id), deliver::<S>);
    }
}

/// Borrow the output `id` names, if this session still knows of one.
///
/// The registry borrow is released before `f` runs: a handler can re-enter
/// wlroots (committing an output, say), which can fire a signal, which can take
/// the borrow mutably.
fn with_output<S>(session: &Session<S>, id: OutputId, f: impl FnOnce(&Output<'_>)) {
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
fn deliver<S: OutputHandler>(session: &Session<S>, state: &mut S, ev: Event) {
    match ev {
        Event::NewOutput(id) => with_output(session, id, |output| state.new_output(output)),
        Event::OutputFrame(id) => with_output(session, id, |output| state.frame(output)),
        // No resolution to do, and nothing to drop the event for: the id
        // outlives the object on purpose, and the handler is told about the
        // destruction even though nothing is left to hand it.
        Event::OutputDestroyed(id) => state.destroyed(id),
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
            let _reg = unsafe { Registration::link(signal, noop, std::ptr::null(), alive, None) };
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
        // and `wl_signal_emit` tolerates a handler unlinking itself — this one
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
            );
            assert!((*hp).alive.get(), "the flag starts set");

            sys::wl_signal_emit(&raw mut (*hp).signal, std::ptr::null_mut());

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
                // this very list, which `wl_signal_emit` tolerates for a
                // handler unlinking itself; see `on_output_destroy`.
                unsafe {
                    sys::wl_signal_emit(&raw mut (*out).events.destroy, out.cast());
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
        body: impl FnOnce(&Session<Recorder>, OutputId),
    ) {
        // SAFETY: the caller's guarantees are what `Dispatcher::emit` and
        // `Registration::link` require. `session` is a local that never moves
        // after its address is taken, and the harness signal and its flag both
        // outlive the registration below. `Session<Recorder>` is not reachable
        // from `Recorder`, so no reference to it can be live across `emit`.
        unsafe {
            let session = Session {
                dispatcher: Dispatcher::new(state),
                outputs: RefCell::new(HashMap::new()),
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
            );

            sys::wl_signal_emit(&raw mut (*hp).signal, output.cast());

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
                sys::wl_signal_emit(&raw mut (*out.0).events.frame, std::ptr::null_mut());
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
        let session: Session<Recorder> = Session {
            // Never dereferenced: these tests call `deliver` directly and pass
            // the state alongside, rather than going through `emit`.
            dispatcher: Dispatcher::new(std::ptr::null_mut()),
            outputs: RefCell::new(HashMap::new()),
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
