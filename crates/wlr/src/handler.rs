//! Handler traits.
//!
//! Consumers implement these on one state struct, which the dispatcher hands to
//! every handler as `&mut S`. Every method is defaulted, so a consumer
//! implements only what they use.

#[cfg(wlr_has_xwayland)]
use crate::{Box2D, XwaylandSurface, XwaylandSurfaceId};
use crate::{
    ActivationToken, CursorShape, CursorShapeDevice, DecorationMode, Edges, KeyEvent,
    LayerSurface, LayerSurfaceId, NodeId, Output, OutputId, SceneOutputId, Toplevel, ToplevelId,
    Transform,
};

/// One output head as it stands *after* a client's output-management
/// configuration has been applied and committed.
///
/// Handed to [`OutputHandler::output_configuration_applied`] as an owned value
/// — every field is a copy, and no wlroots pointer is carried across the
/// boundary — so a handler may keep it for as long as it likes. The wlroots
/// `wlr_output_configuration_v1` the values are derived from is freed the
/// instant the apply callback returns; nothing here borrows it.
///
/// The values are what wlroots reports on the output *after* the commit, not
/// what the client requested: `width`/`height`/`refresh_mhz` come from the
/// output's resulting mode (all `0` on a head that ended up disabled),
/// `x`/`y` are the layout position the crate applied (output-management
/// `state_apply` does not place the output — the crate does that separately),
/// and `scale`/`transform`/`enabled` are read back from the committed output.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedHead {
    /// The connector name (`output.name()`), the stable key a compositor
    /// matches a persisted layout against. `None` on the rare output wlroots
    /// has not named yet.
    pub name: Option<String>,
    /// Whether the head is enabled after the commit.
    pub enabled: bool,
    /// Resulting mode width in pixels, `0` if disabled.
    pub width: i32,
    /// Resulting mode height in pixels, `0` if disabled.
    pub height: i32,
    /// Resulting refresh rate in mHz, `0` if disabled or backend-picked.
    pub refresh_mhz: i32,
    /// Layout x position the crate applied.
    pub x: i32,
    /// Layout y position the crate applied.
    pub y: i32,
    /// Resulting scale.
    pub scale: f32,
    /// Resulting transform.
    pub transform: Transform,
}

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

    /// The GPU was lost — reset, removed, or otherwise invalidated — and the
    /// renderer [`Runtime::init_graphics`](crate::Runtime::init_graphics)
    /// created can no longer be used.
    ///
    /// Every texture, swapchain and in-flight render pass derived from it is
    /// invalid too. wlroots offers no recovery call: the only correct response
    /// is to tear the graphics stack down and build a new one, which for this
    /// crate today means ending the run and starting over. Ignoring it leaves a
    /// compositor drawing into a renderer that will fail every call.
    ///
    /// `rt` is the runtime the run was given, so an implementor does not have
    /// to have kept a clone of it to react. Added in 0.20.19.
    ///
    /// On a renderer a consumer created themselves
    /// ([`Renderer::autocreate`](crate::Renderer::autocreate)) this is not
    /// delivered — ask [`Renderer::is_lost`](crate::Renderer::is_lost) instead.
    ///
    /// **Only [`Backend::run_all`](crate::Backend::run_all) delivers this.**
    /// [`Backend::run`](crate::Backend::run) requires this same trait — so an
    /// implementation written against it compiles, and reads as though it were
    /// wired up — but `run` builds its own empty
    /// [`Runtime`](crate::Runtime) that never has graphics, so there is no
    /// renderer to watch and this is never called. A consumer using `run` and
    /// treating this as their GPU-reset recovery path has no recovery path at
    /// all: on a reset the compositor keeps drawing into a renderer that fails
    /// every call, with no notification and no error. Use `run_all` if you need
    /// it, or poll [`Renderer::is_lost`](crate::Renderer::is_lost) on a
    /// renderer you own.
    ///
    /// **Call [`Runtime::init_graphics`](crate::Runtime::init_graphics) before
    /// [`Backend::run_all`](crate::Backend::run_all), not from inside a
    /// handler.** The listener behind this method is linked once, when the run
    /// registers its listeners, and only if a renderer exists by then. A run
    /// whose graphics are initialised later — from inside
    /// [`new_output`](OutputHandler::new_output), say — gets a renderer nobody
    /// is watching, and this method is never called for the rest of that run.
    /// The run still works; the notification is simply absent, which is the
    /// hard kind of absence to notice, since a lost renderer is rare and its
    /// symptom is every later draw failing rather than a missing callback.
    fn renderer_lost(&mut self, rt: &crate::Runtime) {
        let _ = rt;
    }

    /// An observed scene buffer node is now displayed on `scene_output`.
    ///
    /// Delivered only for nodes a consumer asked about with
    /// [`Runtime::observe_scene_buffer`](crate::Runtime::observe_scene_buffer):
    /// these signals fire per buffer node, and linking a listener into every
    /// node in a scene to deliver events nobody wanted would be a cost with no
    /// buyer. Added in 0.20.19.
    ///
    /// This is the signal a compositor uses to follow a window between
    /// monitors — it fires for a client's surface node as readily as for one
    /// this crate created.
    fn scene_buffer_output_enter(
        &mut self,
        rt: &crate::Runtime,
        node: NodeId,
        scene_output: SceneOutputId,
    ) {
        let _ = (rt, node, scene_output);
    }

    /// An observed scene buffer node is no longer displayed on `scene_output`.
    /// The mirror of
    /// [`scene_buffer_output_enter`](OutputHandler::scene_buffer_output_enter),
    /// with the same opt-in. Added in 0.20.19.
    fn scene_buffer_output_leave(
        &mut self,
        rt: &crate::Runtime,
        node: NodeId,
        scene_output: SceneOutputId,
    ) {
        let _ = (rt, node, scene_output);
    }

    /// The set of scene outputs an observed node is displayed on changed.
    ///
    /// The set itself is **not** carried: ask
    /// [`Runtime::scene_buffer_active_outputs`](crate::Runtime::scene_buffer_active_outputs)
    /// for it. wlroots hands this signal an array valid only for its own
    /// emission, and this crate's events carry ids and scalars so that a
    /// deferred one cannot name freed memory; the array is snapshotted when the
    /// signal fires and read back here. Added in 0.20.19.
    fn scene_buffer_outputs_update(&mut self, rt: &crate::Runtime, node: NodeId) {
        let _ = (rt, node);
    }

    /// An observed scene buffer node was sampled while `scene_output` was
    /// rendered.
    ///
    /// `direct_scanout` is true when wlroots handed the buffer straight to the
    /// display controller instead of compositing it — which is what a
    /// compositor tuning for full-screen video wants to know. Added in 0.20.19.
    fn scene_buffer_output_sample(
        &mut self,
        rt: &crate::Runtime,
        node: NodeId,
        scene_output: SceneOutputId,
        direct_scanout: bool,
    ) {
        let _ = (rt, node, scene_output, direct_scanout);
    }

    /// An observed scene buffer node may draw its next frame.
    ///
    /// `when` is the timestamp wlroots named, read when the signal fired rather
    /// than when this was delivered. For a client's surface the scene answers
    /// this itself; this is the notification for a buffer node whose pixels the
    /// compositor produces. Added in 0.20.19.
    fn scene_buffer_frame_done(
        &mut self,
        rt: &crate::Runtime,
        node: NodeId,
        scene_output: SceneOutputId,
        when: std::time::Duration,
    ) {
        let _ = (rt, node, scene_output, when);
    }

    /// A client's `zwlr_output_manager_v1` configuration was applied and
    /// committed. `heads` is the resulting per-head state as owned
    /// [`AppliedHead`] values (see that type's own doc) — the compositor's cue
    /// to re-derive its geometry from the new layout and persist it.
    ///
    /// Added additively, on the same terms as
    /// [`SeatHandler::session_lock_changed`](crate::SeatHandler::session_lock_changed):
    /// it is defaulted, so an `impl OutputHandler for MyState {}` written
    /// against any earlier 0.20.x still compiles unchanged.
    ///
    /// Only fired after a **successful** apply — a rejected configuration
    /// sends the client `failed` and this is not called. The crate has already
    /// committed each head and applied its layout position by the time this
    /// runs; the handler's job is its own bookkeeping, exactly as
    /// `session_lock_changed`'s is.
    ///
    /// **The crate deliberately does not re-broadcast the new layout to the
    /// other bound `zwlr_output_manager_v1` clients from here** — that would
    /// force a serial bump before the compositor has settled (and persisted)
    /// its geometry. Your implementation MUST call
    /// [`Runtime::update_output_manager_state`](crate::Runtime::update_output_manager_state)
    /// once it has re-derived and persisted the layout, so every other bound
    /// manager sees the fresh head state and a bumped serial. Skipping it
    /// leaves those clients with a stale view until the next output change.
    fn output_configuration_applied(&mut self, heads: Vec<AppliedHead>) {
        let _ = heads;
    }

    /// A `gamma-control-v1` client set (or wlroots otherwise changed) this
    /// output's gamma ramp.
    ///
    /// Notification only, and defaulted to a no-op. Unlike
    /// [`SeatHandler::request_set_shape`](crate::SeatHandler::request_set_shape)
    /// or [`SeatHandler::request_activate`](crate::SeatHandler::request_activate),
    /// there is nothing to *apply* here: [`Runtime::create_gamma_control_manager`](crate::Runtime::create_gamma_control_manager)
    /// wires the manager straight into this runtime's scene
    /// (`wlr_scene_set_gamma_control_manager_v1`), which is wlroots' own
    /// recommended integration — the scene renderer applies the ramp (or
    /// signals `failed`) itself, on its own commit path, before this handler
    /// ever runs. This exists purely for a compositor that wants to react —
    /// log the change, or keep its own idea of the output's gamma state in
    /// step.
    fn gamma_control_changed(&mut self, output: OutputId) {
        let _ = output;
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
    /// [`Until::Stop`](crate::Until::Stop) — but *between* turns, never
    /// during one. Under [`Until::Stop`](crate::Until::Stop) a turn only
    /// happens when the loop has something to dispatch, so a run whose events
    /// dry up blocks in `poll` and never asks again: this bounds a run that
    /// keeps receiving events, and cannot rescue one that stops. If you need
    /// a run that ends whatever the event traffic does, use
    /// [`Until::Turns`](crate::Until::Turns), whose count is enforced by
    /// `run_all` itself.
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
/// `new_layer_surface`, `layer_surface_commit`, `layer_surface_mapped`,
/// `layer_surface_unmapped` and `layer_surface_destroyed` were added in
/// 0.20.11, on the same additive terms — wlr-layer-shell surfaces are not
/// xdg-shell toplevels, but they share this trait rather than getting their
/// own for the identical reason `request_decoration_mode` does: one more
/// defaulted method costs an implementor nothing, and a second trait would
/// cost every consumer a second (also-empty) `impl` block.
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
    /// `preference` is what the client asked for:
    /// `Some(`[`DecorationMode::ClientSide`]`)` or
    /// `Some(`[`DecorationMode::ServerSide`]`)`, or `None` if it stated no
    /// preference at all (`requested_mode` reads
    /// `WLR_XDG_TOPLEVEL_DECORATION_V1_MODE_NONE`) — a client that has not
    /// called `set_mode`, which the protocol allows and which is also how
    /// this fires for a decoration whose client never asks at all.
    ///
    /// Answer with
    /// [`Runtime::set_decoration_mode`](crate::Runtime::set_decoration_mode)
    /// from inside this call. It takes the same [`DecorationMode`], so
    /// honoring the client is `set_decoration_mode(id, pref)` for whatever
    /// `pref` arrived — a preference is only a request, though, and
    /// answering with the other variant is equally valid.
    ///
    /// wlroots requires a mode be set before the toplevel's initial commit
    /// is answered, so — same coalescing pattern as
    /// [`request_maximize`](ToplevelHandler::request_maximize)'s bare
    /// configure — the dispatch layer sends
    /// [`DecorationMode::ServerSide`] after this method returns, but *only*
    /// if this method sent nothing itself: unlike the configure case,
    /// sending twice here is not harmless (it would tell the client
    /// server-side and then, in the same turn, whatever this method asked
    /// for), so the default is conditional rather than unconditional.
    fn request_decoration_mode(&mut self, id: ToplevelId, preference: Option<DecorationMode>) {
        let _ = (id, preference);
    }

    /// A client created a wlr-layer-shell surface (`get_layer_surface`).
    ///
    /// Added in 0.20.11, additively, on this trait rather than a new one —
    /// see this trait's own doc for why `request_maximize` and its 0.20.7
    /// siblings live here rather than on a `LayerShellHandler`: every method
    /// is defaulted, so an `impl ToplevelHandler for MyState {}` written
    /// against any earlier 0.20.x still compiles unchanged.
    ///
    /// Like [`new_toplevel`](ToplevelHandler::new_toplevel), the surface is
    /// not mapped yet and has no buffer. Answer with
    /// [`Runtime::configure_layer_surface`](crate::Runtime::configure_layer_surface)
    /// from here (or later, from
    /// [`layer_surface_commit`](ToplevelHandler::layer_surface_commit)) —
    /// see that method's own doc for what happens if nothing ever answers.
    /// Record [`LayerSurface::id`] if you need to refer to this surface
    /// later; the handle is valid only for this call.
    fn new_layer_surface(&mut self, surface: &LayerSurface<'_>) {
        let _ = surface;
    }

    /// The client committed to this layer surface's underlying `wl_surface`.
    ///
    /// Unlike [`initial_commit`](ToplevelHandler::initial_commit), this
    /// fires on **every** commit, not only the first — wlr-layer-shell
    /// clients routinely re-anchor, resize their exclusive zone, or change
    /// margins after mapping, each by way of another `wl_surface.commit`,
    /// and a compositor generally wants to see all of them, not just the
    /// one that maps the surface. Use
    /// [`LayerSurface::layer`]/[`anchor`](LayerSurface::anchor)/etc. to read
    /// whatever changed.
    fn layer_surface_commit(&mut self, surface: &LayerSurface<'_>) {
        let _ = surface;
    }

    /// The layer surface has a buffer and should be displayed. Only the id,
    /// mirroring [`mapped`](ToplevelHandler::mapped)'s toplevel counterpart
    /// — the crate has already inserted this surface into the scene graph
    /// by this point.
    fn layer_surface_mapped(&mut self, id: LayerSurfaceId) {
        let _ = id;
    }

    /// The layer surface should not be displayed any more. Not the same as
    /// destruction — mirrors
    /// [`unmapped`](ToplevelHandler::unmapped)'s toplevel counterpart
    /// exactly, including that a layer surface can unmap and remap while
    /// keeping its id.
    fn layer_surface_unmapped(&mut self, id: LayerSurfaceId) {
        let _ = id;
    }

    /// The layer surface is gone. Only the id, for the identical reason
    /// [`toplevel_destroyed`](ToplevelHandler::toplevel_destroyed) documents
    /// — including that **`id` may be one you were never told about**, on
    /// the same "queued behind a running handler" grounds. Write this so an
    /// unknown id is harmless.
    fn layer_surface_destroyed(&mut self, id: LayerSurfaceId) {
        let _ = id;
    }

    /// Xwayland is up: the X server has started and its window manager (`xwm`)
    /// is running. `display_name` is the `:N` to export as `DISPLAY` so X11
    /// children connect to this compositor. The crate has already pointed
    /// Xwayland at its own seat by the time this runs (so the X11 to Wayland
    /// clipboard/primary/DND bridge is live); the compositor's job here is to
    /// publish `DISPLAY` into the environment its session children inherit.
    ///
    /// Added on the same additive terms as
    /// [`SeatHandler::session_lock_changed`](crate::SeatHandler::session_lock_changed):
    /// defaulted, and — like every Xwayland method here — feature-gated behind
    /// `wlr_has_xwayland`, so a build without the Xwayland subsystem never sees
    /// it and an `impl ToplevelHandler for MyState {}` still compiles in both
    /// configurations. These methods live on [`ToplevelHandler`] rather than a
    /// new `XwaylandHandler` trait for the reason this trait's own doc gives
    /// for absorbing wlr-layer-shell: one more defaulted method costs an
    /// implementor nothing, whereas a new supertrait on [`Handlers`] would be a
    /// breaking change to a frozen list.
    ///
    /// `display_name` is `None` on the rare occasion wlroots reports the server
    /// ready with no name string yet set.
    #[cfg(wlr_has_xwayland)]
    fn xwayland_ready(&mut self, display_name: Option<&str>) {
        let _ = display_name;
    }

    /// A new X11 window appeared. It has **no** content surface yet — an X11
    /// window is created before a client attaches a `wlr_surface` to it — so
    /// nothing about its buffer is known, and it is neither mapped nor in the
    /// scene. Record [`XwaylandSurface::id`] here; the handle is valid only for
    /// this call.
    ///
    /// The lifecycle from here is two-phase and distinct from xdg-shell's:
    /// [`xwayland_surface_associate`](ToplevelHandler::xwayland_surface_associate)
    /// when a `wlr_surface` is attached (the crate builds the scene node then),
    /// [`xwayland_surface_mapped`](ToplevelHandler::xwayland_surface_mapped)
    /// when that surface first commits a buffer, and the mirror
    /// `unmapped`/`unassociate`/`destroyed` on the way down. A window may
    /// unassociate and re-associate while keeping its id.
    #[cfg(wlr_has_xwayland)]
    fn new_xwayland_surface(&mut self, surface: &XwaylandSurface<'_>) {
        let _ = surface;
    }

    /// A `wlr_surface` was attached to this X11 window. The crate has already
    /// built the surface's scene node by the time this runs (there is no
    /// `wlr_scene_xdg_surface_create` analogue for X11, so it is a plain
    /// subsurface tree). This is the earliest point the window has content to
    /// size and place.
    #[cfg(wlr_has_xwayland)]
    fn xwayland_surface_associate(&mut self, surface: &XwaylandSurface<'_>) {
        let _ = surface;
    }

    /// The `wlr_surface` went away, but the X11 window lives on and may
    /// re-associate later. The crate has torn its scene node down. Only the
    /// id, because there is no content handle to borrow.
    #[cfg(wlr_has_xwayland)]
    fn xwayland_surface_unassociate(&mut self, id: XwaylandSurfaceId) {
        let _ = id;
    }

    /// The associated surface committed a buffer and should be displayed —
    /// the X11 counterpart of [`mapped`](ToplevelHandler::mapped). The crate
    /// has already inserted the surface into the scene graph by this point, so
    /// positioning it here takes effect on the first frame it appears in.
    #[cfg(wlr_has_xwayland)]
    fn xwayland_surface_mapped(&mut self, surface: &XwaylandSurface<'_>) {
        let _ = surface;
    }

    /// The window should not be displayed any more — a null buffer, or the
    /// surface unassociating. **Not** destruction: a window can unmap and map
    /// again, keeping its id. Only the id, mirroring
    /// [`unmapped`](ToplevelHandler::unmapped).
    #[cfg(wlr_has_xwayland)]
    fn xwayland_surface_unmapped(&mut self, id: XwaylandSurfaceId) {
        let _ = id;
    }

    /// The X11 window is gone. Only the id, for the identical reason
    /// [`toplevel_destroyed`](ToplevelHandler::toplevel_destroyed) documents,
    /// including that **`id` may be one you were never told about**. Write this
    /// so an unknown id is harmless — `remove` on a map, never indexing.
    #[cfg(wlr_has_xwayland)]
    fn xwayland_surface_destroyed(&mut self, id: XwaylandSurfaceId) {
        let _ = id;
    }

    /// The window's title (`_NET_WM_NAME`) changed. The whole handle is passed
    /// so the new [`XwaylandSurface::title`] can be read.
    #[cfg(wlr_has_xwayland)]
    fn xwayland_title_changed(&mut self, surface: &XwaylandSurface<'_>) {
        let _ = surface;
    }

    /// The window's `WM_CLASS` changed. The whole handle is passed so the new
    /// [`XwaylandSurface::class`]/[`instance`](XwaylandSurface::instance) can
    /// be read — the values a compositor maps to `app_id`.
    #[cfg(wlr_has_xwayland)]
    fn xwayland_class_changed(&mut self, surface: &XwaylandSurface<'_>) {
        let _ = surface;
    }

    /// The client asked to move and/or resize itself — X11 windows
    /// self-position. `geometry` is the requested `x`/`y`/`width`/`height`. The
    /// crate does **not** apply it: a compositor honours it for override-
    /// redirect surfaces and initial dialog placement, then constrains managed
    /// windows through its own layout, answering with
    /// [`Runtime::configure_xwayland_surface`](crate::Runtime::configure_xwayland_surface).
    #[cfg(wlr_has_xwayland)]
    fn xwayland_request_configure(&mut self, id: XwaylandSurfaceId, geometry: Box2D) {
        let _ = (id, geometry);
    }

    /// The client asked to begin an interactive move — the X11 counterpart of
    /// [`request_move`](ToplevelHandler::request_move), carried by
    /// `_NET_WM_MOVERESIZE`. The compositor drives the same interactive-move
    /// grab it uses for xdg toplevels, writing the result back through
    /// [`Runtime::configure_xwayland_surface`](crate::Runtime::configure_xwayland_surface).
    #[cfg(wlr_has_xwayland)]
    fn xwayland_request_move(&mut self, id: XwaylandSurfaceId) {
        let _ = id;
    }

    /// The client asked to begin an interactive resize — the X11 counterpart
    /// of [`request_resize`](ToplevelHandler::request_resize), carried by
    /// `_NET_WM_MOVERESIZE`. `edges` names the edge or corner being dragged.
    #[cfg(wlr_has_xwayland)]
    fn xwayland_request_resize(&mut self, id: XwaylandSurfaceId, edges: Edges) {
        let _ = (id, edges);
    }

    /// The client asked to (un)maximize itself, by changing
    /// `_NET_WM_STATE_MAXIMIZED_{HORZ,VERT}`. `maximized` is the requested
    /// state, read from the window at emission time. The X11 counterpart of
    /// [`request_maximize`](ToplevelHandler::request_maximize); the compositor
    /// reflects the decision back with
    /// [`Runtime::set_xwayland_surface_maximized`](crate::Runtime::set_xwayland_surface_maximized).
    #[cfg(wlr_has_xwayland)]
    fn xwayland_request_maximize(&mut self, id: XwaylandSurfaceId, maximized: bool) {
        let _ = (id, maximized);
    }

    /// The client asked to enter or leave fullscreen, by changing
    /// `_NET_WM_STATE_FULLSCREEN`. `fullscreen` is the requested state, read
    /// from the window at emission time. The X11 counterpart of
    /// [`request_fullscreen`](ToplevelHandler::request_fullscreen).
    #[cfg(wlr_has_xwayland)]
    fn xwayland_request_fullscreen(&mut self, id: XwaylandSurfaceId, fullscreen: bool) {
        let _ = (id, fullscreen);
    }

    /// The client asked to (un)minimize itself. `minimized` is the requested
    /// state, carried inline by the X11 `wlr_xwayland_minimize_event`. The
    /// compositor reflects the decision back with
    /// [`Runtime::set_xwayland_surface_minimized`](crate::Runtime::set_xwayland_surface_minimized).
    #[cfg(wlr_has_xwayland)]
    fn xwayland_request_minimize(&mut self, id: XwaylandSurfaceId, minimized: bool) {
        let _ = (id, minimized);
    }

    /// The client asked to be activated (`_NET_ACTIVE_WINDOW`) — a focus-steal
    /// request. The X11 counterpart of the xdg-activation path; A1 policy is
    /// honour-or-mark-urgent, so a compositor may focus the window or merely
    /// flag it for attention.
    #[cfg(wlr_has_xwayland)]
    fn xwayland_request_activate(&mut self, id: XwaylandSurfaceId) {
        let _ = id;
    }

    /// A surface flipped its override-redirect flag at runtime. The whole
    /// handle is passed — mirroring [`xwayland_title_changed`](ToplevelHandler::xwayland_title_changed)
    /// — so the compositor can read the new
    /// [`override_redirect`](XwaylandSurface::override_redirect) value **and**
    /// the identity (class/title/pid/geometry) it needs to re-model the surface
    /// on the path it is migrating *to*. The compositor must migrate the surface
    /// between the managed-window model (now `false`) and the unmanaged pop-up
    /// path (now `true`).
    #[cfg(wlr_has_xwayland)]
    fn xwayland_override_redirect_changed(&mut self, surface: &XwaylandSurface<'_>) {
        let _ = surface;
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

    /// The session's lock state changed. `locked = true` when a locker takes a
    /// lock (the compositor should suspend its own focus/layout work and stop
    /// painting normal chrome); `locked = false` **only** on a genuine
    /// `unlock` (never when a locker dies — the session stays locked then, so
    /// this is not called on that path). Defaulted to a no-op.
    ///
    /// The crate has already enforced the security half by the time this runs:
    /// while locked, no normal toplevel or layer surface can take keyboard or
    /// pointer focus regardless of what a handler does here (see
    /// [`Runtime::is_session_locked`](crate::Runtime::is_session_locked)). This
    /// callback is the compositor's cue to stop *its own* bookkeeping, not the
    /// thing that makes the lock safe.
    fn session_lock_changed(&mut self, locked: bool) {
        let _ = locked;
    }

    /// A client asked to change the cursor image via `cursor-shape-v1`.
    /// Defaulted to a no-op: wlroots does not apply the request itself (its
    /// own doc on `wlr_cursor_shape_manager_v1` says a compositor should
    /// handle this "in the same way as `wlr_seat.events.request_set_cursor`"),
    /// so nothing changes on screen unless a compositor overrides this and
    /// calls [`crate::Runtime::set_cursor_shape`] — typically unconditionally,
    /// the way `request_set_cursor` implementations usually do, though
    /// `device`/`serial` are available to a compositor that wants to
    /// validate against its own idea of which device currently owns the
    /// cursor.
    fn request_set_shape(&mut self, device: CursorShapeDevice, serial: u32, shape: CursorShape) {
        let _ = (device, serial, shape);
    }

    /// A client asked, via `xdg-activation-v1`, that a surface be given
    /// focus. `target` is the surface named by the request, mapped to this
    /// crate's own id — `None` if it names no tracked toplevel. `token`
    /// carries the wlroots-validated evidence (serial, seat, requesting
    /// surface) a focus-steal policy decides from.
    ///
    /// Defaulted to a no-op: wlroots does not steal focus itself (the
    /// activation protocol exists precisely so the *compositor* decides
    /// whether an unfocused client's request to be raised is honored), so a
    /// compositor that wants `xdg_activation_v1` to do anything overrides
    /// this and applies its own policy — e.g. focusing `target` only when
    /// [`ActivationToken::has_seat`] is `true`.
    fn request_activate(&mut self, target: Option<ToplevelId>, token: ActivationToken) {
        let _ = (target, token);
    }
}

/// Every handler trait at once.
///
/// The bound on [`Backend::run_all`](crate::Backend::run_all), and
/// blanket-implemented, so a consumer never writes `impl Handlers` — they
/// implement whichever of the traits they care about (all methods are
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
