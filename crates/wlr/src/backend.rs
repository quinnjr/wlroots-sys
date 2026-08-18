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

use crate::dispatch::{Dispatcher, DisplayPinGuard, Event};
use crate::id::{SourceId, attach_id, find_id};
use crate::layer::Layer;
use crate::seat::{KeyEvent, Modifiers};
use crate::{
    Band, Display, Error, EventLoop, Handlers, LayerSurface, LayerSurfaceId, LoopHandler, Output,
    OutputHandler, OutputId, Result, Runtime, Toplevel, ToplevelId, sys,
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
/// `touch = 4` — see this constant group's own doc above. There is no
/// physical-touch-device wiring in this crate (`on_new_input` has no
/// `WLR_INPUT_DEVICE_TOUCH` arm), so this bit is only ever set through
/// [`Runtime::enable_test_touch`], not by any real device arriving.
const WL_SEAT_CAPABILITY_TOUCH: u32 = 4;

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

    /// The toplevel this listener belongs to, for the nine per-toplevel
    /// listeners `on_new_toplevel` links plus the two per-decoration
    /// listeners `on_new_toplevel_decoration` links (keyed by the toplevel
    /// the decoration was created for, not by any id of the decoration's
    /// own — this crate mints no such id); `None` for every other listener
    /// in this file.
    ///
    /// This is what makes `on_toplevel_map`, `on_toplevel_unmap`,
    /// `on_toplevel_set_title` and `on_toplevel_destroy` sound at all: wlroots
    /// 0.20 emits `wlr_surface.events.map`/`.unmap` and
    /// `wlr_xdg_toplevel.events.set_title`/`.destroy` with a **null** `data`
    /// argument (confirmed against the C sources — `wlr_compositor.c` and
    /// `wlr_xdg_toplevel.c`), so a callback that read the id out of `data`
    /// would dereference a null pointer on the first real client. The two
    /// decoration listeners follow the identical discipline for a different
    /// reason: `on_toplevel_decoration_destroy` can fire after wlroots has
    /// already cleared `wlr_xdg_toplevel_decoration_v1::toplevel` to null
    /// (when the toplevel died first), so re-deriving the id from the
    /// decoration itself would be unsound exactly when it matters most.
    /// `Bound` is private to this module, so widening it with a second id
    /// field costs nothing outside it — unlike `Bound::id`, which stays
    /// `Option<OutputId>` rather than being generalised, because every
    /// output-side call site still wants that exact type and a shared enum
    /// would cost every one of them a match arm for a case that can never
    /// apply to it.
    toplevel: Option<ToplevelId>,

    /// A `Cell<bool>` the callback sets when the signal fires, or null.
    ///
    /// Separate from `alive`, which is read by [`Registration::drop`] and is
    /// about the *signal owner*'s liveness. This one is written by
    /// [`on_set_flag`] and is about the signal having fired at all — the shape
    /// `render::Renderer` needs for `events.lost`, where the object stays
    /// alive and only becomes unusable. Reusing `alive` for it would invert
    /// the meaning `Registration::drop` reads out of that field.
    flag: *const Cell<bool>,

    /// The layer surface this listener belongs to, for the four per-layer-
    /// surface listeners `on_new_layer_surface` links (commit/map/unmap on
    /// the base `wlr_surface`, destroy on the layer surface's own
    /// `events.destroy`); `None` for every other listener in this file.
    ///
    /// A fourth id field alongside `id`/`toplevel` rather than a shared enum,
    /// for the identical reason `toplevel`'s own doc gives for not folding
    /// into `id`: `Bound` is private to this module, so widening it costs
    /// nothing outside it, and every other call site wants its own exact
    /// type. wlroots emits `wlr_surface.events.map`/`.unmap` with a null
    /// `data` for a layer surface's base surface too — the same discipline
    /// `toplevel`'s doc documents for `on_toplevel_map`/`on_toplevel_unmap`
    /// applies verbatim here, which is why `on_layer_surface_map`/`_unmap`
    /// read this field rather than `data`.
    layer: Option<LayerSurfaceId>,
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
pub(crate) struct Registration {
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
    // Eight parameters, and every one of them is a slot on `Bound` this
    // constructor exists to fill. The five wrappers below are what call sites
    // actually use; collapsing the slots into a struct would only move the
    // same eight values one line up, and none of them has a meaningful
    // default.
    #[allow(clippy::too_many_arguments)]
    unsafe fn link(
        signal: *mut sys::wl_signal,
        notify: sys::wl_notify_func_t,
        session: *const (),
        alive: *const Cell<bool>,
        flag: *const Cell<bool>,
        id: Option<OutputId>,
        toplevel: Option<ToplevelId>,
        layer: Option<LayerSurfaceId>,
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
            flag,
            id,
            toplevel,
            layer,
        });

        // SAFETY: the caller guarantees `signal` is an initialised `wl_signal`,
        // and the listener is a freshly boxed one that nothing else has linked
        // anywhere. Its address stays put until this `Registration` drops,
        // which unlinks it before the box is freed.
        unsafe { sys::wl_signal_add(signal, &raw mut bound.listener) };

        Registration { bound }
    }

    /// The address of this registration's linked `wl_listener`, as `usize`.
    ///
    /// Stable for the life of the `Registration` — the listener lives in a
    /// boxed [`Bound`] whose address `link` fixed — and equal to the `l`
    /// pointer wlroots hands the notify callback. A map keyed by this can
    /// therefore be looked up from inside the callback even on the destroy
    /// signals wlroots emits with a **null** `data` (the session lock and its
    /// surfaces both do; see `on_session_lock_surface_destroy`), where the
    /// object pointer that would otherwise be the key is unavailable.
    fn listener_addr(&self) -> usize {
        &self.bound.listener as *const sys::wl_listener as usize
    }

    /// Link a listener that routes no per-entity event: the backend-level
    /// signals and the per-input-device ones, which resolve their target some
    /// way other than a `Bound` id field (or route nothing at all, like the
    /// destroy watch). Fixes all three id slots to `None` so no call site has
    /// to spell the empty slots out.
    ///
    /// # Safety
    ///
    /// As for [`Registration::link`], to which every argument is forwarded
    /// verbatim.
    unsafe fn link_bare(
        signal: *mut sys::wl_signal,
        notify: sys::wl_notify_func_t,
        session: *const (),
        alive: *const Cell<bool>,
    ) -> Self {
        // SAFETY: forwarded verbatim; the caller upholds `link`'s contract.
        unsafe {
            Self::link(
                signal,
                notify,
                session,
                alive,
                std::ptr::null(),
                None,
                None,
                None,
            )
        }
    }

    /// Link a listener that does nothing but set `flag` when the signal fires.
    ///
    /// The shape [`crate::Renderer`] needs for `events.lost`: the renderer stays
    /// alive after a GPU reset and only becomes unusable, so there is no
    /// liveness flag to clear and no event to route — a consumer asks
    /// [`Renderer::is_lost`](crate::Renderer::is_lost) afterwards.
    ///
    /// `alive` is left null, which is the *stronger* claim (see
    /// [`Registration::drop`]): every caller of this must unlink the
    /// registration while the signal's owner is still alive.
    ///
    /// # Safety
    ///
    /// * `signal` must point at an initialised `wl_signal` whose owner outlives
    ///   the returned `Registration`.
    /// * `flag` must outlive the returned `Registration`, and must be reachable
    ///   only from the event loop's own thread.
    pub(crate) unsafe fn link_flag(signal: *mut sys::wl_signal, flag: *const Cell<bool>) -> Self {
        // SAFETY: forwarded verbatim; the caller upholds `link`'s contract.
        // `on_set_flag` reads neither `session` nor any id slot.
        unsafe {
            Self::link(
                signal,
                on_set_flag,
                std::ptr::null(),
                std::ptr::null(),
                flag,
                None,
                None,
                None,
            )
        }
    }

    /// Link a per-output listener, carrying the [`OutputId`] its callback reads
    /// back from [`Bound::id`]. Fixes the toplevel and layer slots to `None`.
    ///
    /// # Safety
    ///
    /// As for [`Registration::link`], to which the shared arguments are
    /// forwarded verbatim.
    unsafe fn link_output(
        signal: *mut sys::wl_signal,
        notify: sys::wl_notify_func_t,
        session: *const (),
        alive: *const Cell<bool>,
        id: OutputId,
    ) -> Self {
        // SAFETY: forwarded verbatim; the caller upholds `link`'s contract.
        unsafe {
            Self::link(
                signal,
                notify,
                session,
                alive,
                std::ptr::null(),
                Some(id),
                None,
                None,
            )
        }
    }

    /// Link a per-toplevel (or per-decoration) listener, carrying the
    /// [`ToplevelId`] its callback reads back from [`Bound::toplevel`]. Fixes
    /// the output and layer slots to `None`.
    ///
    /// # Safety
    ///
    /// As for [`Registration::link`], to which the shared arguments are
    /// forwarded verbatim.
    unsafe fn link_toplevel(
        signal: *mut sys::wl_signal,
        notify: sys::wl_notify_func_t,
        session: *const (),
        alive: *const Cell<bool>,
        toplevel: ToplevelId,
    ) -> Self {
        // SAFETY: forwarded verbatim; the caller upholds `link`'s contract.
        unsafe {
            Self::link(
                signal,
                notify,
                session,
                alive,
                std::ptr::null(),
                None,
                Some(toplevel),
                None,
            )
        }
    }

    /// Link a per-layer-surface listener, carrying the [`LayerSurfaceId`] its
    /// callback reads back from [`Bound::layer`]. Fixes the output and toplevel
    /// slots to `None`.
    ///
    /// # Safety
    ///
    /// As for [`Registration::link`], to which the shared arguments are
    /// forwarded verbatim.
    unsafe fn link_layer(
        signal: *mut sys::wl_signal,
        notify: sys::wl_notify_func_t,
        session: *const (),
        alive: *const Cell<bool>,
        layer: LayerSurfaceId,
    ) -> Self {
        // SAFETY: forwarded verbatim; the caller upholds `link`'s contract.
        unsafe {
            Self::link(
                signal,
                notify,
                session,
                alive,
                std::ptr::null(),
                None,
                None,
                Some(layer),
            )
        }
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

    /// This run's listeners on every live decoration object, keyed by the
    /// toplevel it belongs to. Removed, and so unlinked, from
    /// `on_toplevel_decoration_destroy` when the decoration dies, and from
    /// `on_toplevel_destroy` when its toplevel dies first — mirroring
    /// `toplevels`' own destroy-triggered removal, but from two independent
    /// sites instead of one; see `RuntimeInner::decorations`'s own doc for
    /// why both exist.
    decorations: RefCell<HashMap<ToplevelId, DecorationListeners>>,

    /// This run's listeners on every live layer surface. Removed, and so
    /// unlinked, from `on_layer_surface_destroy` — before the layer surface
    /// is freed, mirroring `toplevels` above.
    layers: RefCell<HashMap<LayerSurfaceId, LayerSurfaceListeners>>,

    /// This run's listeners on every live input device, one [`InputDevice`]
    /// per device, keyed by the device's own `*mut wlr_input_device` address
    /// (as `usize`) — unlike the destroy-only removal `outputs` and
    /// `toplevels` do at the *end* of a run, `on_input_destroy` must find
    /// and remove one specific device's entry synchronously, mid-run, the
    /// moment that device is unplugged (see [`InputDevice`]'s own doc for
    /// why), and the device pointer is exactly the key
    /// `wlr_input_device_finish` hands that callback.
    inputs: RefCell<HashMap<usize, InputDevice>>,

    /// This run's destroy listener on every drag icon currently visible,
    /// keyed by the drag icon's own `*mut wlr_drag_icon` address (as
    /// `usize`) — mirrors `inputs`' keying discipline exactly, and for the
    /// same reason: `on_start_drag`'s destroy listener must find and remove
    /// one specific icon's entry synchronously, the moment that icon is
    /// destroyed, so it can clear [`RuntimeInner::drag_icon_tree`] before
    /// wlroots frees the scene tree stored there. Keyed by the *icon*, not
    /// the drag: wlroots frees the tree from two independent paths — the
    /// drag ending normally, and a client destroying the drag-icon surface
    /// mid-drag while the drag itself survives — and `(*icon).events.destroy`
    /// is the one signal both paths fire, always in the same synchronous
    /// emission that frees the tree; `(*drag).events.destroy` does not fire
    /// on the second path at all. A drag with no icon never gets an entry
    /// here — `on_start_drag` returns before linking anything when
    /// `(*drag).icon` is null.
    drags: RefCell<HashMap<usize, Registration>>,

    /// This run's destroy listener on every idle inhibitor currently live,
    /// keyed by the `destroy` listener's own address (as `usize`) — NOT the
    /// inhibitor object's, because wlroots emits the inhibitor's `destroy`
    /// signal with its **surface** as `data` (not the inhibitor), so the
    /// handler recovers its key from the firing `wl_listener` `l` instead. The
    /// map still serves `drags`' role: `on_idle_inhibitor_destroy` must find
    /// and remove one specific inhibitor's entry synchronously, the moment that
    /// inhibitor is destroyed, without touching any other live inhibitor's
    /// listener.
    idle_inhibitors: RefCell<HashMap<usize, Registration>>,

    /// This run's `new_surface`/`unlock`/`destroy` listeners on every live
    /// `wlr_session_lock_v1`, keyed by the `destroy` listener's own address
    /// (as `usize`) — NOT the lock object's, because wlroots emits the
    /// session-lock `destroy` signal with a null `data`, so the handler
    /// recovers its key from the firing `wl_listener` `l` instead.
    /// `on_session_lock_destroy` must find and remove one specific
    /// lock's listeners synchronously the moment that lock is destroyed, so
    /// the three-listener bundle is dropped (and unlinked) together. At most
    /// one entry is normally live — wlroots serialises locks — but the map,
    /// not a single slot, is what lets a lock's destroy remove exactly its own
    /// bundle by key without assuming which one it is.
    session_locks: RefCell<HashMap<usize, SessionLockListeners>>,

    /// This run's `commit`/`destroy` listeners on every live
    /// `wlr_session_lock_surface_v1`, keyed by the `destroy` listener's own
    /// address (as `usize`) — NOT the lock-surface object's, because wlroots
    /// emits the `destroy` signal with a null `data`; the handler recovers its
    /// key from the firing `wl_listener` and the covered output from the
    /// stored entry. The commit listener
    /// drives the "all outputs covered → send `locked` once" decision; the
    /// destroy listener drops the surface's scene tree from
    /// [`RuntimeInner::lock_surface_trees`](crate::runtime::RuntimeInner)
    /// before wlroots frees it. Removed — and so unlinked — from
    /// `on_session_lock_surface_destroy`, mirroring `drags`.
    lock_surfaces: RefCell<HashMap<usize, LockSurfaceListeners>>,

    /// This run's `set_region`/`destroy` listeners on every live
    /// `wlr_pointer_constraint_v1`, keyed by the `destroy` listener's own
    /// address (as `usize`). wlroots does emit the constraint's `destroy`
    /// signal with the constraint itself as `data` (unlike the null-`data`
    /// session-lock/lock-surface signals and the surface-`data` idle-inhibitor
    /// one), but this map keys on the firing listener regardless — the same
    /// discipline every other destroy-keyed table here uses, so identity never
    /// depends on what a given signal happens to pass. `on_pointer_constraint_destroy`
    /// removes one specific constraint's entry synchronously the moment that
    /// constraint is destroyed, dropping the two-listener bundle (and unlinking
    /// it) together, and clears [`RuntimeInner::active_constraint`] if the
    /// entry it removed was the active one.
    pointer_constraints: RefCell<HashMap<usize, PointerConstraintListeners>>,

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

/// One live toplevel's listeners: the surface's commit/map/unmap, the
/// toplevel's own set_title/destroy, and its four client-request signals
/// (maximize/fullscreen/move/resize). Field order is not load-bearing here —
/// unlike [`OutputEntry`], nothing here owns the `Bound` any of the others
/// recover their session from — but all nine must drop, and so unlink, as
/// part of removing the entry, which happens while the toplevel is still
/// alive.
struct ToplevelListeners {
    _commit: Registration,
    _map: Registration,
    _unmap: Registration,
    _set_title: Registration,
    _destroy: Registration,
    _request_maximize: Registration,
    _request_fullscreen: Registration,
    _request_move: Registration,
    _request_resize: Registration,
}

/// One live decoration object's two listeners: its own `request_mode` and
/// `destroy`. Field order is not load-bearing, as for `ToplevelListeners`.
struct DecorationListeners {
    _request_mode: Registration,
    _destroy: Registration,
}

/// One live layer surface's listeners: the base surface's commit/map/unmap,
/// and the layer surface's own `destroy`. Field order is not load-bearing,
/// as for [`ToplevelListeners`].
struct LayerSurfaceListeners {
    _commit: Registration,
    _map: Registration,
    _unmap: Registration,
    _destroy: Registration,
}

/// One live `wlr_session_lock_v1`'s three listeners: `new_surface`, `unlock`,
/// and the lock's own `destroy`. Field order is not load-bearing, as for
/// [`ToplevelListeners`] — all three must drop, and so unlink, as part of
/// removing the entry, which happens while the lock is still alive (its own
/// `destroy` emission removes it, after `wl_signal_emit_mutable` has already
/// advanced past the firing listener).
struct SessionLockListeners {
    _new_surface: Registration,
    _unlock: Registration,
    _destroy: Registration,
}

/// One live `wlr_session_lock_surface_v1`'s listeners: the base surface's
/// `commit` (which drives the send-`locked` decision) and the lock surface's
/// own `destroy` (which drops its scene tree from the runtime before wlroots
/// frees it). Field order is not load-bearing, as for [`ToplevelListeners`].
/// One live `wlr_pointer_constraint_v1`'s listeners: its own `set_region` and
/// `destroy`. Field order is not load-bearing, as for [`ToplevelListeners`] —
/// both must drop, and so unlink, as part of removing the entry, which happens
/// while the constraint is still alive (its own `destroy` emission removes it,
/// after `wl_signal_emit_mutable` has advanced past the firing listener).
struct PointerConstraintListeners {
    _set_region: Registration,
    _destroy: Registration,
    /// The constraint this entry's listeners are linked on, as a raw pointer,
    /// so `on_pointer_constraint_destroy` can tell whether the constraint it is
    /// tearing down is the one recorded in
    /// [`RuntimeInner::active_constraint`](crate::runtime::RuntimeInner) and
    /// clear it. Captured at link time rather than read from the destroy
    /// signal's `data`, keeping identity independent of the signal — the same
    /// reason `LockSurfaceListeners` captures its `output`.
    constraint: NonNull<sys::wlr_pointer_constraint_v1>,
}

struct LockSurfaceListeners {
    _commit: Registration,
    _destroy: Registration,
    /// The `wlr_output` this lock surface covered, as `usize`, so
    /// `on_session_lock_surface_destroy` can drop the surface's tree from
    /// [`RuntimeInner::lock_surface_trees`](crate::runtime::RuntimeInner)
    /// (which is keyed by output). wlroots emits the lock surface's `destroy`
    /// signal with a **null** `data`, so the output cannot be recovered from
    /// the signal at destroy time — it is captured here at link time instead,
    /// the same discipline [`Bound::layer`] uses for that surface's own
    /// null-`data` events.
    output: usize,
}

/// Clears `Runtime`'s layer-surface table when the `run_inner` call holding
/// this guard returns, on every exit path. Mirrors [`ToplevelTableGuard`]
/// exactly, for the identical reason: see
/// `Runtime::clear_layer_surfaces`'s own doc.
struct LayerSurfaceTableGuard<'r>(&'r Runtime);

impl Drop for LayerSurfaceTableGuard<'_> {
    fn drop(&mut self) {
        self.0.clear_layer_surfaces();
    }
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

/// Clears `Runtime`'s `outputs` table when the `run_inner` call holding this
/// guard returns, on every exit path. Mirrors [`ToplevelTableGuard`] exactly,
/// for the identical reason: see `Runtime::clear_outputs`'s own doc.
struct OutputTableGuard<'r>(&'r Runtime);

impl Drop for OutputTableGuard<'_> {
    fn drop(&mut self) {
        self.0.clear_outputs();
    }
}

/// Clears `Runtime`'s `live_sources` table when the `run_inner` call holding
/// this guard returns, on every exit path — the same "declared once,
/// registered and torn down per run" rule [`ToplevelTableGuard`] enforces
/// for toplevels. The sources themselves are already removed from the event
/// loop by this point, each by its own [`FdRegistration`]'s `Drop`; this
/// only stops the table naming a `SourceId` whose registration is gone,
/// which would otherwise let a later [`Runtime::remove_fd`] attempt a
/// second `wl_event_source_remove` on it.
struct LiveSourcesGuard<'r>(&'r Runtime);

impl Drop for LiveSourcesGuard<'_> {
    fn drop(&mut self) {
        self.0.clear_live_sources();
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
            Registration::link_bare(
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

    /// The liveness gate, for in-crate callers outside this module that take a
    /// `&Backend` from a consumer at an arbitrary moment —
    /// [`Renderer::autocreate`](crate::Renderer::autocreate) is one, and unlike
    /// `init_graphics` it can be called after a DRM unplug has already killed
    /// the backend.
    pub(crate) fn alive_or_err(&self) -> Result<()> {
        alive_or_err(&self.alive)
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
    /// Prefer [`Backend::run_all`] for a real compositor: `run` never installs
    /// fd sources, the xdg-shell, or the seat, and does not flush clients.
    /// `run` remains the minimal output-only path and is kept forever.
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

        // Debug-only Display-identity bug detector, covering the listener-
        // linking hazard `Runtime::create_xdg_shell`'s own doc names: this
        // call is about to have `run_inner` link `runtime`'s cached xdg-shell/
        // decoration/layer-shell listeners into `display`'s `wl_list`s, and if
        // `runtime` was pinned (by `init_graphics`) to a *different* `Display`,
        // those lists belong to a freed display — a use-after-free with no
        // recovery path. `display` is the authoritative ground truth here (as
        // it is in `init_graphics`), so this compares against it directly
        // rather than against `dispatch::current_display()`, which is not yet
        // set to this call's display at run_all entry. Skipped when
        // `pinned_display` is `0` — `init_graphics` has not run yet (a
        // compositor is free to defer it into a `NewOutput` handler), so there
        // is nothing recorded to compare against — the same "0 means no ground
        // truth, skip rather than treat as agreement" rule the mutator sites
        // apply to `current_display()`. `debug_assert_eq!` rather than a real
        // check, for the reason `RuntimeInner::pinned_display`'s own doc gives.
        let pinned = runtime.inner.pinned_display.get();
        if pinned != 0 {
            debug_assert_eq!(
                pinned,
                display.as_ptr() as usize,
                "Backend::run_all is driving a Runtime pinned (by init_graphics) \
                 to a different Display — its cached shell/decoration/layer-shell \
                 listeners would be linked into a freed display's wl_lists"
            );
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

        // Pins `Runtime`'s Display-identity assert (see `Runtime`'s own doc
        // on the pin) to `display`'s pointer for the life of this call —
        // `None` for `run` (`display` is `None` there; see `DisplayPinGuard`'s
        // own doc). Declared before `session` so it outlives every handler
        // call this function makes, which is the whole window the assert
        // needs covered.
        let _display_pin = DisplayPinGuard::enter(display.map(|d| d.as_ptr() as usize));

        // `state` is consumed into a raw pointer here and never touched as a
        // reference again for the rest of this function, so no `&mut S` is live
        // while a callback delivers through the dispatcher.
        let session = Session {
            dispatcher: Dispatcher::new(&raw mut *state),
            outputs: RefCell::new(HashMap::new()),
            toplevels: RefCell::new(HashMap::new()),
            decorations: RefCell::new(HashMap::new()),
            layers: RefCell::new(HashMap::new()),
            inputs: RefCell::new(HashMap::new()),
            drags: RefCell::new(HashMap::new()),
            idle_inhibitors: RefCell::new(HashMap::new()),
            session_locks: RefCell::new(HashMap::new()),
            lock_surfaces: RefCell::new(HashMap::new()),
            pointer_constraints: RefCell::new(HashMap::new()),
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

        // Same reasoning, for `layer_surfaces`; see that guard's own doc.
        let _layer_surface_table_guard = LayerSurfaceTableGuard(runtime);

        // Same reasoning, for `outputs`; see that guard's own doc.
        let _output_table_guard = OutputTableGuard(runtime);

        // Same reasoning, for `live_sources`; see that guard's own doc.
        // Declared alongside `_toplevel_table_guard` for the same reason:
        // nothing here depends on drop order relative to the registrations
        // below, but declaring the clear-on-exit guards together up front
        // keeps every "cleared on every exit path" table's guard in one
        // place.
        let _live_sources_guard = LiveSourcesGuard(runtime);

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
            Registration::link_bare(
                &raw mut (*self.raw.as_ptr()).events.new_output,
                on_new_output::<S>,
                (&raw const session).cast::<()>(),
                &raw const *self.alive,
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

        let (timeout, mut remaining) = match until {
            Until::Turns(n) => (0, Some(n)),
            Until::Stop => (-1, None),
        };

        loop {
            if let Some(0) = remaining {
                return Ok(());
            }
            // Called directly via `wl_event_loop_dispatch`, rather than
            // through `EventLoop::dispatch`, so a signal landing mid-poll
            // (`EINTR`) can be retried with the same timeout instead of
            // surfacing as a spurious `Error::Operation` — libwayland's own
            // `wl_event_loop_dispatch` does not retry this itself, and a
            // compositor process fields plenty of signals (`SIGCHLD` from a
            // reaped client, a `SIGUSR1` a consumer installs) that have
            // nothing to do with the loop having failed.
            //
            // SAFETY: `self.loop_` was taken from the `EventLoop<'d>` handed
            // to `autocreate`, so it names a live loop belonging to a
            // display that outlives `'d` — and `&self` keeps that borrow
            // alive for this call. Dispatching here is not the re-entry
            // `EventLoop::dispatch` refuses: `run_inner` is not a handler,
            // and the flag that refusal consults is set only for the
            // duration of a delivery, strictly inside this call rather than
            // around it.
            loop {
                use sys::wayland_sys::ffi_dispatch;
                #[allow(unused_imports)]
                use sys::wayland_sys::server::*;
                let n = unsafe {
                    ffi_dispatch!(
                        sys::wayland_sys::server::wayland_server_handle(),
                        wl_event_loop_dispatch,
                        self.loop_.as_ptr(),
                        timeout
                    )
                };
                if n >= 0 {
                    break;
                }
                let errno = std::io::Error::last_os_error();
                // `ErrorKind::Interrupted` rather than a hard-coded `4`: the
                // crate has no `libc` dependency to name `EINTR`, and std's
                // own mapping of the raw errno to this kind is exactly the
                // portable spelling of it — no numeric constant pinned in
                // this crate that a future target could falsify.
                if errno.kind() != std::io::ErrorKind::Interrupted {
                    return Err(Error::Operation("wl_event_loop_dispatch"));
                }
                // EINTR: a signal landed mid-poll; retry with the same
                // timeout.
            }
            // Every callback this turn's dispatch could have invoked —
            // including any `FdHandler::fd_ready` that called
            // `Runtime::remove_fd` on itself or another source — has
            // returned by now, so no `BorrowedFd` handed out during this
            // turn can still be alive. This is the one point per turn
            // `Runtime::remove_fd`'s deferred-close doc promises a close
            // will actually happen by.
            runtime.drain_pending_closes();
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
            // `emit` happen inside the `wl_event_loop_dispatch` call above,
            // which has already returned — so `hooks.should_stop` (which
            // derefs the pointer) is the sole reader here and sees no other
            // `&mut S` live. This is the discharge site `state_ptr`'s own
            // doc requires: the caller states, right here, why no handler
            // is on the stack.
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
            let Some(non_null) = NonNull::new(source) else {
                return Err(Error::Operation("wl_event_loop_add_fd"));
            };
            // Recorded so `Runtime::remove_fd` can find and remove this
            // registration mid-run; see that method's own doc.
            runtime.record_live_source(id, non_null);
            out.push(FdRegistration {
                source,
                id,
                runtime: runtime.clone(),
                _ctx: ctx,
            });
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
                Registration::link_bare(
                    &raw mut (*shell.as_ptr()).events.new_toplevel,
                    on_new_toplevel::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                )
            });
        }

        if let Some(manager) = runtime.xdg_decoration_manager_ptr() {
            // SAFETY: `create_xdg_decoration_manager` returned a non-null
            // `wlr_xdg_decoration_manager_v1` owned by the display, which
            // this call requires to outlive it, exactly as for the xdg
            // shell just above; the same reasoning applies verbatim.
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*manager.as_ptr()).events.new_toplevel_decoration,
                    on_new_toplevel_decoration::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                )
            });
        }

        if let Some(shell) = runtime.layer_shell_ptr() {
            // SAFETY: `create_layer_shell` returned a non-null
            // `wlr_layer_shell_v1` owned by the display, which this call
            // requires to outlive it, exactly as for the xdg shell above;
            // the same reasoning applies verbatim.
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*shell.as_ptr()).events.new_surface,
                    on_new_layer_surface::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
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
                Registration::link_bare(
                    &raw mut (*self.raw.as_ptr()).events.new_input,
                    on_new_input::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    &raw const *self.alive,
                )
            });
        }

        if let Some(seat) = runtime.seat_ptr() {
            // SAFETY: `create_seat` returned a non-null `wlr_seat` owned by the
            // display, which this call requires to outlive it, exactly as for
            // the xdg shell above — so a null liveness flag is correct (the
            // seat's owner, the display, cannot predecease this call). These
            // are the `request_set_selection` / `request_set_primary_selection`
            // signals a focused client raises to own the clipboard / primary
            // selection; each handler is paired with `on_request_set_*` at the
            // same `S`, as above.
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*seat.as_ptr()).events.request_set_selection,
                    on_request_set_selection::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                )
            });
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*seat.as_ptr()).events.request_set_primary_selection,
                    on_request_set_primary_selection::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                )
            });
            // `request_start_drag`/`start_drag`: a client asking to start a
            // drag-and-drop operation, and wlroots confirming one actually
            // started. Same liveness reasoning as the selection signals just
            // above — the seat is display-owned, so a null liveness flag is
            // correct.
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*seat.as_ptr()).events.request_start_drag,
                    on_request_start_drag::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                )
            });
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*seat.as_ptr()).events.start_drag,
                    on_start_drag::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                )
            });
        }

        if let Some(manager) = runtime.virtual_keyboard_manager_ptr() {
            // SAFETY: `create_virtual_keyboard_manager` returned a non-null
            // manager owned by the display, which this call requires to outlive
            // it, exactly as for the xdg shell above — null liveness is correct.
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*manager.as_ptr()).events.new_virtual_keyboard,
                    on_new_virtual_keyboard::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                )
            });
        }

        if let Some(manager) = runtime.virtual_pointer_manager_ptr() {
            // SAFETY: `create_virtual_pointer_manager` returned a non-null
            // manager owned by the display, which this call requires to outlive
            // it, exactly as for the xdg shell above — null liveness is correct.
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*manager.as_ptr()).events.new_virtual_pointer,
                    on_new_virtual_pointer::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                )
            });
        }

        if let Some(manager) = runtime.idle_inhibit_manager_ptr() {
            // SAFETY: `create_idle_inhibit_manager` returned a non-null
            // manager owned by the display, which this call requires to outlive
            // it, exactly as for the xdg shell above — null liveness is correct.
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*manager.as_ptr()).events.new_inhibitor,
                    on_new_idle_inhibitor::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                )
            });
        }

        if let Some(manager) = runtime.session_lock_manager_ptr() {
            // SAFETY: `create_session_lock_manager` returned a non-null manager
            // owned by the display, which this call requires to outlive it,
            // exactly as for the xdg shell above — null liveness is correct.
            // This is the `new_lock` signal a locker raises to lock the
            // session; `on_new_session_lock` enters the locked state.
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*manager.as_ptr()).events.new_lock,
                    on_new_session_lock::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
                )
            });
        }

        if let Some(manager) = runtime.pointer_constraints_manager_ptr() {
            // SAFETY: `create_pointer_constraints_manager` returned a non-null
            // manager owned by the display, which this call requires to outlive
            // it, exactly as for the xdg shell above — null liveness is correct.
            // This is the `new_constraint` signal wlroots raises when a client
            // confines or locks the pointer to a surface region;
            // `on_new_pointer_constraint` records the constraint's lifecycle.
            regs.push(unsafe {
                Registration::link_bare(
                    &raw mut (*manager.as_ptr()).events.new_constraint,
                    on_new_pointer_constraint::<S>,
                    (session as *const Session<'_, S>).cast::<()>(),
                    std::ptr::null(),
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
/// leave a live source pointing at a freed context. That removal is
/// conditional — see the `Drop` impl below — because
/// [`Runtime::remove_fd`](crate::Runtime::remove_fd) may have already done
/// it mid-run.
struct FdRegistration {
    source: *mut sys::wl_event_source,
    id: SourceId,
    /// Cloned rather than borrowed — one `Rc` bump (see [`Runtime`]'s own
    /// doc) — so this struct needs no lifetime parameter of its own for
    /// what is otherwise a plain, `'static`-shaped registration handle.
    runtime: Runtime,
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
        // `Runtime::remove_fd` may have already removed this exact source
        // mid-run — the motivating case is a consumer calling it from
        // inside this very source's own `fd_ready`. `take_live_source` is
        // the single choke point that decides which of the two removers
        // actually calls `wl_event_source_remove`; see its own doc. `None`
        // here means `remove_fd` already claimed and removed it, so this
        // teardown has nothing left to do — calling
        // `wl_event_source_remove` a second time would be a use-after-free
        // on a `wl_event_source` libwayland has already freed.
        if self.runtime.take_live_source(self.id).is_none() {
            return;
        }

        use sys::wayland_sys::ffi_dispatch;
        #[allow(unused_imports)]
        use sys::wayland_sys::server::*;

        // SAFETY: `source` came from `wl_event_loop_add_fd` on a loop that
        // outlives this run (the `Display` borrow guarantees it), and the
        // `take_live_source` call above establishes this is the first (and
        // by construction, only) removal of it.
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
        Event::RequestMaximize(id, maximize) => {
            with_toplevel(session, id, |t| state.request_maximize(t, maximize));
            // xdg-shell requires an answer to *every* maximize/fullscreen
            // request, whether or not the compositor grants it — and the
            // trait default has no `Runtime` to send one with, so this is
            // the dispatch layer discharging that guarantee instead.
            // Unconditional, not "only if the handler staged nothing":
            // wlroots coalesces a second scheduled configure into whatever
            // the handler above already staged through `Runtime::
            // set_toplevel_*`, so scheduling here as well is harmless, and
            // "harmless every time" is simpler and no less correct than
            // probing for whether it was needed this time.
            session.runtime.configure_toplevel(id);
        }
        Event::RequestFullscreen(id, fullscreen) => {
            with_toplevel(session, id, |t| state.request_fullscreen(t, fullscreen));
            // As above.
            session.runtime.configure_toplevel(id);
        }
        Event::RequestMove(id) => state.request_move(id),
        Event::RequestResize(id, edges) => state.request_resize(id, edges),
        Event::RequestDecorationMode(id, preference) => {
            // Cleared here, immediately before the handler runs, rather
            // than back in `on_decoration_request_mode` at emit time — see
            // `DecorationEntry::mode_set_this_dispatch`'s own doc. This is
            // what makes clear-then-check correct even if this event is
            // ever deferred (queued behind an outer handler, or delivered
            // from the synthetic "no `request_mode` ever fired" path
            // `on_surface_commit` drives at initial commit): the clear and
            // the check below now bracket exactly one handler invocation,
            // never a different one's.
            session.runtime.clear_decoration_dispatch_flag(id);
            state.request_decoration_mode(id, preference);
            // wlroots requires a mode be set before the initial commit is
            // answered — see `Runtime::set_decoration_mode`'s own doc for
            // how that requirement is actually satisfied (staged, not sent,
            // until the surface is initialized). The handler may have
            // already answered through `Runtime::set_decoration_mode`,
            // which sets this same flag; unlike `RequestMaximize`'s bare
            // configure, this follow-up is *conditional*: sending a second
            // `set_mode` after the handler already sent (or staged) one
            // would tell the client server-side and then, in the same turn,
            // whatever the handler asked for — not "harmless, coalesced"
            // the way a second configure is.
            if !session.runtime.decoration_dispatch_flag(id) {
                session
                    .runtime
                    .set_decoration_mode(id, crate::DecorationMode::ServerSide);
            }
        }
        Event::NewLayerSurface(id) => {
            with_layer_surface(session, id, |s| state.new_layer_surface(s))
        }
        Event::LayerSurfaceCommit(id) => {
            with_layer_surface(session, id, |s| state.layer_surface_commit(s))
        }
        Event::LayerSurfaceMapped(id) => state.layer_surface_mapped(id),
        Event::LayerSurfaceUnmapped(id) => state.layer_surface_unmapped(id),
        Event::LayerSurfaceDestroyed(id) => state.layer_surface_destroyed(id),
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
        Event::SessionLockChanged(locked) => state.session_lock_changed(locked),
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
    let Some(entry) = session.runtime.toplevel_entry(id) else {
        return;
    };
    // SAFETY: an entry is removed by `on_toplevel_destroy`, which wlroots runs
    // before it frees the toplevel, so a present entry names a live one. The
    // handle is created and dropped inside this call, so it cannot outlive
    // the handler `f` passes it to, and `f` cannot drive the loop (the
    // dispatcher's handler flag is set for exactly this window).
    let toplevel = unsafe { Toplevel::from_raw_with_id(entry.raw.as_ptr(), id) };
    f(&toplevel);
}

/// Borrow the layer surface `id` names, if this runtime still knows of one.
/// Mirrors [`with_toplevel`] exactly, including its obligations on `f`.
fn with_layer_surface<S>(
    session: &Session<'_, S>,
    id: LayerSurfaceId,
    f: impl FnOnce(&LayerSurface<'_>),
) {
    let Some(raw) = session.runtime.layer_surface_ptr(id) else {
        return;
    };
    // SAFETY: an entry is removed by `on_layer_surface_destroy`, which
    // wlroots runs before it frees the layer surface, so a present entry
    // names a live one. The handle is created and dropped inside this call,
    // exactly as `with_toplevel`'s does.
    let surface = unsafe { LayerSurface::from_raw_with_id(raw.as_ptr(), id) };
    f(&surface);
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

/// The signal this listener was linked into fired; record that and nothing else.
///
/// The counterpart to [`on_backend_destroy`] for signals that do not mean "the
/// object is going away" — `wlr_renderer.events.lost` is the one this exists
/// for. See [`Registration::link_flag`].
unsafe extern "C" fn on_set_flag(l: *mut sys::wl_listener, _data: *mut std::ffi::c_void) {
    // SAFETY: wlroots invokes this only for a listener `link_flag` created,
    // which is the `listener` field of a live `Bound` whose `flag` the caller
    // guaranteed outlives the registration.
    //
    // Nothing here can unwind: a `Cell<bool>` write has no failure mode and
    // calls no user code, which matters because this is an `extern "C"` frame.
    unsafe {
        let bound = bound_of(l);
        (*(*bound).flag).set(true);
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
        let frame = Registration::link_output(
            &raw mut (*output).events.frame,
            on_frame::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let destroy = Registration::link_output(
            &raw mut (*output).events.destroy,
            on_output_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
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

        // Recorded in `Runtime`'s own table too, alongside the session-local
        // listeners above: `output_layout_box`/`set_output_position` are
        // called through a `Runtime` clone, which cannot reach `session`.
        // Before the handler is told, mirroring `record_toplevel`'s own
        // ordering, for the same reason (see that call site's comment).
        if let Some(raw) = NonNull::new(output) {
            (*session).runtime.record_output(id, raw);
        }

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
        // Mirrors the removal above, in `Runtime`'s own table — see
        // `record_output`'s call site for why both exist.
        (*session).runtime.forget_output(id);

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::OutputDestroyed(id), deliver);
    }
}

/// A client created a toplevel. Give it an id and a scene tree before anyone
/// is told about it.
/// Honors a client's request to own the clipboard selection. wlroots emits
/// this only with a valid client grant serial, so accepting it as delivered
/// (calling `wlr_seat_set_selection` with the event's own source and serial)
/// is the standard, correct compositor behavior — the same thing tinywl and
/// sway do. A request the client is not entitled to make never reaches here.
unsafe extern "C" fn on_request_set_selection<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener linked into
    // `wlr_seat.events.request_set_selection`, whose `session` is the
    // `*const Session<'_, S>` paired with this instantiation. The signal
    // carries a live `*mut wlr_seat_request_set_selection_event`; its
    // `source`/`serial` are copied straight back into `wlr_seat_set_selection`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let event = data.cast::<sys::wlr_seat_request_set_selection_event>();
        if let Some(seat) = (*session).runtime.seat_ptr() {
            sys::wlr_seat_set_selection(seat.as_ptr(), (*event).source, (*event).serial);
        }
    }
}

/// Honors a client's request to own the primary (middle-click) selection.
/// Same reasoning as [`on_request_set_selection`].
unsafe extern "C" fn on_request_set_primary_selection<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: as for `on_request_set_selection`, but the signal is
    // `wlr_seat.events.request_set_primary_selection` carrying a live
    // `*mut wlr_seat_request_set_primary_selection_event`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let event = data.cast::<sys::wlr_seat_request_set_primary_selection_event>();
        if let Some(seat) = (*session).runtime.seat_ptr() {
            sys::wlr_seat_set_primary_selection(seat.as_ptr(), (*event).source, (*event).serial);
        }
    }
}

/// Honors a client's request to start a drag-and-drop operation. wlroots
/// hands this listener a `wlr_drag` already built (by
/// `wlr_data_device_manager`/`wlr_primary_selection`) but not yet armed; the
/// grab only takes effect — and the seat only enters its drag state machine —
/// once one of `wlr_seat_start_pointer_drag`/`wlr_seat_start_touch_drag` is
/// called below. Validating the serial first is the security gate: a serial
/// that does not match a live pointer or touch grab on `event.origin` means
/// the client is not entitled to start this drag (it is stale, forged, or
/// belongs to a different surface), so neither branch fires and the drag
/// never starts. A later negative test pins this — do not weaken it.
unsafe extern "C" fn on_request_start_drag<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener linked into
    // `wlr_seat.events.request_start_drag`, whose `session` is the
    // `*const Session<'_, S>` paired with this instantiation. The signal
    // carries a live `*mut wlr_seat_request_start_drag_event`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let event = data.cast::<sys::wlr_seat_request_start_drag_event>();
        let Some(seat) = (*session).runtime.seat_ptr() else {
            return;
        };
        let seat = seat.as_ptr();
        // Pointer drag if the serial is a valid pointer grant.
        if sys::wlr_seat_validate_pointer_grab_serial(seat, (*event).origin, (*event).serial) {
            sys::wlr_seat_start_pointer_drag(seat, (*event).drag, (*event).serial);
            return;
        }
        // Otherwise try touch.
        let mut point: *mut sys::wlr_touch_point = std::ptr::null_mut();
        if sys::wlr_seat_validate_touch_grab_serial(
            seat,
            (*event).origin,
            (*event).serial,
            &raw mut point,
        ) {
            sys::wlr_seat_start_touch_drag(seat, (*event).drag, (*event).serial, point);
        }
        // Neither grab validated: the serial is invalid, so the drag never
        // starts. This is not an error — wlroots expects compositors to
        // silently ignore an unentitled request.
    }
}

/// A drag started (via [`on_request_start_drag`] above, once wlroots' own
/// validation and `wlr_seat_start_*_drag` armed it). Parents the drag's icon
/// — if it has one; not every drag carries a visible icon — into the overlay
/// scene band so it renders above every toplevel and layer surface for the
/// duration of the drag, and records the tree in
/// [`RuntimeInner::drag_icon_tree`] so [`Runtime::drag_icon_position`] can
/// observe it.
///
/// An earlier revision of this doc claimed the scene node
/// `wlr_scene_drag_icon_create` returns self-tracks the pointer/touch
/// position via its own motion listeners. That was wrong — verified against
/// wlroots 0.20.2's own C source, `wlr_scene_drag_icon`'s only reposition
/// listener fires on the icon surface's own buffer-commit deltas, never on
/// cursor motion, so left alone the icon renders once at `(0, 0)` and never
/// moves. This crate repositions the node itself instead: `on_pointer_motion`,
/// `on_pointer_motion_absolute` (below), and
/// [`Runtime::inject_touch_motion`] each call
/// [`Runtime::reposition_drag_icon`] with their own freshly-updated cursor or
/// touch-point layout position, on every motion, for as long as
/// `drag_icon_tree` holds a tree — matching tinywl's own
/// `wlr_scene_node_set_position(&drag_icon->node, cursor->x, cursor->y)`. The
/// icon's own hotspot offset is still applied separately, by wlroots'
/// internal `surface_tree` commit handler on the node this tree parents;
/// this crate only ever positions the outer tree.
///
/// Also links a destroy listener on the drag *icon's* own `events.destroy`
/// (not the drag's — see below), keyed in `Session::drags` by the icon
/// pointer — mirrors `on_new_input`'s `InputDevice::_destroy` pattern (same
/// keying discipline: the pointer handed to the destroy signal is exactly
/// the key this inserts under), used here instead of
/// `Registration::link_toplevel`'s id-carrying variants because a drag icon
/// has no [`ToplevelId`]/[`OutputId`]/[`LayerSurfaceId`] to carry. That
/// listener — not this function — is what clears `drag_icon_tree` back to
/// `None`; see [`on_drag_destroy`]'s own doc for why that must happen before
/// wlroots frees the tree, and for why the icon's destroy (not the drag's)
/// is the correct lifetime boundary to watch: wlroots frees the scene tree
/// on two independent paths — the drag ending normally, and a client
/// destroying the drag-icon surface mid-drag while the drag itself survives
/// with `drag->icon` nulled out — and only `(*icon).events.destroy` fires on
/// both, always in the same synchronous emission that frees the tree.
/// `(*drag).events.destroy` does not fire on the second path at all, which
/// would leave `drag_icon_tree` holding a dangling pointer if that were the
/// signal watched here.
unsafe extern "C" fn on_start_drag<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener linked into
    // `wlr_seat.events.start_drag`, whose `session` is the
    // `*const Session<'_, S>` paired with this instantiation. The signal
    // carries a live `*mut wlr_drag`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;
        let drag = data.cast::<sys::wlr_drag>();
        if (*drag).icon.is_null() {
            return;
        }
        let Some(overlay) = runtime.band_ptr(Band::Overlay) else {
            return;
        };
        let tree = sys::wlr_scene_drag_icon_create(overlay.as_ptr(), (*drag).icon);
        let Some(tree) = NonNull::new(tree) else {
            return;
        };
        *runtime.inner.drag_icon_tree.borrow_mut() = Some(tree);

        // Linked on the icon's own `events.destroy`, not the drag's — see
        // this function's own doc for why that is the signal that fires on
        // both teardown paths. No `alive` backstop: as for the decoration
        // listeners this mirrors, the icon cannot be freed by anything other
        // than the destroy this very listener watches, so there is no
        // "owner died first" case to guard against. `(*drag).icon` is
        // non-null here — checked above — so dereferencing it to reach its
        // `events` is sound.
        let icon = (*drag).icon;
        let destroy = Registration::link_bare(
            &raw mut (*icon).events.destroy,
            on_drag_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
        );
        (*session).drags.borrow_mut().insert(icon as usize, destroy);
    }
}

/// The drag icon is about to be freed. Clear [`RuntimeInner::drag_icon_tree`]
/// *now*, whatever the handler does — the tree `on_start_drag` stored there
/// is a child of this very icon, and wlroots' `drag_icon_destroy` (called
/// either from `seat_handle_drag_destroy` when the drag ends normally, or
/// independently when a client destroys the drag-icon surface mid-drag)
/// frees the scene tree as part of the same teardown that emits this signal,
/// so a read through [`Runtime::drag_icon_position`] after this point without
/// this clear would dereference freed memory.
///
/// Also removes this icon's entry from `Session::drags`, which unlinks this
/// very listener — sound for the same reason `on_input_destroy` and
/// `on_toplevel_decoration_destroy` unlinking themselves is:
/// `wl_signal_emit_mutable` has already advanced its cursor past the firing
/// listener before this callback runs.
unsafe extern "C" fn on_drag_destroy<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_start_drag` into this drag icon's own
    // `events.destroy`; the icon is still live memory for the duration of
    // this emission. `data` is the same `*mut wlr_drag_icon` `on_start_drag`
    // linked against — wlroots' `drag_icon_destroy` emits `events.destroy`
    // with the icon itself as the signal data, on both the normal-drag-end
    // path and the icon-surface-destroyed-mid-drag path — so, cast to
    // `usize`, it recovers the exact key that call inserted this entry
    // under regardless of which path fired.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;
        runtime.inner.drag_icon_tree.borrow_mut().take();
        let key = data as usize;
        let removed = (*session).drags.borrow_mut().remove(&key);
        drop(removed);
    }
}

/// A client bound `zwp_idle_inhibit_manager_v1` and requested an inhibitor on
/// one of its surfaces. Counts it in [`RuntimeInner::idle_inhibitors`] and
/// re-gates the idle notifier via
/// [`Runtime::refresh_idle_inhibited`](crate::Runtime::refresh_idle_inhibited),
/// then links a destroy listener on the inhibitor's own `events.destroy`,
/// keyed in `Session::idle_inhibitors` by that destroy listener's own address
/// — NOT the inhibitor pointer, because wlroots emits the inhibitor's
/// `destroy` signal with its **surface** as the signal `data`, not the
/// inhibitor. Mirrors the session-lock keying discipline: the destroy handler
/// recovers the same key from the `wl_listener` `l` it is handed.
unsafe extern "C" fn on_new_idle_inhibitor<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener linked into
    // `wlr_idle_inhibit_manager_v1.events.new_inhibitor`, whose `session` is
    // the `*const Session<'_, S>` paired with this instantiation. The signal
    // carries a live `*mut wlr_idle_inhibitor_v1`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;
        let inhibitor = data.cast::<sys::wlr_idle_inhibitor_v1>();

        runtime
            .inner
            .idle_inhibitors
            .set(runtime.inner.idle_inhibitors.get() + 1);
        runtime.refresh_idle_inhibited();

        // Linked on the inhibitor's own `events.destroy`. No `alive`
        // backstop: as for the drag icon this mirrors, the inhibitor cannot
        // be freed by anything other than the destroy this very listener
        // watches, so there is no "owner died first" case to guard against.
        let destroy = Registration::link_bare(
            &raw mut (*inhibitor).events.destroy,
            on_idle_inhibitor_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
        );
        // Key by the destroy listener's own address, not the inhibitor
        // pointer: wlroots emits the inhibitor's `destroy` with its surface as
        // `data`, so the destroy handler recovers the key from the `l` it is
        // handed (equal to this address), never from the signal `data`.
        let key = destroy.listener_addr();
        (*session).idle_inhibitors.borrow_mut().insert(key, destroy);
    }
}

/// The idle inhibitor is about to be freed. Decrements
/// [`RuntimeInner::idle_inhibitors`] (saturating — this handler and
/// `on_new_idle_inhibitor` are the only writers, so it never underflows in
/// practice, but a stray double-fire must not panic) and re-gates the idle
/// notifier, then removes this inhibitor's entry from
/// `Session::idle_inhibitors`, which unlinks this very listener — sound for
/// the same reason `on_drag_destroy` unlinking itself is:
/// `wl_signal_emit_mutable` has already advanced its cursor past the firing
/// listener before this callback runs.
unsafe extern "C" fn on_idle_inhibitor_destroy<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_idle_inhibitor` into this inhibitor's own
    // `events.destroy`; the inhibitor is still live memory for this emission.
    // wlroots emits this signal with the inhibitor's **surface** as `data`
    // (NOT the inhibitor), so identity comes from `l` — the address of this
    // handler's own listener, under which `on_new_idle_inhibitor` keyed the
    // `idle_inhibitors` entry — never from the signal `data`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;
        runtime
            .inner
            .idle_inhibitors
            .set(runtime.inner.idle_inhibitors.get().saturating_sub(1));
        runtime.refresh_idle_inhibited();
        let key = l as usize;
        let removed = (*session).idle_inhibitors.borrow_mut().remove(&key);
        drop(removed);
    }
}

/// A client bound `zwp_pointer_constraints_v1` and requested a constraint
/// (confine or lock) on one of its surfaces. Links this run's `set_region` and
/// `destroy` listeners on the constraint's own signals and records them in
/// `Session::pointer_constraints`, keyed by the `destroy` listener's own
/// address — the same keying discipline every other destroy-tracked table here
/// uses, so identity never depends on what the destroy signal happens to pass
/// as `data` (it passes the constraint, but this code does not rely on that).
///
/// This is lifecycle + observability only: it does **not** activate the
/// constraint. Which constraint becomes the active one — and the motion
/// suppression/clamp it then drives — is the enforcement path's job, landing in
/// a follow-up task; a freshly created constraint stays inactive here and that
/// path picks it up on the next focus resolution.
unsafe extern "C" fn on_new_pointer_constraint<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener linked into
    // `wlr_pointer_constraints_v1.events.new_constraint`, whose `session` is
    // the `*const Session<'_, S>` paired with this instantiation. The signal
    // carries a live `*mut wlr_pointer_constraint_v1`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(constraint) = NonNull::new(data.cast::<sys::wlr_pointer_constraint_v1>()) else {
            return;
        };

        // Linked on the constraint's own `set_region`/`destroy`. No `alive`
        // backstop, as for the idle inhibitor this mirrors: the constraint
        // cannot be freed by anything other than the destroy this very listener
        // watches, so there is no "owner died first" case to guard against.
        let set_region = Registration::link_bare(
            &raw mut (*constraint.as_ptr()).events.set_region,
            on_pointer_constraint_set_region::<S>,
            (*bound).session,
            std::ptr::null(),
        );
        let destroy = Registration::link_bare(
            &raw mut (*constraint.as_ptr()).events.destroy,
            on_pointer_constraint_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
        );
        // Key by the destroy listener's own address: the destroy handler
        // recovers the same key from the `l` it is handed, never from the
        // signal `data`.
        let key = destroy.listener_addr();
        (*session).pointer_constraints.borrow_mut().insert(
            key,
            PointerConstraintListeners {
                _set_region: set_region,
                _destroy: destroy,
                constraint,
            },
        );
    }
}

/// A constraint's region changed. wlroots keeps `constraint->region` current,
/// so there is nothing to store — but if the *active* constraint's region just
/// moved off the cursor, the confine invariant (cursor inside the region) is
/// broken, and every future motion's `wlr_region_confine` would return `false`
/// and wedge the pointer. A client can trigger exactly that. So when the
/// constraint whose region changed is the active one, re-anchor the cursor back
/// into the region immediately, restoring the invariant the moment it could
/// break.
///
/// The constraint is identified from the firing listener `l` — matched against
/// each entry's own `set_region` `Registration` — never from the signal `data`,
/// the same discipline the destroy handler uses.
unsafe extern "C" fn on_pointer_constraint_set_region<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_pointer_constraint` into a live constraint's
    // `events.set_region`; the constraint is live for this emission.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;

        // Which constraint fired? Match `l` against each entry's `set_region`
        // listener address (the map is keyed by the *destroy* listener, so a
        // scan is needed here).
        let key = l as usize;
        let constraint = {
            let map = (*session).pointer_constraints.borrow();
            map.values()
                .find(|entry| entry._set_region.listener_addr() == key)
                .map(|entry| entry.constraint)
        };

        let Some(constraint) = constraint else {
            return;
        };
        if runtime.active_constraint() != Some(constraint) {
            return;
        }

        // Restore the confine invariant. If the cursor moved, re-notify the
        // surface under its new position so pointer focus stays correct.
        if let Some((x, y)) = runtime.reanchor_cursor_into_region(constraint)
            && let Some(seat) = runtime.seat_ptr()
        {
            enter_surface_under_cursor(session, seat.as_ptr(), x, y, 0);
        }
    }
}

/// The pointer constraint is about to be freed. Removes this constraint's entry
/// from `Session::pointer_constraints` — keyed by the firing listener's own
/// address (`l`), NOT the signal `data` — which drops and so unlinks its two
/// listeners together, sound for the same reason `on_idle_inhibitor_destroy`
/// unlinking itself is: `wl_signal_emit_mutable` has already advanced its
/// cursor past this listener before the callback runs. If the constraint being
/// destroyed is the one recorded in
/// [`RuntimeInner::active_constraint`](crate::runtime::RuntimeInner), clears it
/// so enforcement stops — comparing against the constraint captured in the
/// removed entry, never against the destroy signal's `data`.
unsafe extern "C" fn on_pointer_constraint_destroy<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_pointer_constraint` into this constraint's own
    // `events.destroy`; the constraint is still live memory for this emission.
    // Identity comes from `l` — the address of this handler's own listener,
    // under which `on_new_pointer_constraint` keyed the entry — never from the
    // signal `data`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;
        let key = l as usize;
        let removed = (*session).pointer_constraints.borrow_mut().remove(&key);
        if let Some(entry) = &removed
            && runtime.active_constraint() == Some(entry.constraint)
        {
            runtime.inner.active_constraint.set(None);
        }
        drop(removed);
    }
}

/// A locker took a lock on the session — the security core. wlroots hands this
/// a `wlr_session_lock_v1` the instant a client binds
/// `ext_session_lock_manager_v1` and sends `lock`. This **synchronously**
/// enters the locked state ([`Runtime::begin_session_lock`]: `session_locked =
/// true`, the lock recorded, keyboard focus pulled off whatever normal client
/// held it), links the lock's own `new_surface`/`unlock`/`destroy` listeners,
/// and then notifies the handler via `session_lock_changed(true)`. The state
/// change is applied here and now, not deferred behind the handler — the lock
/// is in force the moment this returns, whatever a handler does.
///
/// A new lock may arrive while [`Runtime::is_session_locked`] is already
/// `true` — the previous locker died without unlocking, leaving the session
/// locked (see [`on_session_lock_destroy`]) — in which case this simply takes
/// over: `session_lock` was cleared to `None` by that destroy, so recording
/// the new one and re-clearing focus is exactly right.
unsafe extern "C" fn on_new_session_lock<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener linked into
    // `wlr_session_lock_manager_v1.events.new_lock`, whose `session` is the
    // `*const Session<'_, S>` paired with this instantiation. The signal
    // carries a live `*mut wlr_session_lock_v1`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;
        let lock = data.cast::<sys::wlr_session_lock_v1>();
        let Some(lock) = NonNull::new(lock) else {
            return;
        };

        // Reject a second, concurrent lock. wlroots fires `new_lock` for EVERY
        // lock request; refusing one while another is still live is the
        // compositor's job. If we accepted it, a malicious client could take a
        // second lock over the already-locked session, cover the outputs to
        // drive `locked`, then `unlock_and_destroy` to unlock the session with
        // no user authentication — a full lock-screen bypass. Per
        // ext-session-lock, reject by destroying the new lock (this sends
        // `finished` to that client) and leave the existing lock untouched.
        // `session_lock_ptr()` is `None` both when never locked and when a
        // previous locker DIED (its destroy cleared it), so the legitimate
        // takeover-after-crash path is preserved; only a second lock over a
        // still-live one is refused.
        if runtime.session_lock_ptr().is_some() {
            sys::wlr_session_lock_v1_destroy(lock.as_ptr());
            return;
        }

        // Enter the locked state synchronously — the security bit is set here,
        // before anything else runs.
        runtime.begin_session_lock(lock);

        // Link the lock's three signals. Null liveness flag: each is dropped
        // from inside `on_session_lock_destroy`, while the lock is still
        // alive, the same discipline the toplevel listeners use.
        let new_surface = Registration::link_bare(
            &raw mut (*lock.as_ptr()).events.new_surface,
            on_session_lock_new_surface::<S>,
            (*bound).session,
            std::ptr::null(),
        );
        let unlock = Registration::link_bare(
            &raw mut (*lock.as_ptr()).events.unlock,
            on_session_unlock::<S>,
            (*bound).session,
            std::ptr::null(),
        );
        let destroy = Registration::link_bare(
            &raw mut (*lock.as_ptr()).events.destroy,
            on_session_lock_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
        );
        // Key by the destroy listener's own address, not the lock pointer:
        // wlroots emits the lock's `destroy` with a null `data`, so the
        // destroy handler recovers the key from the `l` it is handed (equal to
        // this address), never from the object pointer.
        let key = destroy.listener_addr();
        (*session).session_locks.borrow_mut().insert(
            key,
            SessionLockListeners {
                _new_surface: new_surface,
                _unlock: unlock,
                _destroy: destroy,
            },
        );

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::SessionLockChanged(true), deliver);
    }
}

/// A lock surface arrived for the active lock — one per output the locker
/// wants to cover. Renders it in [`Band::Lock`] (topmost, above every normal
/// surface), configures it to the output's current size, focuses the first
/// one for the keyboard, and links its commit/destroy listeners. The commit
/// listener is what eventually sends the protocol's `locked` event once every
/// output is covered.
unsafe extern "C" fn on_session_lock_new_surface<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener linked into a live
    // `wlr_session_lock_v1.events.new_surface`, whose `session` is the paired
    // `*const Session<'_, S>`. The signal carries a live
    // `*mut wlr_session_lock_surface_v1`, with its `.output` and `.surface`
    // set.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;
        let lock_surface = data.cast::<sys::wlr_session_lock_surface_v1>();
        let Some(lock_surface) = NonNull::new(lock_surface) else {
            return;
        };
        let output = (*lock_surface.as_ptr()).output;
        let surface = (*lock_surface.as_ptr()).surface;
        if surface.is_null() {
            return;
        }

        // No lock band means graphics was never initialised — drop the surface
        // rather than dereference a null tree, the same shape `on_new_toplevel`
        // takes for its band.
        let Some(lock_band) = runtime.band_ptr(Band::Lock) else {
            return;
        };

        // Render the lock surface in the topmost lock band.
        let tree = sys::wlr_scene_subsurface_tree_create(lock_band.as_ptr(), surface);
        let Some(tree) = NonNull::new(tree) else {
            return;
        };

        // Focus the *first* lock surface for the keyboard: if nothing is
        // covered yet, this is the first. Computed before recording so the
        // check is honest.
        let is_first = runtime.inner.lock_surface_trees.borrow().is_empty();
        runtime.record_lock_surface(output, tree, surface);

        // Configure it to the output's current pixel size, so the client can
        // paint a full-screen lock. `width`/`height` are `i32` on `wlr_output`
        // and non-negative for a real mode; clamp to be safe before the `u32`
        // cast the protocol call takes.
        let (w, h) = if output.is_null() {
            (0u32, 0u32)
        } else {
            (
                (*output).width.max(0) as u32,
                (*output).height.max(0) as u32,
            )
        };
        sys::wlr_session_lock_surface_v1_configure(lock_surface.as_ptr(), w, h);

        if is_first {
            // SAFETY: `surface` is the live surface of a just-announced lock
            // surface.
            runtime.focus_lock_surface_keyboard(surface);
        }

        // Commit drives the send-`locked` decision; destroy drops the tree
        // from the runtime before wlroots frees it. Both null-liveness, both
        // dropped from `on_session_lock_surface_destroy`.
        let commit = Registration::link_bare(
            &raw mut (*surface).events.commit,
            on_session_lock_surface_commit::<S>,
            (*bound).session,
            std::ptr::null(),
        );
        let destroy = Registration::link_bare(
            &raw mut (*lock_surface.as_ptr()).events.destroy,
            on_session_lock_surface_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
        );
        // Key by the destroy listener's own address, not the lock surface
        // pointer: wlroots emits this surface's `destroy` with a null `data`,
        // so the destroy handler recovers the key from the `l` it is handed
        // (equal to this address), never from the object pointer.
        let key = destroy.listener_addr();
        (*session).lock_surfaces.borrow_mut().insert(
            key,
            LockSurfaceListeners {
                _commit: commit,
                _destroy: destroy,
                output: output as usize,
            },
        );
    }
}

/// A lock surface committed. Once every live output is covered by a committed,
/// mapped lock surface — and it has not already been sent — this sends the
/// protocol's `locked` event exactly once ([`Runtime::send_locked_if_covered`]
/// carries the coverage check and the send-once guard). `data` (the surface)
/// is unused: the decision is global, not per-surface.
unsafe extern "C" fn on_session_lock_surface_commit<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_session_lock_new_surface` into a live lock
    // surface's underlying `wlr_surface.events.commit`, unlinked before that
    // surface is freed.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        (*session).runtime.send_locked_if_covered();
    }
}

/// A lock surface is about to be freed. Drops its scene tree from the runtime
/// (keyed by the output it covered) *before* wlroots frees the tree — the
/// subsurface tree is destroyed together with its surface — and unlinks this
/// surface's own commit/destroy listeners by removing its `lock_surfaces`
/// entry. The tree is only ever dropped here, never destroyed by this crate:
/// wlroots owns it.
unsafe extern "C" fn on_session_lock_surface_destroy<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_session_lock_new_surface` into this lock surface's
    // own `events.destroy`; the lock surface is still live memory for this
    // emission. wlroots emits this signal with a **null** `data`
    // (`wlr_session_lock_v1.c`'s `lock_surface_destroy` passes NULL, then
    // asserts the listener list is empty), so identity comes from `l` — the
    // address of this handler's own listener, under which
    // `on_session_lock_new_surface` keyed the `lock_surfaces` entry — and the
    // covered output was captured in that entry at link time, not read from
    // `data` here.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;
        // wlroots emits this signal with a null `data`, so the lock surface
        // pointer (and thus its `.output`) is not available here. The key is
        // the destroy listener's own address, which is exactly the `l` wlroots
        // handed us; the covered output was captured in the entry at link time.
        let key = l as usize;
        let removed = (*session).lock_surfaces.borrow_mut().remove(&key);
        if let Some(entry) = &removed {
            runtime.forget_lock_surface(entry.output as *mut sys::wlr_output);
        }
        drop(removed);
    }
}

/// The active locker asked to **unlock** the session — a genuine unlock, the
/// only path that clears the lock. Records the unlock (so the following lock
/// `destroy` completes the teardown rather than staying locked), drops the
/// lock surfaces, leaves the session unlocked, and notifies the handler via
/// `session_lock_changed(false)`. The compositor re-establishes its own focus
/// from that callback.
unsafe extern "C" fn on_session_unlock<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_session_lock` into a live
    // `wlr_session_lock_v1.events.unlock`, whose `session` is the paired
    // `*const Session<'_, S>`. `_data` unused — the active lock is already
    // recorded on the runtime.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;

        // Apply the unlock synchronously: session_locked -> false, unlock
        // requested recorded, surfaces dropped.
        runtime.unlock_session();

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::SessionLockChanged(false), deliver);
    }
}

/// The active `wlr_session_lock_v1` is about to be freed — **the security
/// invariant lives here**. wlroots frees the lock either after an `unlock`
/// (the client's own teardown) or when the locker dies. These two must be told
/// apart:
///
/// - **After a genuine unlock** ([`on_session_unlock`] ran first, so
///   [`Runtime::take_lock_destroy_was_unlocked`] returns `true`): the session
///   is already unlocked; this just finishes the teardown — clears the lock
///   pointer, drops any leftover surface trees, unlinks the lock's listeners.
///
/// - **Without an unlock** (the locker crashed or was killed;
///   `take_lock_destroy_was_unlocked` returns `false`): the session **stays
///   locked**. `session_locked` is left `true`, the lock band stays in place
///   (now blank — the dead client's surface trees are dropped, but input is
///   still refused to every normal client by the focus/hit-test gates), and
///   `session_lock_changed(false)` is **never** fired. A subsequent
///   `on_new_session_lock` may take over and eventually unlock. This is what
///   keeps the screen locked when a lock screen crashes; it is reviewed
///   adversarially and must not be weakened.
unsafe extern "C" fn on_session_lock_destroy<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_session_lock` into this lock's own
    // `events.destroy`; the lock is still live memory for this emission.
    // wlroots emits this signal with a **null** `data`, so identity comes from
    // `l` — the address of this handler's own listener, under which
    // `on_new_session_lock` keyed the `session_locks` entry — not from the
    // object pointer.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;

        // Common cleanup, and the unlock/no-unlock discriminant. This never
        // touches `session_locked`, so the stay-locked path is preserved by
        // construction — only the genuine-unlock branch, via
        // `on_session_unlock`, ever cleared it.
        let was_unlocked = runtime.take_lock_destroy_was_unlocked();

        // Unlink this lock's own three listeners.
        let key = l as usize;
        let removed = (*session).session_locks.borrow_mut().remove(&key);
        drop(removed);

        if was_unlocked {
            // Genuine unlock already ran: teardown is complete, session is
            // unlocked, handler was already told. Nothing more.
        } else {
            // SECURITY: the locker died without unlocking. Keep the session
            // locked — do NOT clear `session_locked`, do NOT fire
            // `session_lock_changed(false)`. The dead client's surfaces are
            // gone (their trees dropped above), so pull keyboard focus off
            // them; input stays refused to normal clients by the gates.
            runtime.clear_keyboard_focus();
        }
    }
}

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
        let Some(toplevel_band) = (*session).runtime.toplevel_band_ptr() else {
            return;
        };

        // Insert into the scene before the handler runs, so that a handler
        // positioning the window by id finds a node to position. Parented
        // into the toplevel band, not the scene root directly — see
        // `Layer`'s own doc for why this is what makes `raise_toplevel`
        // (and every layer surface's fixed position relative to it)
        // structurally correct rather than an ordering that the next
        // toplevel or the next raise call can undo.
        let tree = sys::wlr_scene_xdg_surface_create(toplevel_band.as_ptr(), base);
        let Some(tree) = NonNull::new(tree) else {
            return;
        };
        let Some(raw) = NonNull::new(toplevel) else {
            return;
        };

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
        let commit = Registration::link_toplevel(
            &raw mut (*surface).events.commit,
            on_surface_commit::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let map = Registration::link_toplevel(
            &raw mut (*surface).events.map,
            on_toplevel_map::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let unmap = Registration::link_toplevel(
            &raw mut (*surface).events.unmap,
            on_toplevel_unmap::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let set_title = Registration::link_toplevel(
            &raw mut (*toplevel).events.set_title,
            on_toplevel_set_title::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let destroy = Registration::link_toplevel(
            &raw mut (*toplevel).events.destroy,
            on_toplevel_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let request_maximize = Registration::link_toplevel(
            &raw mut (*toplevel).events.request_maximize,
            on_toplevel_request_maximize::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let request_fullscreen = Registration::link_toplevel(
            &raw mut (*toplevel).events.request_fullscreen,
            on_toplevel_request_fullscreen::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let request_move = Registration::link_toplevel(
            &raw mut (*toplevel).events.request_move,
            on_toplevel_request_move::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let request_resize = Registration::link_toplevel(
            &raw mut (*toplevel).events.request_resize,
            on_toplevel_request_resize::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
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
                _request_maximize: request_maximize,
                _request_fullscreen: request_fullscreen,
                _request_move: request_move,
                _request_resize: request_resize,
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
        let Some(id) = toplevel_id_of_surface(surface) else {
            return;
        };
        let Some(entry) = (*session).runtime.toplevel_entry(id) else {
            return;
        };
        let base = (*entry.raw.as_ptr()).base;
        if base.is_null() || !(*base).initial_commit {
            return;
        }

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::ToplevelInitialCommit(id), deliver);

        // The decoration default-answer synthesis below reads
        // `take_staged_decoration_mode`/`decoration_answered` *after* the
        // `ToplevelInitialCommit` emit above and assumes that emit delivered
        // synchronously — that the handler has already run, so this state
        // reflects whatever it negotiated. That holds on the only path any
        // public API reaches: a first commit driven from the event loop with
        // no handler on the stack, where `emit` runs `deliver` inline.
        //
        // It does *not* hold if this callback ever fires re-entrantly from
        // inside a running handler (a nested synchronous commit).
        // `Dispatcher::emit` would then see `in_dispatch` already set, queue
        // `ToplevelInitialCommit` for `deliver_all` to run *after* the outer
        // handler returns, and come back here before the handler has run —
        // so synthesizing now would answer from pre-handler state and could
        // re-send a `ServerSide` default over the answer the deferred handler
        // is about to negotiate (the 0.20.8 bug class). Gating on
        // `in_handler()` keeps the synthesis on the synchronous path only; in
        // the deferred case it is skipped and the queued
        // `ToplevelInitialCommit` still reaches the handler through
        // `deliver_all`. No public API produces that nested commit today, so
        // this is latent — the gate is what stops it becoming live.
        if !crate::dispatch::in_handler() {
            // If this toplevel has a decoration, this is the first point at
            // which answering it is safe: `wlr_xdg_surface_schedule_configure`
            // — which both `Runtime::set_decoration_mode` and the client's own
            // `set_mode` request funnel into — asserts `surface->initialized`,
            // and `initial_commit` handling (above `base.initial_commit`'s
            // gate at the top of this function) is exactly what just made that
            // true. See `Runtime::set_decoration_mode`'s own doc for the full
            // argument. Two cases follow from whether a `request_mode` already
            // ran for this decoration before this commit:
            if let Some(mode) = (*session).runtime.take_staged_decoration_mode(id) {
                // It did: the client called `set_mode`/`unset_mode` before
                // committing, `on_decoration_request_mode` already gave the
                // handler its say, and the resulting decision — the handler's,
                // or the dispatch-layer default — was staged rather than sent
                // because the surface was not yet initialized then. Flush it
                // now; `set_decoration_mode` takes the immediate path this
                // time, since `initialized` just went true.
                (*session).runtime.set_decoration_mode(id, mode);
            } else if !(*session).runtime.decoration_answered(id) {
                // It did not, *and* nothing else has answered this decoration
                // either. The guard is `decoration_answered`, not "nothing is
                // staged": the `ToplevelInitialCommit` emitted just above runs
                // with `initialized` already true, so a handler that answers
                // from `initial_commit` sends immediately and leaves `staged`
                // empty — indistinguishable, to a staged-only check, from a
                // decoration nobody has touched. 0.20.8 made exactly that
                // mistake and overrode such an answer with the server-side
                // default one statement later. `answered` latches on both of
                // `set_decoration_mode`'s branches, so it separates the two.
                if let Some(preference) = (*session).runtime.decoration_requested_preference(id) {
                    // A decoration exists (`decoration_requested_preference`
                    // returns `None` for a toplevel without one) but the client
                    // never called `set_mode` — legal, and what a client that
                    // wants the compositor to just decide does. Nothing has
                    // consulted the handler for it yet, so do that now, through
                    // the same event a real `request_mode` would produce.
                    // `deliver_all`'s arm answers it, and — since `initialized`
                    // is already true here — that answer is sent immediately
                    // rather than staged again.
                    (*session).dispatcher.emit(
                        &*session,
                        Event::RequestDecorationMode(id, preference),
                        deliver,
                    );
                }
            }
        }

        // xdg-shell requires an answer to the first commit. The handler may
        // already have staged one through `Runtime::set_toplevel_*`, in which
        // case wlroots coalesces this into that same configure; if it staged
        // nothing, this is what stops the client waiting forever. This also
        // picks up whichever decoration configure the block above just sent,
        // coalesced into the same wire message.
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
        // Removing the entry drops every registration it holds, one of which owns
        // this very `Bound`. `wl_signal_emit_mutable` advances past the
        // firing listener before calling us, so that is what it exists to
        // tolerate; `bound` is dangling from here on and is not touched again.
        (*session).runtime.forget_toplevel(id);
        let listeners = (*session).toplevels.borrow_mut().remove(&id);
        drop(listeners);
        // The toplevel dying first is one of the two orders a decoration and
        // its toplevel can die in; see `RuntimeInner::decorations`'s own
        // doc. `forget_toplevel` just purged the data-side table above —
        // this purges this run's listener registrations for the same id, if
        // any. wlroots *does* eventually free the decoration in this order
        // too — it posts the `orphaned` protocol error and destroys the
        // resource — but not from inside this emission: that happens from
        // wlroots' own `toplevel_destroy` listener on the decoration, which
        // was registered after this crate's (`get_toplevel_decoration` runs
        // after `on_new_toplevel`, and `wl_signal_add` appends), so it
        // fires *after* this callback returns. The decoration is therefore
        // still alive right now, so dropping — and so unlinking — these
        // registrations here is not a use-after-free; it is what makes
        // wlroots' later `assert`-on-empty-listener-lists (see
        // `on_toplevel_decoration_destroy`'s own doc) hold once that
        // listener does run.
        let decoration_listeners = (*session).decorations.borrow_mut().remove(&id);
        drop(decoration_listeners);

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::ToplevelDestroyed(id), deliver);
    }
}

/// A client created a decoration object for an already-announced toplevel.
unsafe extern "C" fn on_new_toplevel_decoration<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener
    // `register_toplevel_and_input` linked into
    // `wlr_xdg_decoration_manager_v1.events.new_toplevel_decoration`, whose
    // `session` is the `*const Session<'_, S>` paired with this
    // instantiation. The signal carries a `*mut wlr_xdg_toplevel_decoration_v1`,
    // live and fully initialised at the point wlroots announces it —
    // including `toplevel`, which is non-null here: a client can only reach
    // `get_toplevel_decoration` by naming a live toplevel resource, and
    // wlroots resolves that reference before emitting this signal.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let decoration = data.cast::<sys::wlr_xdg_toplevel_decoration_v1>();

        let toplevel = (*decoration).toplevel;
        if toplevel.is_null() {
            return;
        }
        let base = (*toplevel).base;
        if base.is_null() {
            return;
        }
        let surface = (*base).surface;
        if surface.is_null() {
            return;
        }
        // The id is looked up, not minted: `on_new_toplevel` already
        // attached one to this surface when the toplevel itself was
        // announced, and a decoration cannot exist for a toplevel that was
        // not — `get_toplevel_decoration` names an existing toplevel
        // resource. A miss here means the surface's id addon is somehow
        // absent, which this callback cannot recover from soundly (there is
        // no `ToplevelId` to key either table by), so the announcement is
        // dropped rather than minting a fresh, disconnected id.
        let Some(id) = toplevel_id_of_surface(surface) else {
            return;
        };
        let Some(raw) = NonNull::new(decoration) else {
            return;
        };

        // Two listeners, both keyed by `Bound::toplevel` rather than by
        // re-deriving the id from `data` at callback time — the same
        // discipline `on_new_toplevel`'s five listeners follow, and for the
        // same underlying reason: `destroy` on this object may fire after
        // its `toplevel` field has already been cleared to null by wlroots,
        // at which point re-deriving the id from `(*decoration).toplevel`
        // would read a null pointer instead of resolving to anything.
        let request_mode = Registration::link_toplevel(
            &raw mut (*decoration).events.request_mode,
            on_decoration_request_mode::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let destroy = Registration::link_toplevel(
            &raw mut (*decoration).events.destroy,
            on_toplevel_decoration_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );

        let displaced = (*session).decorations.borrow_mut().insert(
            id,
            DecorationListeners {
                _request_mode: request_mode,
                _destroy: destroy,
            },
        );
        drop(displaced);

        (*session).runtime.record_decoration(id, raw);

        // A decoration created *after* its toplevel's initial commit has
        // already missed `on_surface_commit`'s "the client never asked"
        // path, which runs once and has run. That ordering is legal: the
        // protocol only forbids `get_toplevel_decoration` once a buffer has
        // been attached, so a client may commit with no buffer, wait for
        // `xdg_surface.configure`, create its decoration, and wait for a
        // decoration configure before attaching one. Such a client that
        // never calls `set_mode` would then wait forever — nothing else
        // would ever answer it. So give the handler its say here instead,
        // through the same event, whenever the surface is already
        // initialized (which is exactly the "we are past that commit" case;
        // when it is false the commit path has not run yet and will handle
        // this decoration itself).
        //
        // `initialized`, not `initial_commit`: the latter is only true
        // during the commit that sets it, and this runs outside any commit.
        //
        // SAFETY: `toplevel_entry` resolves through the same table
        // `on_toplevel_destroy` purges before wlroots frees the toplevel, so
        // a present entry names a live one — and this runs from
        // `get_toplevel_decoration`, which wlroots only dispatches for a
        // toplevel resource the client still holds. `base` is set once at
        // role-object creation and never cleared while the toplevel lives
        // (`configure_toplevel` documents the same claim); it is null-checked
        // here anyway, since this path is reached from a client request
        // rather than from a wlroots callback that guarantees the role is
        // established. `unwrap_or(false)` makes a missing entry mean "not
        // initialized", which skips the emission — the safe direction, since
        // an id with no toplevel has nothing to answer.
        let initialized = (*session)
            .runtime
            .toplevel_entry(id)
            .map(|e| {
                let base = (*e.raw.as_ptr()).base;
                !base.is_null() && (*base).initialized
            })
            .unwrap_or(false);
        if initialized && !(*session).runtime.decoration_answered(id) {
            let requested = (*raw.as_ptr()).requested_mode;
            let preference = crate::decoration::requested_preference(requested);
            let deliver = (*session).deliver;
            (*session).dispatcher.emit(
                &*session,
                Event::RequestDecorationMode(id, preference),
                deliver,
            );
        }
    }
}

/// The client asked for a decoration mode (`set_mode` on
/// `zxdg_toplevel_decoration_v1`, or the implicit request an `unset_mode`
/// leaves behind).
unsafe extern "C" fn on_decoration_request_mode<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_toplevel_decoration` into this decoration's
    // `events.request_mode`, unlinked (from `on_toplevel_decoration_destroy`
    // or `on_toplevel_destroy`) before the decoration or its toplevel is
    // freed. `_data` is deliberately unused, on the same "resolve by id
    // through the table the runtime already tracks" discipline
    // `on_toplevel_request_maximize` documents: the requested mode is read
    // from the decoration entry below, not from whatever wlroots put in
    // `data` for this signal.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).toplevel else { return };

        // The `mode_set_this_dispatch` flag is cleared in `deliver_all`,
        // immediately before it calls the handler — not here. See
        // `DecorationEntry::mode_set_this_dispatch`'s own doc for why that
        // placement, rather than this one, is what keeps clear-then-check
        // correct under deferral and reentrancy.
        let Some(decoration) = (*session).runtime.decoration_ptr(id) else {
            return;
        };
        let requested = (*decoration.as_ptr()).requested_mode;
        let preference = crate::decoration::requested_preference(requested);

        let deliver = (*session).deliver;
        (*session).dispatcher.emit(
            &*session,
            Event::RequestDecorationMode(id, preference),
            deliver,
        );
    }
}

/// The decoration object is about to be freed. Forget it *now*, whatever the
/// handler does — the same "purge before emitting, not after" rule
/// `on_toplevel_destroy` and `on_output_destroy` already follow, applied
/// here even though no event is emitted for this signal at all: there is
/// nothing for [`crate::ToplevelHandler`] to be told (the toplevel itself
/// may still be alive and unaffected), only bookkeeping to do.
///
/// Unlinking both of this decoration's listeners is not optional cleanup:
/// wlroots' own `toplevel_decoration_handle_resource_destroy` emits
/// `events.destroy` and then **asserts both of the decoration's signal
/// listener lists are empty** (`wlr_xdg_decoration_v1.c`). A wrapper that
/// left either of these two linked past this point would abort wlroots on
/// every decoration a client destroys.
unsafe extern "C" fn on_toplevel_decoration_destroy<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_toplevel_decoration` into this decoration's
    // `events.destroy`; the decoration is still alive for the duration of
    // this emission. `_data` unused for the same reason
    // `on_toplevel_set_title` leaves its `_data` unused — the id comes from
    // `Bound::toplevel`, set at link time, since re-deriving it from the
    // decoration's own `toplevel` field would read null if the toplevel
    // already died first.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).toplevel else { return };

        // The decoration dying first is the other of the two orders a
        // decoration and its toplevel can die in; see
        // `RuntimeInner::decorations`'s own doc. This is the half of the
        // purge owned by the decoration side — the toplevel, if still
        // alive, is entirely untouched.
        (*session).runtime.forget_decoration(id);
        let listeners = (*session).decorations.borrow_mut().remove(&id);
        drop(listeners);
    }
}

/// A client created a wlr-layer-shell surface (`get_layer_surface`). Give it
/// an id and a scene tree before anyone is told about it — mirrors
/// `on_new_toplevel` exactly, with `wlr_scene_layer_surface_v1_create` in
/// place of `wlr_scene_xdg_surface_create`, parented straight into the band
/// matching this surface's own [`Layer`] rather than the scene root — see
/// [`Layer`]'s own doc for the banded-tree design this places it into.
unsafe extern "C" fn on_new_layer_surface<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener
    // `register_toplevel_and_input` linked into
    // `wlr_layer_shell_v1.events.new_surface`, whose `session` is the
    // `*const Session<'_, S>` paired with this instantiation. The signal
    // carries a `*mut wlr_layer_surface_v1`, live and fully initialised —
    // its `surface` included — at the point wlroots announces it.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let ls = data.cast::<sys::wlr_layer_surface_v1>();
        let surface = (*ls).surface;
        if surface.is_null() {
            return;
        }

        // The id lives on the surface's addon set, mirroring
        // `on_new_toplevel`'s identical choice and for the identical
        // reason: the role object itself carries no addon set of its own.
        let id = LayerSurfaceId(ensure_id_raw(&raw mut (*surface).addons));

        // No scene to insert into if `init_graphics` was never called.
        // Drop the announcement rather than dereference a null tree — the
        // same choice `on_new_toplevel` makes for the identical situation.
        let layer = Layer::from_raw((*ls).pending.layer.0);
        let Some(band) = (*session).runtime.layer_band_ptr(layer) else {
            return;
        };

        let scene_layer_surface = sys::wlr_scene_layer_surface_v1_create(band.as_ptr(), ls);
        let Some(scene_layer_surface) = NonNull::new(scene_layer_surface) else {
            return;
        };
        let Some(scene_tree) = NonNull::new((*scene_layer_surface.as_ptr()).tree) else {
            // m-2/N-4: this abandons the `wlr_scene_layer_surface_v1` wlroots
            // just allocated on this path, exactly as `on_new_toplevel` does
            // for the identical situation — deliberately, not an oversight.
            //
            // An earlier fix instead called `libc::free` on it, reasoning
            // that disassembly of `wlr_scene_layer_surface_v1_create` in
            // `libwlroots-0.20.so` shows it allocated with a bare
            // `calloc(1, sizeof(*scene_layer_surface))` and no listener
            // linked into it yet on this path — so freeing was sound today.
            // Re-review reverted that: the path is unreachable in the
            // shipped binary (every real way `tree` could end up null
            // already frees the struct and returns null itself, before this
            // crate ever sees a non-null pointer here), so the fix traded a
            // hypothetical leak for a hard runtime dependency (`libc`) added
            // to the published crate on the eve of its freeze, and inverted
            // `on_new_toplevel`'s own precedent for no live benefit. Worse,
            // the trade only gets worse if wlroots ever *does* change shape
            // here: the most plausible way `tree` becomes null on some
            // future version is a reordering that leaves other state
            // (listeners, the subsurface tree) partially set up, and a bare
            // `libc::free` on that is a use-after-free on the next signal
            // emission, not merely a leak. A leak is the safe failure mode
            // for a guard that exists only for a future this crate cannot
            // observe.
            return;
        };
        let Some(raw) = NonNull::new(ls) else {
            return;
        };

        // Four listeners, all with a null liveness flag: each is dropped
        // from inside the destroy emission, while the object is still
        // alive — the same reasoning `on_new_toplevel`'s five listeners
        // document. `commit`/`map`/`unmap` are the base `wlr_surface`'s
        // signals (wlr-layer-shell has no signals of its own for these, the
        // same "role object defers to its surface" shape xdg-shell has),
        // carrying `id` in `Bound::layer` rather than trusting `data` — see
        // that field's own doc for why.
        let commit = Registration::link_layer(
            &raw mut (*surface).events.commit,
            on_layer_surface_commit::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let map = Registration::link_layer(
            &raw mut (*surface).events.map,
            on_layer_surface_map::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let unmap = Registration::link_layer(
            &raw mut (*surface).events.unmap,
            on_layer_surface_unmap::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );
        let destroy = Registration::link_layer(
            &raw mut (*ls).events.destroy,
            on_layer_surface_destroy::<S>,
            (*bound).session,
            std::ptr::null(),
            id,
        );

        let displaced = (*session).layers.borrow_mut().insert(
            id,
            LayerSurfaceListeners {
                _commit: commit,
                _map: map,
                _unmap: unmap,
                _destroy: destroy,
            },
        );
        drop(displaced);

        (*session)
            .runtime
            .record_layer_surface(id, raw, scene_tree, layer);

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::NewLayerSurface(id), deliver);
    }
}

unsafe extern "C" fn on_layer_surface_commit<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_layer_surface` into this surface's
    // `events.commit`, unlinked (from `on_layer_surface_destroy`) before the
    // surface is freed. `_data` unused: the id comes from `Bound::layer`,
    // the same "resolve by the id carried at link time, not by re-deriving
    // it from `data`" discipline `Bound::layer`'s own doc states and that
    // `on_layer_surface_map`/`_unmap`/`_destroy` already follow — commit is
    // brought into line here too (m-1) rather than staying the one
    // exception, even though `data` really is this surface for the commit
    // signal specifically and re-deriving from it was not itself a live
    // bug.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).layer else { return };
        let Some(raw) = (*session).runtime.layer_surface_ptr(id) else {
            return;
        };

        // Reparent into the current layer's band if it changed since this
        // surface was placed (M-2): a client may send `set_layer` after
        // mapping, and the band it lands in is structural, not something a
        // handler can fix after the fact — see [`Layer`]'s own doc. Read
        // from `current`, which this commit has just populated; done before
        // the handler runs so a handler that positions this surface
        // (`Runtime::set_layer_surface_position`) sees it already in its
        // final band.
        let layer = Layer::from_raw((*raw.as_ptr()).current.layer.0);
        (*session)
            .runtime
            .reparent_layer_surface_if_changed(id, layer);

        // Delivered for **every** commit, not only the first — see
        // `Event::LayerSurfaceCommit`'s own doc for why. A handler running
        // from this event sees `initialized` already true (the same
        // argument `on_surface_commit` makes for `base.initialized`), so a
        // `Runtime::configure_layer_surface` call made from inside it takes
        // the immediate branch regardless of what happens below.
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::LayerSurfaceCommit(id), deliver);

        // The first point at which flushing a staged configure is safe —
        // see `layer.rs`'s own module doc for the full argument. If the
        // handler above already answered through
        // `Runtime::configure_layer_surface` (now immediate, since
        // `initialized` is true), that call already cleared this, so
        // `take_staged_layer_configure` finds nothing and this is a no-op
        // rather than a second, stale configure.
        //
        // Gated on `!in_handler()` for the same reason `on_surface_commit`'s
        // decoration synthesis is: this reads `take_staged_layer_configure`
        // *after* the `LayerSurfaceCommit` emit above and assumes it delivered
        // synchronously — that the handler has already had its chance to stage
        // (or immediately send) a configure. That holds on the only path a
        // public API reaches, a commit driven from the event loop with no
        // handler on the stack. If this callback ever fired re-entrantly from
        // inside a running handler, `emit` would queue `LayerSurfaceCommit`
        // for `deliver_all` and return before the handler ran, so flushing
        // here would send a stale (or absent) configure ahead of the answer
        // the deferred handler is about to make. In that case the flush is
        // skipped; the queued `LayerSurfaceCommit` still reaches the handler
        // through `deliver_all`, and any configure it stages is flushed by the
        // next non-nested commit. No public API produces that nested commit
        // today, so this is latent — the gate keeps it that way.
        if !crate::dispatch::in_handler()
            && (*raw.as_ptr()).initial_commit
            && let Some((width, height)) = (*session).runtime.take_staged_layer_configure(id)
        {
            sys::wlr_layer_surface_v1_configure(raw.as_ptr(), width, height);
        }
    }
}

unsafe extern "C" fn on_layer_surface_map<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: as for `on_toplevel_map` — linked by `on_new_layer_surface`
    // into this surface's `events.map`, which wlroots emits with a **null**
    // `data`, so the id comes from `Bound::layer`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).layer else { return };
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::LayerSurfaceMapped(id), deliver);
    }
}

unsafe extern "C" fn on_layer_surface_unmap<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: as for `on_layer_surface_map`, on `events.unmap`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).layer else { return };
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::LayerSurfaceUnmapped(id), deliver);
    }
}

/// A layer surface is about to be freed. Forget it *now*, whatever the
/// handler does — mirrors `on_toplevel_destroy` exactly.
unsafe extern "C" fn on_layer_surface_destroy<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_layer_surface` into the layer surface's own
    // `events.destroy`; the layer surface is still alive for the duration
    // of the emission. `_data` unused: the id comes from `Bound::layer`,
    // the same "resolve by id carried at link time, not by re-deriving it"
    // discipline `on_toplevel_destroy` follows.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).layer else { return };

        // Purge before emitting — the same ordering argument
        // `on_toplevel_destroy` and `on_output_destroy` both make: a
        // destroy queued behind a running handler is delivered long after
        // wlroots freed the object, so resolving at delivery time would
        // dereference freed memory. Clearing here means it simply misses.
        (*session).runtime.forget_layer_surface(id);
        let listeners = (*session).layers.borrow_mut().remove(&id);
        drop(listeners);

        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::LayerSurfaceDestroyed(id), deliver);
    }
}

/// The client asked to (un)maximize.
unsafe extern "C" fn on_toplevel_request_maximize<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_toplevel` into `wlr_xdg_toplevel.events.
    // request_maximize`, unlinked from `on_toplevel_destroy` before the
    // toplevel is freed. `_data` is deliberately unused: this crate reads
    // the requested state through the toplevel entry the runtime already
    // tracks — the same "do not trust `data`'s identity, resolve by id
    // instead" discipline `on_toplevel_set_title` and its siblings already
    // follow, applied here to the *payload* rather than just the id, so
    // this callback makes no claim at all about what wlroots puts in
    // `data` for this signal.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).toplevel else { return };
        let Some(entry) = (*session).runtime.toplevel_entry(id) else {
            return;
        };
        let maximize = (*entry.raw.as_ptr()).requested.maximized;
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::RequestMaximize(id, maximize), deliver);
    }
}

/// The client asked to (un)fullscreen.
unsafe extern "C" fn on_toplevel_request_fullscreen<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: as for `on_toplevel_request_maximize`, on `wlr_xdg_toplevel.
    // events.request_fullscreen`.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).toplevel else { return };
        let Some(entry) = (*session).runtime.toplevel_entry(id) else {
            return;
        };
        let fullscreen = (*entry.raw.as_ptr()).requested.fullscreen;
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::RequestFullscreen(id, fullscreen), deliver);
    }
}

/// Interactive move request.
unsafe extern "C" fn on_toplevel_request_move<S: Handlers>(
    l: *mut sys::wl_listener,
    _data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_toplevel` into `wlr_xdg_toplevel.events.
    // request_move`. `_data` carries a `wlr_xdg_toplevel_move_event` (seat +
    // serial), deliberately ignored: this crate does not forward them to
    // the consumer, by design — see `ToplevelHandler::request_move`'s own
    // doc. Only the id, from `Bound::toplevel`, is needed.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).toplevel else { return };
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::RequestMove(id), deliver);
    }
}

/// Interactive resize request.
unsafe extern "C" fn on_toplevel_request_resize<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_toplevel` into `wlr_xdg_toplevel.events.
    // request_resize`, whose `data` is a live `wlr_xdg_toplevel_resize_event`
    // (`wlr_xdg_shell.h`) for the duration of this emission. Seat and serial
    // are ignored for the same reason `on_toplevel_request_move` ignores
    // them; only `edges` is read. Guarded against null anyway, on the same
    // "an `extern "C"` frame does not get to panic" footing as every other
    // callback in this file — reading a null pointer would abort the
    // process just as surely as a panic would.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let Some(id) = (*bound).toplevel else { return };
        let event = data.cast::<sys::wlr_xdg_toplevel_resize_event>();
        let edges = if event.is_null() {
            crate::Edges::default()
        } else {
            crate::Edges::from_xdg((*event).edges)
        };
        let deliver = (*session).deliver;
        (*session)
            .dispatcher
            .emit(&*session, Event::RequestResize(id, edges), deliver);
    }
}

/// One live input device: its own liveness flag, its keyboard/pointer
/// identity (for pruning `Runtime`'s counts on destroy), and every
/// registration linked against it.
///
/// # Why this exists, and why the destroy path is synchronous
///
/// A device can be unplugged mid-run independently of the backend dying —
/// nothing else in this crate ties a keyboard or pointer's lifetime to
/// anything longer-lived. wlroots 0.20's `wlr_input_device_finish` — called
/// by `wlr_keyboard_finish`/`wlr_pointer_finish`, which every backend's own
/// device-removal path calls — does this, in order (confirmed against
/// wlroots 0.20's own `types/wlr_input_device.c`, `types/wlr_keyboard.c` and
/// `types/wlr_pointer.c`):
///
/// 1. `wl_signal_emit_mutable(&device->events.destroy, device)`
/// 2. `assert(wl_list_empty(&device->events.destroy.listener_list))`
/// 3. (keyboard/pointer only) `assert(wl_list_empty(&kb->events.key...))`,
///    and likewise for `modifiers`/`motion`/`motion_absolute`/`button` and
///    every other per-type signal.
///
/// Both asserts are **live** in the installed libwlroots (confirmed via
/// `nm`/`strings` on the shared library this crate links against — this is
/// not a debug-only check). Merely recording that the device died (clearing
/// a `Cell<bool>`, as this crate's earlier drafts did) does nothing to
/// satisfy either assert: they fire regardless of what *our* code does,
/// because they inspect wlroots' own intrusive lists, not any flag of ours.
/// The only thing that keeps them from firing is unlinking every listener on
/// the device **synchronously, from inside step 1's emission** — which is
/// exactly `on_input_destroy`'s job: it removes this `InputDevice` from
/// `Session::inputs`, and dropping it drops `_destroy` (this very listener —
/// sound because `wl_signal_emit_mutable` has already advanced its cursor
/// past the firing listener, the same idiom `on_toplevel_destroy` and
/// `on_output_destroy` already rely on) and every listener in `_listeners`,
/// unlinking all of them before step 2 or 3 can observe otherwise. Skipping
/// this — as an earlier draft of this file did, relying on `alive` alone —
/// means the first real hot-unplug, or a VT switch (`libinput_suspend`
/// emits `LIBINPUT_EVENT_DEVICE_REMOVED` for every device), reaches
/// `__assert_fail` and aborts the whole compositor process.
///
/// `alive` is kept anyway, as a **backstop only**: if `on_input_destroy`
/// somehow failed to find this entry (a bug in the key it looks up by, say),
/// `alive` at least keeps `Registration::drop` — reached only later, when
/// `Session` itself tears down — from unlinking against memory wlroots may
/// have already freed by then. It is not, and cannot be, a substitute for
/// the synchronous removal above.
struct InputDevice {
    /// This device's keyboard identity, if it is one — recorded so
    /// `on_input_destroy` can call [`Runtime::forget_keyboard`] and keep
    /// [`Runtime::has_keyboard`] truthful once this device is gone.
    keyboard: Option<NonNull<sys::wlr_keyboard>>,
    /// As `keyboard`, for [`Runtime::forget_pointer`]/[`Runtime::has_pointer`].
    pointer: Option<NonNull<sys::wlr_pointer>>,
    _destroy: Registration,
    /// The device's own signals: two for a keyboard (`key`, `modifiers`),
    /// three for a pointer (`motion`, `motion_absolute`, `button`), zero for
    /// a device type this crate does not yet wire up (the destroy watch
    /// above is still linked, so this entry is still found and removed on
    /// destroy even for an ignored device type).
    _listeners: Vec<Registration>,
    /// See this struct's own doc for why this is a backstop, not the
    /// primary mechanism.
    ///
    /// `Rc`, not `Box`, for the same reason [`Backend::alive`] is: the flag's
    /// address must survive this struct moving — `Session::inputs` is a
    /// `HashMap`, which reallocates — because every other field's
    /// `Registration` holds a raw pointer into it.
    ///
    /// **Declared last so it drops last.** `_destroy` and `_listeners` consult
    /// this flag from their own `Registration::drop`; were `alive` to drop
    /// first (freeing the `Cell` it owns), those reads would be a
    /// use-after-free. Field drop runs in declaration order, so keeping this
    /// after every `Registration` that points at it is what makes a bulk drop
    /// of `Session::inputs` (a run ending with a device still attached — e.g.
    /// a keyboard live at shutdown, or an injected virtual keyboard) unlink
    /// cleanly instead of skipping the unlink against a freed flag and
    /// tripping wlroots' `wlr_input_device_finish` list-empty assert.
    #[allow(dead_code)]
    alive: Rc<Cell<bool>>,
}

/// Recompute and push the seat's capabilities from what `runtime` currently
/// has recorded. Called from both `on_new_input` (a device arrived) and
/// `on_input_destroy` (one left) — a seat that still advertises a keyboard
/// or pointer capability after the only one of that kind was unplugged is
/// exactly the same bug `has_keyboard`/`has_pointer` exist to prevent on the
/// way in.
///
/// A no-op with no seat.
///
/// `pub(crate)`, not private: [`Runtime::enable_test_touch`] (`runtime.rs`)
/// also needs to trigger this recompute, from outside this module, the
/// instant the test-touch flag is set — see that method's own doc for why a
/// deferred recompute (waiting for the next device add/remove) is not good
/// enough.
pub(crate) fn update_seat_capabilities(runtime: &Runtime) {
    let Some(seat) = runtime.seat_ptr() else {
        return;
    };
    let mut caps = 0;
    if runtime.has_pointer() {
        caps |= WL_SEAT_CAPABILITY_POINTER;
    }
    if runtime.has_keyboard() {
        caps |= WL_SEAT_CAPABILITY_KEYBOARD;
    }
    if runtime.test_touch_enabled() {
        caps |= WL_SEAT_CAPABILITY_TOUCH;
    }
    // SAFETY: the seat is this runtime's own (created by `create_seat`) and
    // live for as long as this runtime can be used.
    unsafe { sys::wlr_seat_set_capabilities(seat.as_ptr(), caps) };
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
        let count = xkbcommon_sys::xkb_keymap_key_get_syms_by_level(
            keymap,
            xkb_keycode,
            0,
            0,
            &raw mut syms,
        );
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

/// A client created a virtual keyboard. Attach its embedded `wlr_keyboard` to
/// the seat exactly as a physical keyboard from the backend would be, so the
/// seat gains keyboard capability and its enter/key events mint input serials
/// (which `wl_data_device.set_selection` and other serial-gated requests
/// require). Teardown reuses [`on_input_destroy`], keyed by the keyboard's base
/// input device, when the client destroys the virtual keyboard.
unsafe extern "C" fn on_new_virtual_keyboard<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: linked into
    // `wlr_virtual_keyboard_manager_v1.events.new_virtual_keyboard`, whose data
    // is a live `wlr_virtual_keyboard_v1`. Its embedded `keyboard` and that
    // keyboard's `base` input device are live and fully initialised when
    // wlroots announces it; the base's `events.destroy` fires when the client
    // destroys the virtual keyboard, handled by `on_input_destroy` under the
    // same `device as usize` key inserted here.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let vk = data.cast::<sys::wlr_virtual_keyboard_v1>();
        let kb = &raw mut (*vk).keyboard;
        let device = &raw mut (*vk).keyboard.base;
        let runtime = (*session).runtime;

        // Single-seat assumption: the injected keyboard is attached to this
        // runtime's only seat, not to `(*vk).seat` (the seat the client named
        // in `create_virtual_keyboard`). Sound because this crate creates
        // exactly one seat, so the two are necessarily the same. A future
        // multi-seat crate must compare `(*vk).seat` against the target seat
        // here instead of attaching unconditionally.

        let alive = Rc::new(Cell::new(true));
        let destroy = Registration::link_bare(
            &raw mut (*device).events.destroy,
            on_input_destroy::<S>,
            (*bound).session,
            &raw const *alive,
        );

        // No keymap set here: the virtual-keyboard protocol delivers the
        // client's own keymap to `kb`. Serial minting (enter/key) does not
        // depend on a keymap.
        sys::wlr_keyboard_set_repeat_info(kb, 25, 600);
        if let Some(seat) = runtime.seat_ptr() {
            sys::wlr_seat_set_keyboard(seat.as_ptr(), kb);
        }
        let kb_nn = NonNull::new_unchecked(kb);
        runtime.record_keyboard(kb_nn);

        let listeners = vec![
            Registration::link_bare(
                &raw mut (*kb).events.key,
                on_key::<S>,
                (*bound).session,
                &raw const *alive,
            ),
            Registration::link_bare(
                &raw mut (*kb).events.modifiers,
                on_modifiers::<S>,
                (*bound).session,
                &raw const *alive,
            ),
        ];

        // Keyed by the base input-device pointer, exactly as `on_new_input`
        // keys a physical keyboard, so `on_input_destroy` recovers this entry
        // from the `events.destroy` data with no side channel.
        (*session).inputs.borrow_mut().insert(
            device as usize,
            InputDevice {
                alive,
                keyboard: Some(kb_nn),
                pointer: None,
                _destroy: destroy,
                _listeners: listeners,
            },
        );

        update_seat_capabilities(runtime);
    }
}

/// A client created a virtual pointer. Attach its embedded `wlr_pointer` to
/// the cursor exactly as a physical pointer from the backend would be, so the
/// seat gains pointer capability and its motion/button events mint input
/// serials. Teardown reuses [`on_input_destroy`], keyed by the pointer's base
/// input device, when the client destroys the virtual pointer.
unsafe extern "C" fn on_new_virtual_pointer<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: linked into
    // `wlr_virtual_pointer_manager_v1.events.new_virtual_pointer`, whose data
    // is a live `wlr_virtual_pointer_v1_new_pointer_event`. Its `new_pointer`
    // is a live `wlr_virtual_pointer_v1`; that pointer's embedded `pointer`
    // and that pointer's `base` input device are live and fully initialised
    // when wlroots announces it; the base's `events.destroy` fires when the
    // client destroys the virtual pointer, handled by `on_input_destroy`
    // under the same `device as usize` key inserted here.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let event = data.cast::<sys::wlr_virtual_pointer_v1_new_pointer_event>();
        let vp = (*event).new_pointer;
        let pointer = &raw mut (*vp).pointer;
        let device = &raw mut (*vp).pointer.base;
        let runtime = (*session).runtime;

        // Single-seat assumption: the injected pointer is attached to this
        // runtime's only cursor, not routed by `(*event).suggested_seat` (the
        // seat the client suggested). Sound because this crate creates
        // exactly one seat, so the two are necessarily the same. A future
        // multi-seat crate must compare `(*event).suggested_seat` against the
        // target seat here instead of attaching unconditionally.

        let alive = Rc::new(Cell::new(true));
        let destroy = Registration::link_bare(
            &raw mut (*device).events.destroy,
            on_input_destroy::<S>,
            (*bound).session,
            &raw const *alive,
        );

        if let Some(cursor) = runtime.cursor_ptr() {
            sys::wlr_cursor_attach_input_device(cursor.as_ptr(), device);
        }
        let p_nn = NonNull::new_unchecked(pointer);
        runtime.record_pointer(p_nn);

        let listeners = vec![
            Registration::link_bare(
                &raw mut (*pointer).events.motion,
                on_pointer_motion::<S>,
                (*bound).session,
                &raw const *alive,
            ),
            Registration::link_bare(
                &raw mut (*pointer).events.motion_absolute,
                on_pointer_motion_absolute::<S>,
                (*bound).session,
                &raw const *alive,
            ),
            Registration::link_bare(
                &raw mut (*pointer).events.button,
                on_pointer_button::<S>,
                (*bound).session,
                &raw const *alive,
            ),
        ];

        // Keyed by the base input-device pointer, exactly as `on_new_input`
        // keys a physical pointer, so `on_input_destroy` recovers this entry
        // from the `events.destroy` data with no side channel.
        (*session).inputs.borrow_mut().insert(
            device as usize,
            InputDevice {
                keyboard: None,
                pointer: Some(p_nn),
                _destroy: destroy,
                _listeners: listeners,
                alive,
            },
        );

        update_seat_capabilities(runtime);
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
        // `Backend::autocreate`'s own death watch — and, unlike that one,
        // this is load-bearing for more than the flag: see `InputDevice`'s
        // own doc for why `on_input_destroy` (not `on_backend_destroy`,
        // which this device-watch reused before this was found unsound)
        // must unlink this device's other listeners synchronously, from
        // inside the very emission this registers for.
        let destroy = Registration::link_bare(
            &raw mut (*device).events.destroy,
            on_input_destroy::<S>,
            (*bound).session,
            &raw const *alive,
        );

        let mut listeners = Vec::new();
        let mut keyboard = None;
        let mut pointer = None;

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
                    let kb_nn = NonNull::new_unchecked(kb);
                    runtime.record_keyboard(kb_nn);
                    keyboard = Some(kb_nn);

                    listeners.push(Registration::link_bare(
                        &raw mut (*kb).events.key,
                        on_key::<S>,
                        (*bound).session,
                        &raw const *alive,
                    ));
                    listeners.push(Registration::link_bare(
                        &raw mut (*kb).events.modifiers,
                        on_modifiers::<S>,
                        (*bound).session,
                        &raw const *alive,
                    ));
                }
            }
            sys::wlr_input_device_type::WLR_INPUT_DEVICE_POINTER => {
                if let Some(cursor) = runtime.cursor_ptr() {
                    sys::wlr_cursor_attach_input_device(cursor.as_ptr(), device);
                }
                let raw_pointer = sys::wlr_pointer_from_input_device(device);
                if !raw_pointer.is_null() {
                    let p_nn = NonNull::new_unchecked(raw_pointer);
                    runtime.record_pointer(p_nn);
                    pointer = Some(p_nn);

                    listeners.push(Registration::link_bare(
                        &raw mut (*raw_pointer).events.motion,
                        on_pointer_motion::<S>,
                        (*bound).session,
                        &raw const *alive,
                    ));
                    listeners.push(Registration::link_bare(
                        &raw mut (*raw_pointer).events.motion_absolute,
                        on_pointer_motion_absolute::<S>,
                        (*bound).session,
                        &raw const *alive,
                    ));
                    listeners.push(Registration::link_bare(
                        &raw mut (*raw_pointer).events.button,
                        on_pointer_button::<S>,
                        (*bound).session,
                        &raw const *alive,
                    ));
                }
            }
            _ => {}
        }

        // Keyed by the device pointer's own address, which is exactly the
        // `data` `on_input_destroy` receives (`wlr_input_device_finish`
        // emits `events.destroy` with `wlr_device` itself as data — the
        // same pointer this signal was linked against), so that callback
        // recovers this same key with no side channel needed.
        (*session).inputs.borrow_mut().insert(
            device as usize,
            InputDevice {
                alive,
                keyboard,
                pointer,
                _destroy: destroy,
                _listeners: listeners,
            },
        );

        // Capabilities are recomputed rather than accumulated: a seat that
        // advertises a keyboard or pointer it does not have makes clients
        // wait for input that never arrives.
        update_seat_capabilities(runtime);
    }
}

/// A device is being removed — unplugged, or a VT switch's
/// `libinput_suspend` tearing every device down. Unlink every one of this
/// device's listeners **now**, synchronously, before returning: see
/// [`InputDevice`]'s own doc for why wlroots aborts the process if this
/// doesn't happen before this emission returns.
unsafe extern "C" fn on_input_destroy<S: Handlers>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: linked by `on_new_input` into a device's own `events.destroy`.
    // `data` is the same `*mut wlr_input_device` `wlr_input_device_finish`
    // was called on — confirmed against wlroots 0.20's own
    // `types/wlr_input_device.c`, which emits with `wlr_device` itself as
    // the signal data — and so, cast to `usize`, recovers the exact key
    // `on_new_input` inserted this entry under. The device (and the
    // keyboard/pointer struct embedding it, if any) is still live memory at
    // this point: `wlr_input_device_finish`'s own asserts, which this
    // exists to satisfy, run *after* this emission returns, and freeing the
    // struct is the removing backend's job, done only after `*_finish`
    // itself returns.
    unsafe {
        let bound = bound_of(l);
        let session = (*bound).session.cast::<Session<'_, S>>();
        let runtime = (*session).runtime;
        let key = data as usize;

        // Removed, not merely looked up: dropping the entry (below) unlinks
        // `_destroy` — this very listener, sound for the same reason
        // `on_toplevel_destroy` unlinking itself is (`wl_signal_emit_mutable`
        // has already advanced its cursor past the firing listener) — and
        // every listener in `_listeners`, all before this call returns.
        let removed = (*session).inputs.borrow_mut().remove(&key);
        if let Some(entry) = removed {
            if let Some(kb) = entry.keyboard {
                runtime.forget_keyboard(kb);
            }
            if let Some(p) = entry.pointer {
                runtime.forget_pointer(p);
            }
            drop(entry);
        }

        update_seat_capabilities(runtime);
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
        (*session).runtime.notify_seat_activity();
        let Some(seat) = (*session).runtime.seat_ptr() else {
            return;
        };
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

        // Reset *before* `emit`, not read stale after it: `emit` only calls
        // `deliver_all` (which is what sets this to the handler's actual
        // answer) synchronously when no other handler is already running.
        // If this key is deferred — queued behind one that is — `emit`
        // returns immediately without touching the flag at all, and it
        // would otherwise still hold whatever the *previous* key's answer
        // was. A previous key that was consumed would then silently drop
        // this one on the floor instead of forwarding it, contradicting the
        // "a deferred key is forwarded" guarantee below. Defaulting to
        // "not consumed" here is what makes that guarantee actually true:
        // a deferred key is forwarded unless and until `deliver_all` says
        // otherwise.
        (*session).last_key_consumed.set(false);

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
        let Some(seat) = (*session).runtime.seat_ptr() else {
            return;
        };
        let kb = sys::wlr_seat_get_keyboard(seat.as_ptr());
        if kb.is_null() {
            return;
        }
        sys::wlr_seat_keyboard_notify_modifiers(seat.as_ptr(), &raw mut (*kb).modifiers);
    }
}

/// Reconcile pointer-constraint activation with the surface that just gained
/// pointer focus. Called from the single pointer-focus chokepoint
/// ([`enter_surface_under_cursor`]) so activation tracks focus exactly.
///
/// First, if a constraint is active on a *different* surface than `focused`,
/// deactivate it: send its `deactivated` event and clear `active_constraint`.
/// When that constraint was a **lock** carrying an enabled cursor hint, warp
/// the cursor to the hint first — the client's requested resting place for the
/// pointer once the lock releases. (If the active constraint is already on
/// `focused`, nothing changes and no re-activation is attempted.)
///
/// Then, if `focused` has a stored constraint for this `seat`, activate it:
/// send its `activated` event and record it as `active_constraint`. This is
/// gated on [`Runtime::is_session_locked`] — while the session is locked the
/// lock surface owns focus and no client constraint may take the pointer, a
/// defensive guard on top of the focus routing that already keeps a normal
/// surface from being entered under a lock.
///
/// # Safety
///
/// `session` is this callback's own live `Session`; `focused` is a surface
/// from this runtime's scene or null; `seat` must be a live `wlr_seat`.
unsafe fn update_pointer_constraint_activation<S: Handlers>(
    session: *const Session<'_, S>,
    focused: *mut sys::wlr_surface,
    seat: *mut sys::wlr_seat,
) {
    // SAFETY: `session` is live per the contract; the constraints it references
    // are live for as long as they remain in the map (their destroy handler
    // removes them synchronously).
    unsafe {
        let runtime = (*session).runtime;

        // Deactivate the active constraint if focus moved off its surface.
        if let Some(active) = runtime.active_constraint() {
            if (*active.as_ptr()).surface == focused {
                // Same surface still focused — the active constraint stands.
                return;
            }
            // Locked with an enabled cursor hint: park the cursor at the hint
            // before releasing the lock.
            if (*active.as_ptr()).type_
                == sys::wlr_pointer_constraint_v1_type::WLR_POINTER_CONSTRAINT_V1_LOCKED
                && (*active.as_ptr()).current.cursor_hint.enabled
            {
                let hx = (*active.as_ptr()).current.cursor_hint.x;
                let hy = (*active.as_ptr()).current.cursor_hint.y;
                runtime.warp_cursor(hx, hy);
            }
            sys::wlr_pointer_constraint_v1_send_deactivated(active.as_ptr());
            runtime.inner.active_constraint.set(None);
        }

        // Do not activate onto no surface, nor while the session is locked.
        if focused.is_null() || runtime.is_session_locked() {
            return;
        }

        // Activate a constraint stored for `focused` on this seat, if any.
        let candidate = {
            let map = (*session).pointer_constraints.borrow();
            map.values()
                .map(|entry| entry.constraint)
                .find(|c| (*c.as_ptr()).surface == focused && (*c.as_ptr()).seat == seat)
        };
        if let Some(c) = candidate {
            sys::wlr_pointer_constraint_v1_send_activated(c.as_ptr());
            runtime.inner.active_constraint.set(Some(c));
        }
    }
}

/// Move the pointer focus and forward a motion to whatever the cursor is
/// over, or clear pointer focus if it is over nothing. Shared by
/// `on_pointer_motion`, `on_pointer_motion_absolute` and `on_pointer_button`
/// rather than factored differently: each caller already has the seat and
/// the cursor's current position in hand, and this only names the repeated
/// "find the surface under the cursor and enter/clear-focus it" step, not
/// the forwarding call itself (which differs per caller: motion sends a
/// motion, a button sends nothing here at all).
///
/// Routed through [`Runtime::leaf_surface_at`], **not**
/// [`Runtime::toplevel_at`]: the latter answers "which window, and where in
/// that window", and the window's root surface is not the surface that was
/// struck whenever the hit lands on a popup or a subsurface. Using it here
/// would notify the toplevel's root surface for a click that belongs to a
/// popup — which gets no input at all — and at a window-relative offset
/// rather than the struck surface's own. `leaf_surface_at` resolves the
/// actual struck surface, so this always notifies the right one.
///
/// # Safety
///
/// `seat` must be a live `wlr_seat`.
unsafe fn enter_surface_under_cursor<S: Handlers>(
    session: *const Session<'_, S>,
    seat: *mut sys::wlr_seat,
    x: f64,
    y: f64,
    time_msec: u32,
) {
    // SAFETY: `session` is this callback's own live `Session`; `runtime`
    // outlives the call.
    let runtime = unsafe { (*session).runtime };
    let focused: *mut sys::wlr_surface = match runtime.leaf_surface_at(x, y) {
        // SAFETY: `leaf_surface_at` reads `surface` out of a live
        // `wlr_scene_surface` found in this runtime's own scene, which
        // outlives the call; the seat is live per this function's contract.
        Some((surface, sx, sy)) => unsafe {
            sys::wlr_seat_pointer_notify_enter(seat, surface, sx, sy);
            sys::wlr_seat_pointer_notify_motion(seat, time_msec, sx, sy);
            surface
        },
        // SAFETY: as above.
        None => unsafe {
            sys::wlr_seat_pointer_notify_clear_focus(seat);
            std::ptr::null_mut()
        },
    };
    // This is the single pointer-focus chokepoint, so pointer-constraint
    // activation tracks focus exactly: whatever surface (or nothing) the
    // cursor just entered is what a constraint may activate on.
    // SAFETY: `session`, `focused` (a surface from this runtime's scene or
    // null), and `seat` are all live for this call.
    unsafe { update_pointer_constraint_activation(session, focused, seat) };
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
        runtime.notify_seat_activity();
        let Some(cursor) = runtime.cursor_ptr() else {
            return;
        };

        let device = &raw mut (*(*ev).pointer).base;
        let (dx, dy) = ((*ev).delta_x, (*ev).delta_y);
        let (udx, udy) = ((*ev).unaccel_dx, (*ev).unaccel_dy);
        let time_msec = (*ev).time_msec;

        // Pointer-constraint enforcement, applied to the *absolute* cursor
        // motion before it is committed. A relative-pointer client still gets
        // the raw deltas afterward, constraint or not.
        let active = runtime.active_constraint();
        let locked = matches!(active, Some(c)
            if (*c.as_ptr()).type_
                == sys::wlr_pointer_constraint_v1_type::WLR_POINTER_CONSTRAINT_V1_LOCKED);
        match active {
            // LOCKED: the cursor is frozen. Do not move it at all — skip the
            // `wlr_cursor_move` entirely — but still emit relative motion below
            // (a locked pointer is exactly what a relative-pointer client wants).
            Some(_) if locked => {}
            // CONFINED: clamp the intended motion into the region.
            Some(c) => {
                let region = &raw const (*c.as_ptr()).region;
                let (cx0, cy0) = ((*cursor.as_ptr()).x, (*cursor.as_ptr()).y);
                let (nx, ny) = (cx0 + dx, cy0 + dy);
                match runtime.confine_motion(region, cx0, cy0, nx, ny) {
                    // Old position was inside the region: warp to the confined
                    // point (never `wlr_cursor_move`, which would ignore the
                    // clamp).
                    Some((cxc, cyc)) => runtime.warp_cursor(cxc, cyc),
                    // Old position outside the region — `confine` returned
                    // `false` and left its out-params untouched. Re-anchor the
                    // cursor into the region rather than "keeping it put",
                    // which would wedge every future motion (a client can force
                    // this via `set_region`; the set-region handler re-anchors
                    // too, making this arm belt-and-suspenders).
                    None => {
                        runtime.reanchor_cursor_into_region(c);
                    }
                }
            }
            // Unconstrained: the ordinary path.
            None => sys::wlr_cursor_move(cursor.as_ptr(), device, dx, dy),
        }

        // Always, on every motion regardless of constraint: forward the raw
        // deltas to relative-pointer clients.
        if let Some(seat) = runtime.seat_ptr() {
            runtime.send_relative_pointer_motion(seat.as_ptr(), time_msec, dx, dy, udx, udy);
        }

        // A locked pointer produced no absolute motion: nothing to reposition,
        // notify, or re-enter. Everything below concerns the moved cursor.
        if locked {
            return;
        }

        runtime.ensure_cursor_image();

        let (x, y) = ((*cursor.as_ptr()).x, (*cursor.as_ptr()).y);

        // `wlr_scene_drag_icon` does not self-track the cursor (see
        // `Runtime::drag_icon_position`'s doc for the full story) — this is
        // the fix, called with the cursor's own freshly-updated layout
        // position. `.round()`, not truncation: a truncated `x as i32` on a
        // sub-pixel cursor position left the icon a pixel short of the
        // cursor in the downstream prototype that found this bug.
        runtime.reposition_drag_icon(x.round() as i32, y.round() as i32);

        let deliver = (*session).deliver;
        (*session).dispatcher.emit(
            &*session,
            Event::PointerMotion {
                x_milli: (x * 1000.0) as i64,
                y_milli: (y * 1000.0) as i64,
                time_msec,
            },
            deliver,
        );

        if let Some(seat) = runtime.seat_ptr() {
            enter_surface_under_cursor(session, seat.as_ptr(), x, y, time_msec);
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
        runtime.notify_seat_activity();
        let Some(cursor) = runtime.cursor_ptr() else {
            return;
        };

        let device = &raw mut (*(*ev).pointer).base;
        let time_msec = (*ev).time_msec;

        // Where this absolute event *intends* to put the cursor, in layout
        // coordinates: the event carries normalised 0..1 coordinates, so ask
        // wlroots to map them exactly as `wlr_cursor_warp_absolute` would.
        let (cx0, cy0) = ((*cursor.as_ptr()).x, (*cursor.as_ptr()).y);
        let (mut lx, mut ly) = (cx0, cy0);
        sys::wlr_cursor_absolute_to_layout_coords(
            cursor.as_ptr(),
            device,
            (*ev).x,
            (*ev).y,
            &mut lx,
            &mut ly,
        );
        // Deltas for relative-pointer clients: the intended layout position
        // minus the current one. An absolute event carries no separate
        // unaccelerated delta, so the same delta feeds both.
        let (dx, dy) = (lx - cx0, ly - cy0);

        let active = runtime.active_constraint();
        let locked = matches!(active, Some(c)
            if (*c.as_ptr()).type_
                == sys::wlr_pointer_constraint_v1_type::WLR_POINTER_CONSTRAINT_V1_LOCKED);
        match active {
            // LOCKED: freeze — skip the warp entirely.
            Some(_) if locked => {}
            // CONFINED: clamp the intended target into the region before
            // warping (the confine keys off the current position, which the
            // invariant keeps inside the region; if it has slipped out, the
            // `None` arm re-anchors).
            Some(c) => {
                let region = &raw const (*c.as_ptr()).region;
                match runtime.confine_motion(region, cx0, cy0, lx, ly) {
                    Some((cxc, cyc)) => runtime.warp_cursor(cxc, cyc),
                    None => {
                        runtime.reanchor_cursor_into_region(c);
                    }
                }
            }
            // Unconstrained: the ordinary absolute warp.
            None => sys::wlr_cursor_warp_absolute(cursor.as_ptr(), device, (*ev).x, (*ev).y),
        }

        if let Some(seat) = runtime.seat_ptr() {
            runtime.send_relative_pointer_motion(seat.as_ptr(), time_msec, dx, dy, dx, dy);
        }

        if locked {
            return;
        }

        runtime.ensure_cursor_image();

        let (x, y) = ((*cursor.as_ptr()).x, (*cursor.as_ptr()).y);

        // See `on_pointer_motion`'s identical call for why this is here and
        // why `.round()`.
        runtime.reposition_drag_icon(x.round() as i32, y.round() as i32);

        let deliver = (*session).deliver;
        (*session).dispatcher.emit(
            &*session,
            Event::PointerMotion {
                x_milli: (x * 1000.0) as i64,
                y_milli: (y * 1000.0) as i64,
                time_msec,
            },
            deliver,
        );

        if let Some(seat) = runtime.seat_ptr() {
            enter_surface_under_cursor(session, seat.as_ptr(), x, y, time_msec);
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
        runtime.notify_seat_activity();
        let Some(cursor) = runtime.cursor_ptr() else {
            return;
        };
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
            enter_surface_under_cursor(session, seat.as_ptr(), x, y, (*ev).time_msec);
            sys::wlr_seat_pointer_notify_button(
                seat.as_ptr(),
                (*ev).time_msec,
                (*ev).button,
                (*ev).state,
            );
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
        | Event::RequestMaximize(..)
        | Event::RequestFullscreen(..)
        | Event::RequestMove(..)
        | Event::RequestResize(..)
        | Event::RequestDecorationMode(..)
        | Event::NewLayerSurface(..)
        | Event::LayerSurfaceCommit(..)
        | Event::LayerSurfaceMapped(..)
        | Event::LayerSurfaceUnmapped(..)
        | Event::LayerSurfaceDestroyed(..)
        | Event::Key { .. }
        | Event::PointerMotion { .. }
        | Event::PointerButton { .. }
        // Unreachable: `run` never registers a session-lock manager either
        // (`Backend::register_toplevel_and_input` is `run_all`'s hook; `run`
        // uses `no_extra`), so this cannot be produced on this path.
        | Event::SessionLockChanged(..) => {}
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

            let reg = Registration::link_bare(
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
            let _reg = unsafe { Registration::link_bare(signal, noop, std::ptr::null(), alive) };
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
            let reg = Registration::link_bare(
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
            let reg = Registration::link_bare(
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
                decorations: RefCell::new(HashMap::new()),
                layers: RefCell::new(HashMap::new()),
                inputs: RefCell::new(HashMap::new()),
                drags: RefCell::new(HashMap::new()),
                idle_inhibitors: RefCell::new(HashMap::new()),
                session_locks: RefCell::new(HashMap::new()),
                lock_surfaces: RefCell::new(HashMap::new()),
                pointer_constraints: RefCell::new(HashMap::new()),
                last_key_consumed: Cell::new(false),
                runtime: &runtime,
                deliver: deliver::<Recorder>,
            };
            let mut h = Harness::new();
            let hp = &raw mut *h;

            // Declared after `session`, so it unlinks while the session it
            // names is still alive — the ordering `run` uses.
            let _reg = Registration::link_bare(
                &raw mut (*hp).signal,
                on_new_output::<Recorder>,
                (&raw const session).cast::<()>(),
                &raw const (*hp).alive,
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
            decorations: RefCell::new(HashMap::new()),
            layers: RefCell::new(HashMap::new()),
            inputs: RefCell::new(HashMap::new()),
            drags: RefCell::new(HashMap::new()),
            idle_inhibitors: RefCell::new(HashMap::new()),
            session_locks: RefCell::new(HashMap::new()),
            lock_surfaces: RefCell::new(HashMap::new()),
            pointer_constraints: RefCell::new(HashMap::new()),
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

    /// Pins the `wl_seat.capability` bit values this module's own doc
    /// comment claims are core-Wayland-protocol-stable, per `wayland.xml`'s
    /// `wl_seat::capability` enum (`pointer = 1`, `keyboard = 2`, `touch =
    /// 4`) — checked here rather than trusted from that comment alone, the
    /// same reasoning `seat.rs`'s `modifier_bit_values_match_wlroots_headers`
    /// already applies to the `wlr_keyboard_modifier` bits.
    #[test]
    fn capability_bits_match_the_wayland_protocol() {
        assert_eq!(WL_SEAT_CAPABILITY_POINTER, 1);
        assert_eq!(WL_SEAT_CAPABILITY_KEYBOARD, 2);
        assert_eq!(WL_SEAT_CAPABILITY_TOUCH, 4);
    }

    /// [`Runtime::enable_test_touch`] must make a seat that already exists
    /// start advertising the touch capability immediately, not only on the
    /// next unrelated device add/remove — this is what
    /// `crates/wlr/tests/seat.rs` cannot observe itself, since `wlr_seat` and
    /// its `capabilities` field are not part of this crate's public surface.
    /// Reads `wlr_seat.capabilities` directly, the same field
    /// `wlr_seat_set_capabilities` writes, so this exercises exactly what
    /// `update_seat_capabilities` does rather than this crate's own
    /// bookkeeping of it.
    #[test]
    fn enable_test_touch_sets_the_capability_bit_immediately() {
        let display = crate::Display::new().expect("display");
        let runtime = crate::Runtime::new().expect("runtime");
        runtime.create_seat(&display, "seat0").expect("seat");

        let seat = runtime.seat_ptr().expect("seat was just created");
        // SAFETY: `seat` is this runtime's own live seat, just created above.
        assert_eq!(
            unsafe { (*seat.as_ptr()).capabilities } & WL_SEAT_CAPABILITY_TOUCH,
            0,
            "a fresh seat with no devices and no enable_test_touch call must \
             not advertise touch"
        );

        runtime.enable_test_touch();

        // SAFETY: same seat, still live.
        assert_eq!(
            unsafe { (*seat.as_ptr()).capabilities } & WL_SEAT_CAPABILITY_TOUCH,
            WL_SEAT_CAPABILITY_TOUCH,
            "enable_test_touch must set the touch bit without waiting for a \
             device add/remove to trigger the next recompute"
        );
    }

    /// Reproduces, at the smallest possible scale and with no `Session`,
    /// `Backend` or `Display` involved, the exact mechanism
    /// `on_input_destroy` relies on to keep a hot-unplug from aborting the
    /// process.
    ///
    /// wlroots' own `wlr_keyboard_finish` (confirmed against wlroots 0.20's
    /// `types/wlr_keyboard.c`) does, in order: emit `kb->base.events.destroy`
    /// (via `wlr_input_device_finish`), assert that signal's listener list is
    /// now empty, then assert `kb->events.key`/`.modifiers`/`.keymap`/
    /// `.repeat_info`'s listener lists are all empty too — with nothing in
    /// between the emission and those asserts that could unlink anything on
    /// our behalf. A destroy watch that only records the death (clears a
    /// flag) leaves every one of those lists non-empty and every assert
    /// fires, aborting the process — there is no way for a test to catch
    /// that and report a clean failure; the symptom is the test binary
    /// itself dying, which is exactly what a real hot-unplug or VT switch
    /// would do to a compositor built the same way.
    ///
    /// This test builds a bare `wlr_keyboard` with `wlr_keyboard_init` (no
    /// backend needed — it only initialises the struct and its own
    /// signals), links `key` and `modifiers` listeners the way `on_new_input`
    /// does, and links a destroy watch that removes and drops all three
    /// `Registration`s — itself included — synchronously, from inside the
    /// destroy emission, exactly as `on_input_destroy` does for a real
    /// device. If that is sufficient, `wlr_keyboard_finish` returns
    /// normally and this test passes; if it is not, the process aborts and
    /// no assertion in this function is ever reached.
    #[test]
    fn a_synchronous_destroy_watch_satisfies_wlroots_finish_asserts() {
        /// What the destroy watch reaches through `Bound::session` to unlink
        /// every registration on this keyboard — itself included, the same
        /// way `on_input_destroy` drops the `InputDevice` entry that owns
        /// its own `_destroy` registration.
        struct KbRegistrations {
            key: Option<Registration>,
            modifiers: Option<Registration>,
            destroy: Option<Registration>,
        }

        unsafe extern "C" fn on_kb_destroy(l: *mut sys::wl_listener, _data: *mut std::ffi::c_void) {
            // SAFETY: linked below into `kb.base.events.destroy`, whose
            // `session` is the `*const RefCell<KbRegistrations>` set up
            // before `wlr_keyboard_finish` is called.
            unsafe {
                let bound = bound_of(l);
                let regs = (*bound).session.cast::<RefCell<KbRegistrations>>();
                let mut regs = (*regs).borrow_mut();
                // Dropping each `Registration` unlinks it. `destroy` last,
                // and its drop is unlinking the very listener this callback
                // is running through — sound because `wl_signal_emit_mutable`
                // has already advanced its cursor past it before calling us,
                // the same idiom `on_toplevel_destroy`/`on_output_destroy`/
                // `on_input_destroy` all rely on elsewhere in this file.
                regs.key.take();
                regs.modifiers.take();
                regs.destroy.take();
            }
        }

        // SAFETY: `kb` is exclusively owned by this test and never moves
        // again after this point; `wlr_keyboard_init` fully overwrites it
        // (`*kb = (struct wlr_keyboard){ .. }` in wlroots' own source), so
        // the zeroed value handed in is never read before being replaced.
        let mut kb: sys::wlr_keyboard = unsafe { std::mem::zeroed() };
        let kb_impl = sys::wlr_keyboard_impl {
            name: c"test-keyboard".as_ptr(),
            led_update: None,
        };
        // SAFETY: `kb` and `kb_impl` are both live, exclusively-owned locals
        // for the rest of this function.
        unsafe { sys::wlr_keyboard_init(&raw mut kb, &raw const kb_impl, c"test".as_ptr()) };

        let regs = RefCell::new(KbRegistrations {
            key: None,
            modifiers: None,
            destroy: None,
        });
        let regs_ptr = (&raw const regs).cast::<()>();

        // SAFETY: `kb.events.key`/`.modifiers` are initialised by
        // `wlr_keyboard_init` above and live for the rest of this function;
        // `noop` matches every registration's `S`-free signature (there is
        // no `Session<S>` in this test at all, so `session` is null for
        // these two — `noop` never reads it).
        let key = unsafe {
            Registration::link_bare(
                &raw mut kb.events.key,
                noop,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        let modifiers = unsafe {
            Registration::link_bare(
                &raw mut kb.events.modifiers,
                noop,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        // SAFETY: `kb.base.events.destroy` is initialised by
        // `wlr_input_device_init` (called from `wlr_keyboard_init` above)
        // and live for the rest of this function; `regs_ptr` outlives the
        // registration (it is a local that is not dropped until after
        // `wlr_keyboard_finish` returns, below).
        let destroy = unsafe {
            Registration::link_bare(
                &raw mut kb.base.events.destroy,
                on_kb_destroy,
                regs_ptr,
                std::ptr::null(),
            )
        };

        {
            let mut regs = regs.borrow_mut();
            regs.key = Some(key);
            regs.modifiers = Some(modifiers);
            regs.destroy = Some(destroy);
        }

        // The whole point: if `on_kb_destroy` did not unlink synchronously,
        // this call aborts the process (via wlroots' own `assert`) and
        // nothing after it ever runs.
        //
        // SAFETY: `kb` is fully initialised and has no pressed keys
        // (`num_keycodes` is 0, so `wlr_keyboard_finish`'s "release pressed
        // keys" loop is a no-op and never touches a seat), and nothing else
        // holds a reference to it.
        unsafe { sys::wlr_keyboard_finish(&raw mut kb) };

        // Reaching this line at all is the proof. The registrations were
        // already taken (and so already dropped/unlinked) by `on_kb_destroy`
        // during the call above; asserting that here is a belt-and-braces
        // check on this test's own setup, not part of what's being proven.
        assert!(regs.borrow().key.is_none());
        assert!(regs.borrow().modifiers.is_none());
        assert!(regs.borrow().destroy.is_none());
    }
}
