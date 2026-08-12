//! Handler traits.
//!
//! Consumers implement these on one state struct, which the dispatcher hands to
//! every handler as `&mut S`. Every method is defaulted, so a consumer
//! implements only what they use.

use crate::{Edges, KeyEvent, Output, OutputId, Toplevel, ToplevelId};

/// Output lifecycle and frame events.
///
/// # Panics
///
/// Every method here is called from C, underneath an `extern "C"` frame, so a
/// panic escaping one **aborts the process**. That has been defined behaviour
/// rather than undefined since Rust 1.81, but it is still an abort out of a
/// compositor's event loop.
///
/// It is also the intended outcome, so do not read it as a defect to be papered
/// over: unwinding back through wlroots' C frames is not possible, and catching
/// the panic and returning into wlroots would resume a compositor whose state
/// is half-updated and whose invariants the handler just abandoned. Aborting is
/// the honest end.
///
/// The consequence for an implementor is that a handler is not a place for
/// `assert!`, `unwrap`, or indexing that might be out of range. Record the
/// problem in your own state and check it once control is back in your hands.
pub trait OutputHandler {
    /// A new output was attached. The handle is valid only for this call;
    /// remember [`Output::id`] if you need to refer to it later.
    ///
    /// An announcement may be **dropped** rather than delivered. If wlroots
    /// destroys the output between announcing it and this call — which it can,
    /// because an announcement arriving while another handler is running is
    /// queued behind it — there is no longer an object to hand you, and the
    /// event is discarded. See [`destroyed`](OutputHandler::destroyed) for the
    /// consequence.
    fn new_output(&mut self, output: &Output<'_>) {
        let _ = output;
    }

    /// It is a good time to render a frame for this output.
    ///
    /// # Timeliness
    ///
    /// This is delivered like every other event, which means it **may be
    /// deferred**. wlroots emits signals synchronously from inside its own API
    /// calls, so a frame arriving while another handler is already running is
    /// queued and delivered once that handler returns — after wlroots' own
    /// emission has returned, and therefore outside the window wlroots intended
    /// the rendering to happen in.
    ///
    /// That is a genuine cost of this crate's dispatch model and not a bug to
    /// be reported, because the alternative is unsound rather than merely
    /// awkward: delivering directly from inside a running handler would hand
    /// out a second `&mut Self` while the first is still live, which is
    /// undefined behaviour. No rendering deadline is worth that, so the
    /// deferral wins and this is the price.
    ///
    /// A slice that needs the guarantee will have to change the model — a
    /// render-scheduling API that does not put a `&mut Self` on the stack in
    /// the first place, say — rather than special-case this method.
    fn frame(&mut self, output: &Output<'_>) {
        let _ = output;
    }

    /// The output is gone. Only the id is passed, because there is no longer an
    /// object to borrow.
    ///
    /// **`id` may be one you were never told about.** This is delivered
    /// unconditionally *within the `run` that announced the output*, whereas
    /// [`new_output`](OutputHandler::new_output) is dropped when the output
    /// dies before it can be delivered — so an output created and destroyed
    /// while another handler was running produces a `destroyed` with no
    /// preceding `new_output`. That falls out of ids outliving objects on
    /// purpose, and it is not going to change.
    ///
    /// The "within the `run`" qualifier is not decorative: an output
    /// announced by one call to `Backend::run` gets no further events at all
    /// — `destroyed` included — once that call returns. Its registration
    /// lives with that call and is torn down with it, and nothing re-creates
    /// it on a later `run`, so an output that dies after its announcing
    /// `run` has returned never produces a `destroyed` here.
    ///
    /// Write this method so an unknown id is harmless. `remove` on a map that
    /// has no such key is fine; indexing it is not, and a panic here **aborts
    /// the process** rather than failing anything (see the trait's own docs).
    fn destroyed(&mut self, id: OutputId) {
        let _ = id;
    }
}

/// File-descriptor source readiness.
///
/// # Panics
///
/// As for [`OutputHandler`]: this runs underneath an `extern "C"` frame, so a
/// panic escaping it aborts the process.
pub trait FdHandler {
    /// `source` is ready.
    ///
    /// `fd` is the descriptor that was registered, borrowed for this call
    /// only — the [`Runtime`](crate::Runtime) owns it and closes it when it
    /// drops, so nothing here may close it or take ownership of it (do not
    /// build a `File` from it; use a borrowing reader).
    ///
    /// Draining the fd is the implementor's job. libwayland's event loop is
    /// level-triggered, so an implementor that reads nothing is called again
    /// on the next turn, forever.
    fn fd_ready(
        &mut self,
        source: crate::SourceId,
        fd: std::os::fd::BorrowedFd<'_>,
        readiness: crate::Readiness,
    ) {
        let _ = (source, fd, readiness);
    }
}

/// Control over how long [`Backend::run_all`](crate::Backend::run_all) keeps
/// dispatching.
///
/// Unlike every other trait here, this one is **not** called from C: `run_all`
/// calls it between dispatch turns, with no handler on the stack. A panic here
/// unwinds normally out of `run_all`. It is still the wrong place for one.
pub trait LoopHandler {
    /// Called once per dispatch turn, after that turn's events have been
    /// delivered. Return `true` to end the run.
    ///
    /// Consulted under both [`Until::Turns`](crate::Until::Turns) and
    /// [`Until::Stop`](crate::Until::Stop), so an early exit is always
    /// available.
    fn should_stop(&mut self) -> bool {
        false
    }
}

/// xdg-shell toplevel lifecycle.
///
/// Declared with no methods in 0.20.1 so that [`Handlers`]' supertrait list
/// could freeze from the first release; these methods were added in 0.20.2,
/// which is additive because they are all defaulted. An
/// `impl ToplevelHandler for MyState {}` written against 0.20.1 still
/// compiles unchanged.
///
/// `request_maximize`, `request_fullscreen`, `request_move` and
/// `request_resize` were added in 0.20.7, additively for the same reason:
/// every one of them is defaulted, so an impl written against any earlier
/// 0.20.x still compiles.
///
/// # Panics
///
/// As for [`OutputHandler`]: every method runs underneath an `extern "C"`
/// frame, so a panic escaping one aborts the process.
pub trait ToplevelHandler {
    /// A client created a toplevel. It is **not** mapped yet and has no
    /// buffer: nothing about its size or content is known, and the client is
    /// waiting for a configure.
    ///
    /// Record [`Toplevel::id`] here; the handle is valid only for this call.
    fn new_toplevel(&mut self, toplevel: &Toplevel<'_>) {
        let _ = toplevel;
    }

    /// The client committed for the first time, which is where xdg-shell
    /// requires the compositor to answer with a configure.
    ///
    /// Stage whatever you want in that configure —
    /// [`Runtime::set_toplevel_size`](crate::Runtime::set_toplevel_size) and
    /// its siblings — and this crate schedules it right after this event is
    /// delivered. "Schedules", not "sends": the configure goes out from an
    /// idle source, not synchronously from inside this call, and — under
    /// deferral, if this event was queued behind a handler already running —
    /// the schedule can be requested before this method actually runs,
    /// rather than strictly after it returns. Either way the client ends up
    /// configured, which is the guarantee that matters: if you stage nothing,
    /// it is configured with its own preferred size, because a client that is
    /// never configured never maps and the symptom is an invisible window
    /// rather than an error. What is not guaranteed under deferral is that
    /// the *first* configure a client sees necessarily carries whatever this
    /// method goes on to stage.
    fn initial_commit(&mut self, toplevel: &Toplevel<'_>) {
        let _ = toplevel;
    }

    /// The toplevel has a buffer and should be displayed.
    ///
    /// The crate has already inserted it into the scene graph by this point,
    /// so positioning it here takes effect on the first frame it appears in.
    fn mapped(&mut self, toplevel: &Toplevel<'_>) {
        let _ = toplevel;
    }

    /// The toplevel should not be displayed any more — a null buffer, or the
    /// role object going away. Only the id, because the handle may already be
    /// unusable.
    ///
    /// **Not** the same as destruction: a toplevel can be unmapped and mapped
    /// again, keeping its id.
    fn unmapped(&mut self, id: ToplevelId) {
        let _ = id;
    }

    /// The client sent `set_title`.
    fn title_changed(&mut self, toplevel: &Toplevel<'_>) {
        let _ = toplevel;
    }

    /// The toplevel is gone. Only the id is passed, because there is no
    /// longer an object to borrow.
    ///
    /// **`id` may be one you were never told about**, for the same reason
    /// [`OutputHandler::destroyed`] documents: an announcement that arrived
    /// while another handler was running is queued, and a toplevel created
    /// and destroyed inside that window produces a destroy with no preceding
    /// `new_toplevel`. Write this so an unknown id is harmless — `remove` on
    /// a map, never indexing.
    fn toplevel_destroyed(&mut self, id: ToplevelId) {
        let _ = id;
    }

    /// The client asked to (un)maximize (`xdg_toplevel.set_maximized` /
    /// `unset_maximized`). `maximize` is the state the client asked for —
    /// the requested *target*, not a toggle: `true` for `set_maximized`,
    /// `false` for `unset_maximized`, regardless of the current state.
    ///
    /// The whole handle is passed, not just the id, because deciding whether
    /// to grant a state request is policy, and policy usually wants
    /// [`Toplevel::title`](crate::Toplevel::title) /
    /// [`app_id`](crate::Toplevel::app_id) — unlike
    /// [`request_move`](ToplevelHandler::request_move) /
    /// [`request_resize`](ToplevelHandler::request_resize), which get a bare
    /// id because the handle offers nothing move-specific.
    ///
    /// xdg-shell requires the compositor to answer *every* such request
    /// with a configure, whether or not it grants it — ignoring the request
    /// is legal, ignoring the configure is not. This default does nothing,
    /// and it is still protocol-correct: the guarantee lives in the
    /// **dispatch layer**, not here. After this method returns, dispatch
    /// unconditionally schedules a bare configure on this toplevel
    /// ([`Runtime::configure_toplevel`](crate::Runtime::configure_toplevel))
    /// — a default trait method has no `Runtime` to send one with itself.
    /// wlroots coalesces a second scheduled configure into whatever this
    /// call already staged (`Runtime::set_toplevel_*`), so overriding this
    /// method to honour the request costs nothing extra: the dispatch
    /// layer's schedule after it returns is harmless, not a duplicate send.
    fn request_maximize(&mut self, toplevel: &Toplevel<'_>, maximize: bool) {
        let _ = (toplevel, maximize);
    }

    /// The client asked to (un)fullscreen (`xdg_toplevel.set_fullscreen` /
    /// `unset_fullscreen`). `fullscreen` is the state the client asked for,
    /// on the same terms as `maximize` above: a target, not a toggle.
    ///
    /// Same contract as
    /// [`request_maximize`](ToplevelHandler::request_maximize) throughout —
    /// the dispatch layer, not this default, guarantees the answering
    /// configure, and the handle is passed for the same policy reason.
    fn request_fullscreen(&mut self, toplevel: &Toplevel<'_>, fullscreen: bool) {
        let _ = (toplevel, fullscreen);
    }

    /// Interactive move request (`xdg_toplevel.move`).
    ///
    /// Unlike the two methods above, ignoring this is legal on its own
    /// terms — an interactive move that never starts is not a protocol
    /// violation — so there is no configure guarantee to keep and no
    /// dispatch-layer follow-up after it.
    ///
    /// Only the id, not a [`Toplevel`] handle: starting an interactive move
    /// needs the pointer's position and the compositor's own window
    /// geometry, none of which the handle carries, so borrowing it would buy
    /// nothing.
    ///
    /// The seat and serial the wire event carries are deliberately not
    /// passed through: this crate does not forward them, so a compositor
    /// wanting interactive move/resize enforces its own pointer-pressed
    /// policy rather than trusting the client's claim of an active grab.
    fn request_move(&mut self, id: ToplevelId) {
        let _ = id;
    }

    /// Interactive resize request (`xdg_toplevel.resize`). `edges` is which
    /// edge(s) the client reported dragging. Same no-guarantee contract as
    /// [`request_move`](ToplevelHandler::request_move), and the same reason
    /// seat/serial are absent.
    fn request_resize(&mut self, id: ToplevelId, edges: Edges) {
        let _ = (id, edges);
    }

    /// The client (un)stated a decoration-mode preference for this toplevel,
    /// via `zxdg_decoration_manager_v1`/`zxdg_toplevel_decoration_v1`.
    ///
    /// `client_side_preferred`: `Some(true)` means the client asked for
    /// client-side decorations, `Some(false)` means it asked for
    /// server-side, and `None` means it stated no preference at all
    /// (`requested_mode` reads `WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_NONE`).
    ///
    /// Answer with
    /// [`Runtime::set_decoration_mode`](crate::Runtime::set_decoration_mode)
    /// from inside this call. wlroots requires a mode be set before the
    /// toplevel's initial commit is answered, so — same coalescing pattern
    /// as [`request_maximize`](ToplevelHandler::request_maximize)'s bare
    /// configure — the dispatch layer sends the server-side default after
    /// this method returns, but *only* if this method sent nothing itself:
    /// unlike the configure case, sending twice here is not harmless (it
    /// would tell the client server-side and then, in the same turn,
    /// whatever this method asked for), so the default is conditional
    /// rather than unconditional.
    fn request_decoration_mode(&mut self, id: ToplevelId, client_side_preferred: Option<bool>) {
        let _ = (id, client_side_preferred);
    }
}

/// Seat, keyboard and pointer input.
///
/// Declared with no methods in 0.20.1 so that [`Handlers`]' supertrait list
/// could freeze; these were added in 0.20.4, additively. An
/// `impl SeatHandler for MyState {}` written against 0.20.1 still compiles
/// unchanged.
///
/// # Panics
///
/// As for [`OutputHandler`]: every method here runs underneath an
/// `extern "C"` frame, so a panic escaping one **aborts the process**.
pub trait SeatHandler {
    /// A key was pressed or released.
    ///
    /// Return `true` if the compositor consumed it — a bound action fired —
    /// and it will **not** be forwarded to the focused client. Return `false`
    /// and the library forwards it, which is what makes typing work.
    ///
    /// Called for releases as well as presses; a compositor that only binds
    /// presses must check [`KeyEvent::pressed`] and return `false` otherwise,
    /// or the matching release never reaches the client and it will believe
    /// the key is still held.
    ///
    /// # A deferred key is forwarded whatever you return
    ///
    /// Events are queued behind a handler that is already running (see
    /// [`OutputHandler::frame`]'s own note on deferral, which is the same
    /// mechanism), and a key that is queued has already had its forwarding
    /// decided by the time this runs: the library forwards it, because the
    /// compositor's answer is not known yet and dropping a keystroke is
    /// worse than sending one. So a binding that happens to fire from a
    /// deferred key still fires — this method is always called — but the
    /// client sees the key as well.
    ///
    /// It is rare, since it needs a key to arrive during a handler, and it
    /// is not fixable without holding wlroots' emission open across a
    /// second `&mut Self`. Do not write a binding whose correctness depends
    /// on the client never seeing the key; treat consumption as an
    /// optimisation, not a guarantee.
    fn key(&mut self, event: &KeyEvent<'_>) -> bool {
        let _ = event;
        false
    }

    /// The pointer moved to `(x, y)` in scene coordinates.
    ///
    /// The library has already moved the cursor and updated the client-side
    /// pointer focus before this is called, so an implementor is free to do
    /// nothing at all; this exists for compositors that track the pointer
    /// themselves (for a drag, or a snap preview).
    fn pointer_motion(&mut self, x: f64, y: f64, time_msec: u32) {
        let _ = (x, y, time_msec);
    }

    /// A pointer button changed state at `(x, y)` in scene coordinates.
    ///
    /// `button` is a Linux input event code — `BTN_LEFT` is `0x110`.
    ///
    /// The library forwards the button to the focused client after this
    /// returns, unconditionally: unlike keys there is no interception,
    /// because a compositor that wants to swallow a click does it by not
    /// having a client under the pointer, not by filtering.
    fn pointer_button(&mut self, x: f64, y: f64, button: u32, pressed: bool, time_msec: u32) {
        let _ = (x, y, button, pressed, time_msec);
    }
}

/// Every handler trait at once.
///
/// The bound on [`Backend::run_all`](crate::Backend::run_all), and
/// blanket-implemented, so a consumer never writes `impl Handlers` — they
/// implement whichever of the five traits they care about (all methods are
/// defaulted, so an empty `impl` is enough for the rest) and this follows.
///
/// The blanket impl also means this trait cannot be implemented manually: any
/// hand-written impl would overlap it. That is deliberate — it is what keeps
/// the supertrait list, rather than each consumer's idea of it, the contract.
pub trait Handlers:
    OutputHandler + ToplevelHandler + SeatHandler + FdHandler + LoopHandler
{
}

impl<T> Handlers for T where
    T: OutputHandler + ToplevelHandler + SeatHandler + FdHandler + LoopHandler
{
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A consumer implementing nothing at all must still satisfy the trait.
    struct Minimal;
    impl OutputHandler for Minimal {}

    #[test]
    fn every_handler_method_is_defaulted() {
        fn accepts<S: OutputHandler>(_: &S) {}
        accepts(&Minimal);
    }

    /// A consumer implementing nothing at all must satisfy the whole set,
    /// which is what makes `run_all` usable by an output-only consumer.
    struct MinimalAll;
    impl OutputHandler for MinimalAll {}
    impl ToplevelHandler for MinimalAll {}
    impl SeatHandler for MinimalAll {}
    impl FdHandler for MinimalAll {}
    impl LoopHandler for MinimalAll {}

    #[test]
    fn the_blanket_impl_covers_a_state_that_implements_every_trait_emptily() {
        fn accepts<S: Handlers>(_: &S) {}
        accepts(&MinimalAll);
    }

    #[test]
    fn should_stop_defaults_to_never_stopping() {
        let mut m = MinimalAll;
        assert!(!m.should_stop());
    }
}
