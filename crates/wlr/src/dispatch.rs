//! Event delivery, and the reentrancy guard that makes it sound.
//!
//! One `&mut S` reaches handlers. wlroots emits signals synchronously from
//! inside API calls, so a handler that destroys an object re-enters dispatch
//! while that `&mut S` is still live — which aliases `&mut` and is undefined
//! behaviour. The dispatcher detects reentrancy and queues the inner event,
//! draining the queue once the outer handler returns.
//!
//! Two consequences fall out of that and are deliberate:
//!
//! 1. Deferred events carry an [`OutputId`], never a handle, because the object
//!    may be destroyed before delivery. Delivery re-resolves and drops silently
//!    if it is gone.
//! 2. Anything wlroots requires the compositor to complete *before* a callback
//!    returns is delivered late when it is deferred, and this crate has no
//!    exemption for it. There is exactly one such path today —
//!    [`crate::OutputHandler::frame`], which wlroots expects to render before
//!    its emission returns — and it is deferred along with everything else.
//!
//!    That is forced, not a shortcut. The exemption would have to deliver
//!    directly while a handler is running, which is precisely the second
//!    `&mut S` this module exists to prevent; a rendering deadline does not
//!    outrank undefined behaviour. So no never-deferred category exists here,
//!    and none can be added without changing the model. `frame`'s own
//!    documentation states the consequence for consumers, and any future path
//!    with the same requirement inherits the same answer.
//!
//! Deferral is only half of what a running handler needs, though. It keeps the
//! `&mut S` unaliased; it does nothing about the *handles* a handler is holding.
//! A handler given `&Output<'h>` cannot let it escape the call, but it can drive
//! the event loop from inside the call — `EventLoop::dispatch` is public, safe,
//! takes `&self`, and any `&Display` yields an `EventLoop` — and wlroots is then
//! free to destroy and free that very output while the handler still holds the
//! handle. No `unsafe` appears anywhere in that sequence. So a second,
//! thread-scoped flag ([`IN_HANDLER`]) records that a handler is on the stack,
//! and `EventLoop::dispatch` refuses while it is set. The lifetime bounds where
//! a handle may travel; this bounds what may happen underneath it.
//!
//! The drain loop below is intentionally unbounded: a handler that queues a
//! new event on every delivery livelocks rather than terminating. That is the
//! correct behaviour for real wlroots traffic (it is bounded by however many
//! objects a compositor can actually destroy in a chain), not an oversight.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use crate::{DecorationMode, Edges, LayerSurfaceId, NodeId, OutputId, SceneOutputId, ToplevelId};

/// An event awaiting delivery.
///
/// Carries ids rather than handles precisely because a deferred event may name
/// an object that no longer exists by the time it is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    NewOutput(OutputId),
    OutputFrame(OutputId),
    OutputDestroyed(OutputId),

    /// The runtime's renderer emitted `events.lost`: the GPU was reset and
    /// everything derived from that renderer is invalid.
    ///
    /// Carries nothing, because there is nothing to carry — the run has exactly
    /// one renderer, and wlroots passes the renderer itself as the signal's
    /// data, which this crate must not turn into a handle for the usual reason
    /// (a deferred event may name a dead object).
    RendererLost,

    /// An fd source is ready. Carries the readiness mask rather than a
    /// [`Readiness`](crate::Readiness) so the enum stays `Copy` and `Eq`
    /// without exporting a public type into a private one.
    FdReady(crate::SourceId, u32),

    NewToplevel(ToplevelId),
    ToplevelInitialCommit(ToplevelId),
    ToplevelMapped(ToplevelId),
    ToplevelUnmapped(ToplevelId),
    ToplevelTitleChanged(ToplevelId),
    ToplevelDestroyed(ToplevelId),

    /// A client requested (un)maximize. The bool is the requested state —
    /// read from `wlr_xdg_toplevel::requested.maximized` at emission time,
    /// not re-read at delivery, so a deferred event still reports what the
    /// client actually asked for.
    RequestMaximize(ToplevelId, bool),
    /// As `RequestMaximize`, for `requested.fullscreen`.
    RequestFullscreen(ToplevelId, bool),
    /// Interactive move. Seat and serial are not carried — see
    /// `ToplevelHandler::request_move`'s own doc for why.
    RequestMove(ToplevelId),
    /// Interactive resize. As `RequestMove`, minus the edges the client
    /// reported dragging.
    RequestResize(ToplevelId, Edges),

    /// The client asked for a decoration mode on a toplevel's
    /// `zxdg_toplevel_decoration_v1`. The `Option<DecorationMode>` is read
    /// from `wlr_xdg_toplevel_decoration_v1::requested_mode` at emission
    /// time — see [`crate::decoration::requested_preference`] — not re-read
    /// at delivery, so a deferred event still reports what the client
    /// actually asked for.
    RequestDecorationMode(ToplevelId, Option<DecorationMode>),

    NewLayerSurface(LayerSurfaceId),
    /// Fires on **every** commit of a layer surface's underlying
    /// `wlr_surface`, not only the first — unlike `ToplevelInitialCommit`,
    /// wlr-layer-shell surfaces routinely change their anchor, margins or
    /// exclusive zone after mapping and a compositor needs to see each one.
    LayerSurfaceCommit(LayerSurfaceId),
    LayerSurfaceMapped(LayerSurfaceId),
    LayerSurfaceUnmapped(LayerSurfaceId),
    LayerSurfaceDestroyed(LayerSurfaceId),

    /// An observed scene buffer node is now displayed on a scene output.
    SceneBufferOutputEnter(NodeId, SceneOutputId),
    /// An observed scene buffer node is no longer displayed on a scene output.
    SceneBufferOutputLeave(NodeId, SceneOutputId),
    /// The set of outputs an observed scene buffer node is displayed on
    /// changed.
    ///
    /// Carries no payload beyond the node, deliberately. wlroots hands the
    /// signal a C array that is valid only for the emission, and this enum is
    /// `Copy + Eq` (this module's own reentrancy tests compare events with
    /// `assert_eq!`), so a `Vec` cannot live here. The array is snapshotted at
    /// emission time into the runtime instead, and
    /// [`Runtime::scene_buffer_active_outputs`](crate::Runtime::scene_buffer_active_outputs)
    /// reads it back at delivery — see that method's own doc for what a
    /// deferred delivery therefore reports.
    SceneBufferOutputsUpdate(NodeId),
    /// An observed scene buffer node was sampled while rendering a scene
    /// output. The bool is whether it was scanned out directly.
    SceneBufferOutputSample(NodeId, SceneOutputId, bool),
    /// An observed scene buffer node's own `frame_done` fired. The duration is
    /// the timestamp wlroots named, read at emission time.
    SceneBufferFrameDone(NodeId, SceneOutputId, std::time::Duration),

    Key {
        keysym: u32,
        modifiers_raw: u32,
        pressed: bool,
        time_msec: u32,
    },
    /// Coordinates are thousandths of a pixel in an `i64` rather than `f64`:
    /// `Event` derives `Eq` (this module's own reentrancy tests compare it
    /// with `assert_eq!`), and an `f64` field would force that derive off,
    /// since `f64` implements neither `Eq` nor `Hash`. Delivery converts
    /// back (`x_milli as f64 / 1000.0`); sub-milli-pixel precision has no
    /// consumer.
    PointerMotion {
        x_milli: i64,
        y_milli: i64,
        time_msec: u32,
    },
    PointerButton {
        x_milli: i64,
        y_milli: i64,
        button: u32,
        pressed: bool,
        time_msec: u32,
    },

    /// The session's lock state changed. Carries the new state — `true` when a
    /// locker takes a lock, `false` only on a genuine unlock. Never emitted
    /// with `false` on the "locker died without unlocking" path: the session
    /// stays locked there, so no state change is announced. The bool is read
    /// at emission time (the crate has already applied the state change
    /// synchronously by then), so a deferred delivery still reports the
    /// transition that actually happened.
    SessionLockChanged(bool),
}

thread_local! {
    /// Set while a handler is running anywhere on this thread.
    ///
    /// Distinct from [`Dispatcher::in_dispatch`], which is per-dispatcher and
    /// decides *deferral*. This one is thread-scoped and decides *refusal*: it
    /// is what [`crate::EventLoop::dispatch`] consults, and that entry point
    /// has no dispatcher to ask. See [`HandlerGuard`].
    static IN_HANDLER: Cell<bool> = const { Cell::new(false) };
}

/// Whether a handler is currently running on this thread.
///
/// The one fact that makes a borrowed handle's validity window enforceable.
/// A handler is handed `&Output<'h>`, whose `'h` stops it escaping the call —
/// but not from driving the event loop *within* the call, which lets wlroots
/// free the very output the handler is still holding. Nothing about that needs
/// `unsafe`: [`crate::EventLoop::dispatch`] is public, safe, takes `&self`, and
/// an `EventLoop` is re-derivable from any `&Display` a handler's own state
/// happens to hold. So the handle's lifetime cannot be the whole argument, and
/// this flag supplies the rest: while it is set, the event loop refuses to be
/// driven, so no wlroots code that could free a live handle's object runs.
///
/// The other half of the argument is that the flag is thread-local: it only
/// closes this hole because `Display`, `EventLoop`, `Backend`, and `Output`
/// are all `!Send`/`!Sync`, so a handler cannot move one to another thread
/// and find the flag clear there. That currently holds incidentally, from
/// `NonNull` and `PhantomData<&Display>` fields, not from any explicit
/// assertion on those types besides the compile-time check pinning it
/// elsewhere in the crate.
pub(crate) fn in_handler() -> bool {
    IN_HANDLER.get()
}

thread_local! {
    /// Set while a handler *delivery* is running on this thread.
    ///
    /// Narrower than [`IN_HANDLER`], which any borrow guard also raises. Only
    /// [`HandlerGuard`] sets this one, so it answers "could a handler be
    /// holding a [`BorrowedFd`](std::os::fd::BorrowedFd) this crate handed
    /// it", which is a different question from "could a handle be live".
    ///
    /// [`crate::Runtime::remove_fd`] needs the narrow one. It defers the
    /// `OwnedFd`'s close when a handler might still be reading the
    /// descriptor, and the deferred queue is drained per-turn by a run. Asking
    /// `in_handler()` made a scene borrow taken with no run on the stack — the
    /// ordinary way to inspect the scene by id — look like a delivery, so
    /// `rt.with_node(id, |_| rt.remove_fd(src))` queued the fd against a drain
    /// that was never going to happen and leaked it for the process's life.
    /// A borrow guard cannot be holding an fd borrow: the fd borrow only
    /// exists for the duration of a `fd_ready` call, and that is exactly what
    /// raises this flag.
    static IN_DELIVERY: Cell<bool> = const { Cell::new(false) };
}

/// Whether a handler delivery — not merely a borrow guard — is on this
/// thread's stack. See [`IN_DELIVERY`].
pub(crate) fn in_delivery() -> bool {
    IN_DELIVERY.get()
}

thread_local! {
    /// The `wl_display` pointer (as `usize`) [`crate::Backend::run_all`] is
    /// currently driving, or `0` when no `run_all` call is on this thread's
    /// stack.
    ///
    /// Set for the duration of `run_all` only — [`crate::Backend::run`]
    /// (the `OutputHandler`-only entry point) has no `Display` to record,
    /// so it leaves this at whatever an outer `run_all` left it at (`0` if
    /// none). This is what lets [`crate::Runtime`]'s graphics-touching
    /// entry points assert they are being driven by the same `Display`
    /// [`crate::Runtime::init_graphics`] was pinned to: read this once,
    /// compare against the pinned value, and only assert when this reads
    /// nonzero — a `0` means "no live `run_all` to compare against", not
    /// "the display matches", so it is skipped rather than treated as
    /// agreement.
    static CURRENT_DISPLAY: Cell<usize> = const { Cell::new(0) };
}

/// The display [`CURRENT_DISPLAY`] currently names, or `None` if no
/// `run_all` call is on this thread's stack right now.
pub(crate) fn current_display() -> Option<usize> {
    let raw = CURRENT_DISPLAY.get();
    if raw == 0 { None } else { Some(raw) }
}

/// Records `display` in [`CURRENT_DISPLAY`] for the life of the guard,
/// restoring whatever was there before on drop — the same "restore, don't
/// clear" shape [`HandlerGuard`] uses, and for the same reason: a nested
/// `run_all` (there is no such call today, but this costs nothing to keep
/// correct) must not let an inner call's drop erase an outer call's still-
/// live pin.
pub(crate) struct DisplayPinGuard {
    previous: usize,
}

impl DisplayPinGuard {
    /// `display` is `None` for [`crate::Backend::run`], which has no
    /// `Display` to pin — the guard still restores whatever was already
    /// recorded on drop, it just does not overwrite it with anything new.
    pub(crate) fn enter(display: Option<usize>) -> Self {
        let previous = CURRENT_DISPLAY.get();
        if let Some(display) = display {
            CURRENT_DISPLAY.set(display);
        }
        DisplayPinGuard { previous }
    }
}

impl Drop for DisplayPinGuard {
    fn drop(&mut self) {
        CURRENT_DISPLAY.set(self.previous);
    }
}

/// Marks a handler as running, on both the per-dispatcher and the thread-wide
/// flag, and clears both on every exit path.
///
/// A guard rather than paired assignments so that an unwind out of `deliver`
/// cannot leave either flag stuck — a stuck [`IN_HANDLER`] would wedge the
/// event loop shut for the rest of the thread's life. This is the same shape,
/// and for the same reason, as `Backend`'s `ReentryGuard`; the two are
/// deliberately separate because they guard different windows. `ReentryGuard`
/// spans a whole `Backend::run` call and refuses a second `run`; this spans one
/// handler delivery and refuses the loop being driven from inside it. Neither
/// can subsume the other: `run` legitimately dispatches the loop (outside any
/// handler), and a handler legitimately runs with no `run` on the stack at all
/// once a future entry point delivers events some other way.
struct HandlerGuard<'a> {
    in_dispatch: &'a Cell<bool>,

    /// As `previous`, for [`IN_DELIVERY`] — the narrower flag that says a
    /// delivery specifically, rather than any borrow guard, is on the stack.
    previous_delivery: bool,

    /// What [`IN_HANDLER`] read on the way in, restored rather than cleared on
    /// the way out. Today it is always `false` — a second dispatcher on one
    /// thread is what `Backend::run`'s `ReentryGuard` refuses — but restoring
    /// costs nothing and means a nested guard cannot reopen the loop while an
    /// outer handler is still running, which clearing unconditionally would.
    previous: bool,
}

impl<'a> HandlerGuard<'a> {
    fn enter(in_dispatch: &'a Cell<bool>) -> Self {
        in_dispatch.set(true);
        let previous = IN_HANDLER.replace(true);
        let previous_delivery = IN_DELIVERY.replace(true);
        HandlerGuard {
            in_dispatch,
            previous,
            previous_delivery,
        }
    }
}

impl Drop for HandlerGuard<'_> {
    fn drop(&mut self) {
        IN_HANDLER.set(self.previous);
        IN_DELIVERY.set(self.previous_delivery);
        self.in_dispatch.set(false);
    }
}

/// Marks an `extern "C"` frame that runs consumer code but is not a
/// [`Dispatcher`] delivery.
///
/// [`HandlerGuard`] needs a dispatcher to set its per-dispatcher flag, and some
/// callbacks have none: `render::sync`'s timeline waiter is invoked by
/// libwayland from inside `wl_event_loop_dispatch`, with a plain closure and no
/// `&mut S` anywhere. Deferral is therefore not the question — there is no
/// second `&mut S` to alias — but *refusal* still is, because the closure can
/// hold a `&Display` and drive the loop from underneath the dispatch that is
/// running it.
///
/// So this sets only [`IN_HANDLER`], and restores rather than clears it for the
/// same reason `HandlerGuard` does: a frame nested inside a real handler
/// delivery must not reopen the loop when it returns.
pub(crate) struct ForeignFrame {
    previous: bool,
}

impl ForeignFrame {
    pub(crate) fn enter() -> ForeignFrame {
        ForeignFrame {
            previous: IN_HANDLER.replace(true),
        }
    }
}

impl Drop for ForeignFrame {
    fn drop(&mut self) {
        IN_HANDLER.set(self.previous);
    }
}

/// Routes events to handler traits, one at a time.
pub(crate) struct Dispatcher<S> {
    state: *mut S,
    in_dispatch: Cell<bool>,
    deferred: RefCell<VecDeque<Event>>,
}

impl<S> Dispatcher<S> {
    pub(crate) fn new(state: *mut S) -> Self {
        Self {
            state,
            in_dispatch: Cell::new(false),
            deferred: RefCell::new(VecDeque::new()),
        }
    }

    /// Run `f` as a handler frame: events it causes are deferred rather than
    /// delivered, and the loop cannot be driven from inside it.
    ///
    /// `run_inner`'s between-turn `should_stop` needs this. It derefs
    /// [`state_ptr`](Dispatcher::state_ptr) into a `&mut S` and holds it for
    /// the whole call, and it was made with every flag down — `in_dispatch`,
    /// `IN_HANDLER`, `IN_DELIVERY` and `node_borrows` all clear. So a
    /// `should_stop` that called one plain safe mutator on an observed scene
    /// buffer had its event delivered *synchronously*, re-entering the
    /// handlers with a second `&mut S` aliasing the live one — the exact thing
    /// [`emit`](Dispatcher::emit)'s own `# Safety` forbids — and a handler
    /// reached that way saw `node_borrows == 0` and could destroy a node
    /// mid-commit, which wlroots asserts on.
    ///
    /// It is a handler in every way that matters; it was simply never marked
    /// as one, because it is spelled as a query.
    pub(crate) fn in_handler_frame<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = HandlerGuard::enter(&self.in_dispatch);
        f()
    }

    /// The state pointer, for `run_inner`'s between-turn `should_stop` check.
    ///
    /// `pub(crate)` and returning a raw pointer rather than a reference: the
    /// caller has to state, at its own call site, that no handler is running
    /// — which is the only thing that makes a `&mut S` derived from this
    /// sound — and a safe accessor returning `&mut S` would make that
    /// obligation invisible.
    pub(crate) fn state_ptr(&self) -> *mut S {
        self.state
    }

    /// Deliver `ev`, or queue it if a handler is already running.
    ///
    /// `ctx` is whatever `deliver` needs besides the state — for this crate,
    /// the table that turns an [`OutputId`] back into a live object. It is
    /// threaded through rather than captured because `deliver` is a plain `fn`
    /// pointer: the dispatcher stores no closure, so there is nothing for a
    /// closure to borrow from and no lifetime to smuggle past `emit`'s
    /// reentrancy guard.
    ///
    /// # Safety
    ///
    /// The `*mut S` this dispatcher was built with must still be valid and must
    /// not be aliased by any live reference. In particular, `S` must not own
    /// this `Dispatcher` by value (e.g. `struct Compositor { dispatcher:
    /// Dispatcher<Self>, .. }`): this call holds `&self` across
    /// `deliver(&mut *self.state, ..)`, and if the dispatcher lives inside
    /// `S` itself, that `&mut S` reborrows the very `&self` this call is
    /// still holding, which is aliasing UB regardless of the reentrancy
    /// guard. The dispatcher must be reachable from `S` only through a raw
    /// pointer, and any reentrant `emit` call (e.g. from inside `deliver`)
    /// must be made through that raw pointer, not through a reference derived
    /// from `S`.
    ///
    /// `ctx` is borrowed only for the call, so it may overlap this dispatcher —
    /// both borrows are shared.
    pub(crate) unsafe fn emit<C>(&self, ctx: &C, ev: Event, deliver: fn(&C, &mut S, Event)) {
        if self.in_dispatch.get() {
            self.deferred.borrow_mut().push_back(ev);
            return;
        }

        // Sets both flags and clears them on every exit path, unwind included.
        let _handler = HandlerGuard::enter(&self.in_dispatch);

        // SAFETY: the flag above guarantees no other `&mut S` is live for the
        // duration of this call (any reentrant `emit` sees the flag set and
        // queues instead of calling `deliver`), and the caller guarantees the
        // pointer is valid. `in_dispatch` only excludes a reentrant call on
        // *this* `Dispatcher`; it is a per-dispatcher `Cell` and a second
        // `Dispatcher<S>` built over the same `*mut S` would not consult it.
        // What excludes that case is this call's own `# Safety` clause above
        // (no live aliasing `&mut S`), which `Backend::run`'s `ReentryGuard`
        // discharges by refusing a second concurrent `run`.
        unsafe { deliver(ctx, &mut *self.state, ev) };

        // Drain whatever the handler queued. `pop_front` borrows only for the
        // statement, so a handler may queue more while we deliver.
        loop {
            let next = self.deferred.borrow_mut().pop_front();
            match next {
                // SAFETY: as above — still inside the guarded region, so no
                // other `&mut S` can be live.
                Some(ev) => unsafe { deliver(ctx, &mut *self.state, ev) },
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// The premise [`IN_HANDLER`] rests on, asserted rather than assumed.
    ///
    /// The guard is thread-scoped: [`crate::EventLoop::dispatch`] refuses while
    /// a handler is on *this* thread's stack. A handler that could move an
    /// `EventLoop` — or a `&Display` it could derive one from — to another
    /// thread would find the flag clear there, dispatch freely, and free an
    /// output this thread still holds a handle to, with no `unsafe` written.
    ///
    /// Today that cannot happen because every one of these types is `!Send`,
    /// which falls out incidentally of holding a `NonNull` or a `PhantomData`
    /// of one. Nothing else states it. A future field — an `Arc`, a plain
    /// `usize` handle — or a well-meant `unsafe impl Send` would void the
    /// entire guard in silence, and no other test would notice.
    mod thread_scoped {
        use static_assertions::assert_not_impl_any;

        assert_not_impl_any!(crate::Display: Send, Sync);
        assert_not_impl_any!(crate::EventLoop<'static>: Send, Sync);
        assert_not_impl_any!(crate::Backend<'static>: Send, Sync);
        assert_not_impl_any!(crate::Output<'static>: Send, Sync);
        // `Region` owns a C allocation and `RegionRef` borrows one, so neither
        // may cross a thread; the raw pointer inside each is what makes that
        // true today, and this is what keeps it true if the representation
        // changes.
        assert_not_impl_any!(crate::Region: Send, Sync);
        assert_not_impl_any!(crate::RegionRef<'static>: Send, Sync);
    }

    /// The other half of the same argument: the plain value types carry no
    /// wlroots ownership at all, so refusing to let them cross a thread would
    /// be a cost with no safety behind it.
    mod thread_free {
        use static_assertions::assert_impl_all;

        assert_impl_all!(crate::Box2D: Send, Sync);
        assert_impl_all!(crate::FBox: Send, Sync);
        assert_impl_all!(crate::Transform: Send, Sync);
        assert_impl_all!(crate::LogLevel: Send, Sync);
    }

    /// Records delivery order, and re-enters the dispatcher from inside a
    /// handler — exactly what wlroots does when a handler destroys an object.
    ///
    /// `reenter_with` is a `Cell`, not a `RefCell`: `Event` is `Copy` and
    /// `Option<Event>` is `Default`, so `Cell::take` is a drop-in replacement
    /// that never holds a borrow guard live across the reentrant `emit` call
    /// below — a `RefCell`'s `RefMut` from `borrow_mut().take()` remains live
    /// through the `if let` body in edition 2024 (rescoping only moved it
    /// ahead of the `else` block), so the reentrant call would double-borrow.
    struct Recorder {
        seen: Vec<Event>,
        reenter_with: Cell<Option<Event>>,
        dispatcher: *const Dispatcher<Recorder>,
    }

    fn deliver(_ctx: &(), state: &mut Recorder, ev: Event) {
        state.seen.push(ev);
        if let Some(inner) = state.reenter_with.take() {
            // SAFETY: the dispatcher outlives the test body.
            unsafe { (*state.dispatcher).emit(&(), inner, deliver) };
        }
    }

    #[test]
    fn reentrant_events_are_deferred_until_the_outer_handler_returns() {
        let mut state = Recorder {
            seen: Vec::new(),
            reenter_with: Cell::new(Some(Event::OutputDestroyed(OutputId(2)))),
            dispatcher: std::ptr::null(),
        };
        // A raw pointer taken once and used throughout: writing through
        // `state.dispatcher = ..` directly (after already deriving `p`)
        // would invalidate `p` under Stacked/Tree Borrows, which is exactly
        // the aliasing hazard `emit`'s contract forbids. Going through `p`
        // for the write keeps one consistent provenance for the whole test.
        let p = &raw mut state;
        let d = Dispatcher::new(p);
        // SAFETY: `p` is a live, exclusively-derived pointer to `state`, and
        // no reference to `state` exists at this point.
        unsafe { (*p).dispatcher = &raw const d };

        // SAFETY: `state` outlives `d` for the duration of this call, and is
        // reached only through `p`'s provenance from here on.
        unsafe { d.emit(&(), Event::OutputFrame(OutputId(1)), deliver) };

        // `p`'s last use was inside `emit` above, so reading through the
        // original `state` binding here does not conflict with it.
        assert_eq!(
            state.seen,
            vec![
                Event::OutputFrame(OutputId(1)),
                Event::OutputDestroyed(OutputId(2))
            ],
            "the inner event must arrive after the outer handler returns, not during it"
        );
    }

    #[test]
    fn non_reentrant_events_dispatch_immediately() {
        let mut state = Recorder {
            seen: Vec::new(),
            reenter_with: Cell::new(None),
            dispatcher: std::ptr::null(),
        };
        let p = &raw mut state;
        let d = Dispatcher::new(p);
        // SAFETY: as in the test above.
        unsafe { (*p).dispatcher = &raw const d };

        // SAFETY: as above.
        unsafe {
            d.emit(&(), Event::NewOutput(OutputId(7)), deliver);
            d.emit(&(), Event::OutputFrame(OutputId(7)), deliver);
        }

        assert_eq!(
            state.seen,
            vec![
                Event::NewOutput(OutputId(7)),
                Event::OutputFrame(OutputId(7))
            ]
        );
        assert!(
            !in_handler(),
            "the handler flag must be clear once dispatch unwinds, or the \
             event loop would stay shut for the rest of this thread's life"
        );
        // One guard sets and clears both flags together, so this also witnesses
        // that the per-dispatcher `in_dispatch` flag was cleared: the only code
        // that can restore `IN_HANDLER` is `HandlerGuard::drop`, which clears
        // `in_dispatch` in the same breath.
    }

    /// A handler-entry/exit marker. Unlike a bare `Event` log, this can tell
    /// deferred delivery apart from naive recursion: both produce the same
    /// final *event* order at one level of reentrancy, but only naive
    /// recursion nests an `Enter` inside another handler's `Enter`/`Exit`
    /// pair.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Trace {
        Enter(Event),
        Exit(Event),
    }

    struct TracingRecorder {
        trace: Vec<Trace>,
        reenter_with: Cell<Option<Event>>,
        dispatcher: *const Dispatcher<TracingRecorder>,
    }

    fn tracing_deliver(_ctx: &(), state: &mut TracingRecorder, ev: Event) {
        state.trace.push(Trace::Enter(ev));
        if let Some(inner) = state.reenter_with.take() {
            // SAFETY: the dispatcher outlives the test body.
            unsafe { (*state.dispatcher).emit(&(), inner, tracing_deliver) };
        }
        state.trace.push(Trace::Exit(ev));
    }

    /// The load-bearing soundness test: proves the inner handler does not
    /// start running until the outer handler has *fully exited*, not merely
    /// that both events eventually appear in the right order. A dispatcher
    /// that recursed into `deliver` directly (instead of deferring) would
    /// produce `Enter(outer), Enter(inner), Exit(inner), Exit(outer)` here —
    /// a nested pair, still with the events in the same final order as the
    /// correct trace, which is exactly what the plain event-order test above
    /// cannot distinguish.
    #[test]
    fn reentrant_handler_fully_exits_before_the_deferred_handler_enters() {
        let mut state = TracingRecorder {
            trace: Vec::new(),
            reenter_with: Cell::new(Some(Event::OutputDestroyed(OutputId(2)))),
            dispatcher: std::ptr::null(),
        };
        let p = &raw mut state;
        let d = Dispatcher::new(p);
        // SAFETY: as in the tests above.
        unsafe { (*p).dispatcher = &raw const d };

        // SAFETY: `state` outlives `d` for the duration of this call, and is
        // reached only through `p`'s provenance from here on.
        unsafe { d.emit(&(), Event::OutputFrame(OutputId(1)), tracing_deliver) };

        assert_eq!(
            state.trace,
            vec![
                Trace::Enter(Event::OutputFrame(OutputId(1))),
                Trace::Exit(Event::OutputFrame(OutputId(1))),
                Trace::Enter(Event::OutputDestroyed(OutputId(2))),
                Trace::Exit(Event::OutputDestroyed(OutputId(2))),
            ],
            "the inner handler must not start until the outer handler has fully exited"
        );
    }

    /// The thread-scoped half of the guard, which `EventLoop::dispatch`
    /// consults to refuse being driven from inside a handler.
    ///
    /// Asserted from inside the delivery rather than around it, because "set
    /// while a handler runs" is the whole claim; a test that only checked it
    /// was clear afterwards would pass against a flag that is never set.
    #[test]
    fn the_handler_flag_is_set_for_the_duration_of_a_delivery() {
        fn record(_ctx: &(), state: &mut Vec<bool>, _ev: Event) {
            state.push(in_handler());
        }

        let mut seen: Vec<bool> = Vec::new();
        assert!(!in_handler(), "no handler is running before dispatch");

        let p = &raw mut seen;
        let d = Dispatcher::new(p);
        // SAFETY: `seen` outlives `d` and is reached only through `p` from
        // here on; nothing else holds a reference to it across the call.
        unsafe { d.emit(&(), Event::OutputFrame(OutputId(1)), record) };

        assert_eq!(
            seen,
            vec![true],
            "the flag must read set from inside the handler, which is the only \
             place it can do any good"
        );
        assert!(!in_handler(), "and be restored once the delivery returns");
    }
}
