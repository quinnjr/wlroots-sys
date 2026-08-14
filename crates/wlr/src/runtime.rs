//! The long-lived side of a compositor: everything a run wires up, and every
//! operation that names an object by id.
//!
//! # Why this type exists
//!
//! Handles cannot be stored (see the crate docs), handlers receive only
//! `&mut S`, and a `Dispatcher<S>` exists only for the duration of one
//! [`Backend::run_all`](crate::Backend::run_all) call. Three consequences
//! follow, and this type is the answer to all three:
//!
//! 1. An fd source cannot be registered against the event loop before a run,
//!    because the C callback has no dispatcher to reach. So sources are
//!    *declared* here and **registered by each run**, torn down when it
//!    returns, and re-armed by the next one — the same lifetime the per-output
//!    listeners already have.
//! 2. A mutation that names an object by id (`set_toplevel_size`, and its
//!    siblings from 0.20.2 on) has to be callable from a handler, which can
//!    reach nothing but its own `&mut S`. So `Runtime` is `Clone` and cheap:
//!    a consumer keeps a clone in their state and calls through it.
//! 3. The tables that turn an id back into a live object outlive any one
//!    handler call but must be readable during one, so they are `RefCell`s.
//!    **No borrow may be held across a call into consumer code.** Copy the
//!    pointer out, drop the borrow, then call — `backend.rs`'s `with_output`
//!    is the pattern. A double borrow inside an `extern "C"` frame is an
//!    abort, not a caught panic.
//!
//! `Runtime` is `!Send`/`!Sync` (its `Rc` and `NonNull` fields see to that),
//! which the thread-scoped dispatch guard in `dispatch.rs` depends on.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr::NonNull;
use std::rc::Rc;

use crate::buffer::create_pixel_buffer;
use crate::decoration::{DecorationEntry, DecorationMode};
use crate::id::{SourceId, next_id};
use crate::layer::Layer;
use crate::scene::RectId;
use crate::{
    Backend, BufferId, Display, Error, Interest, LayerSurfaceId, Output, OutputId, Result,
    ToplevelId, sys,
};

/// A declared fd source: the descriptor, what it wants, and its id.
///
/// Owns the `OwnedFd` so the descriptor cannot be closed while a run has it
/// registered with the event loop — the one hazard a `RawFd` parameter would
/// have left open, and one no amount of documentation prevents.
pub(crate) struct FdSource {
    pub(crate) fd: OwnedFd,
    pub(crate) interest: Interest,
    pub(crate) id: SourceId,
}

pub(crate) struct RuntimeInner {
    pub(crate) sources: RefCell<Vec<FdSource>>,

    /// The scene graph, the output layout it is attached to, and the renderer
    /// and allocator every output has to be initialised against.
    ///
    /// `None` until [`Runtime::init_graphics`] runs, and every field is set
    /// together at that point — there is no state in which one exists but
    /// another does not. This is a deliberate departure from creating the
    /// scene eagerly in [`Runtime::new`]: nothing in this crate can start a
    /// backend (and so nothing can trigger `new_output`) before a consumer
    /// calls [`Backend::run`](crate::Backend::run) or
    /// [`Backend::run_all`](crate::Backend::run_all) — `wlr_backend_start`,
    /// which is what actually announces outputs, is deferred to those calls
    /// (see `backend.rs`'s `ensure_started`) — so there is no window in which
    /// an output could arrive before `init_graphics` has had the chance to
    /// run. Deferring also gives [`Runtime::add_rect`] an honest
    /// [`Error::Create`] before `init_graphics`, instead of a rect silently
    /// attached to a scene no output will ever be attached to.
    pub(crate) graphics: RefCell<Option<Graphics>>,

    pub(crate) rects: RefCell<HashMap<RectId, RectEntry>>,

    /// Every live RGBA pixel-buffer scene node. Same shape and same purge
    /// rules as `rects` — see [`BufferEntry`]'s own doc.
    pub(crate) buffers: RefCell<HashMap<BufferId, BufferEntry>>,

    /// Every fd source currently registered with the event loop, keyed by
    /// the id its declaration in `sources` carries.
    ///
    /// Populated by `backend.rs`'s `register_fd_sources` right after each
    /// source is handed to `wl_event_loop_add_fd`, and cleared by
    /// `run_inner`'s teardown guard when that call returns — the same
    /// "declared here, registered and unregistered per run" split
    /// `sources` documents on [`Runtime::add_fd`]. [`Runtime::remove_fd`]
    /// consults this to decide whether a live registration needs
    /// `wl_event_source_remove` in addition to dropping the declaration:
    /// a source declared but not yet registered (no run has started, or
    /// the removal races the read of this table before a run's
    /// `register_fd_sources` runs) has nothing here to remove.
    pub(crate) live_sources: RefCell<HashMap<SourceId, NonNull<sys::wl_event_source>>>,

    /// Descriptors [`Runtime::remove_fd`] has withdrawn but not yet closed,
    /// because the call happened from inside a handler and closing
    /// synchronously could invalidate a [`BorrowedFd`](std::os::fd::BorrowedFd)
    /// that handler's own [`FdHandler::fd_ready`](crate::FdHandler::fd_ready)
    /// call — or one further up the same synchronous call stack — is still
    /// holding.
    ///
    /// Drained by `backend.rs`'s `run_inner`, once per dispatch turn,
    /// immediately after `wl_event_loop_dispatch` returns: at that point
    /// every callback invoked during the turn has returned, so no
    /// `BorrowedFd` handed out during it can still be alive, and closing is
    /// safe. See [`Runtime::remove_fd`]'s own doc for the full argument.
    pub(crate) pending_close: RefCell<Vec<OwnedFd>>,

    /// The xdg shell, once created. `Option` because a consumer that only
    /// wants a scene never makes one, and because a second one would
    /// advertise a second `xdg_wm_base` global.
    pub(crate) xdg_shell: RefCell<Option<NonNull<sys::wlr_xdg_shell>>>,

    /// The xdg-decoration manager, once created. `Option` for the same
    /// reason `xdg_shell` is: a consumer that never negotiates decorations
    /// never calls [`Runtime::create_xdg_decoration_manager`], and a second
    /// call would advertise a second `zxdg_decoration_manager_v1` global.
    pub(crate) xdg_decoration_manager: RefCell<Option<NonNull<sys::wlr_xdg_decoration_manager_v1>>>,

    /// The primary-selection (`zwp_primary_selection_device_manager_v1`)
    /// manager, once created — middle-click paste. `Option` for the same
    /// reason `xdg_decoration_manager` is: a consumer that never calls
    /// [`Runtime::create_primary_selection_manager`] never advertises the
    /// global, and a second call would advertise a second one.
    pub(crate) primary_selection_manager:
        RefCell<Option<NonNull<sys::wlr_primary_selection_v1_device_manager>>>,

    /// The data-control (`zwlr_data_control_manager_v1`) manager, once
    /// created — lets a clipboard manager observe and set the selection.
    /// `Option`, same rationale as the other manager globals.
    pub(crate) data_control_manager: RefCell<Option<NonNull<sys::wlr_data_control_manager_v1>>>,

    /// The virtual-keyboard (`zwp_virtual_keyboard_manager_v1`) manager, once
    /// created — lets a client inject a keyboard, e.g. an on-screen keyboard,
    /// remote-input bridge, or a test harness that needs a real input serial.
    /// `Option`, same rationale as the other manager globals.
    pub(crate) virtual_keyboard_manager:
        RefCell<Option<NonNull<sys::wlr_virtual_keyboard_manager_v1>>>,

    /// The virtual-pointer (`zwlr_virtual_pointer_manager_v1`) manager, once
    /// created — lets a client inject a pointer, e.g. a remote-input bridge
    /// or a test harness that needs a real input serial. `Option`, same
    /// rationale as the other manager globals.
    pub(crate) virtual_pointer_manager:
        RefCell<Option<NonNull<sys::wlr_virtual_pointer_manager_v1>>>,

    /// Every live toplevel: the role object, its scene tree, and the surface
    /// its id addon lives on.
    pub(crate) toplevels: RefCell<HashMap<ToplevelId, ToplevelEntry>>,

    /// Every live decoration object, keyed by the toplevel it was created
    /// for. At most one entry per [`ToplevelId`] — a client can only ever
    /// hold one `zxdg_toplevel_decoration_v1` per toplevel, since wlroots
    /// itself refuses a second `get_toplevel_decoration` on the same
    /// surface.
    ///
    /// A decoration and its toplevel can die in either order (a decoration
    /// resource outlives its toplevel if the client destroys the toplevel
    /// first, and wlroots merely clears `decoration->toplevel` to null when
    /// that happens), so this table is purged from two independent places:
    /// [`Runtime::forget_decoration`], called when the decoration's own
    /// `destroy` fires, and [`Runtime::forget_toplevel`], which also drops
    /// any entry here for the toplevel it is forgetting.
    pub(crate) decorations: RefCell<HashMap<ToplevelId, DecorationEntry>>,

    /// The layer shell, once created. `Option` for the same reason
    /// `xdg_shell` is: a consumer that never calls
    /// [`Runtime::create_layer_shell`] never advertises
    /// `zwlr_layer_shell_v1`, and a second call would advertise a second
    /// one.
    pub(crate) layer_shell: RefCell<Option<NonNull<sys::wlr_layer_shell_v1>>>,

    /// Every live layer surface: the role object and the scene tree
    /// [`wlr_scene_layer_surface_v1_create`](sys::wlr_scene_layer_surface_v1_create)
    /// created for it. Mirrors `toplevels` in shape and in lifetime — purged
    /// synchronously by `backend.rs`'s `on_layer_surface_destroy` before
    /// wlroots frees the role object, and cleared wholesale by
    /// `LayerSurfaceTableGuard` when the `run_all` call that populated it
    /// returns, the same "an id is only good for the call that announced it"
    /// rule every other by-id table here follows.
    pub(crate) layer_surfaces: RefCell<HashMap<LayerSurfaceId, LayerSurfaceEntry>>,

    /// Reverse lookup for the scene hit test, which finds a `wlr_scene_tree`
    /// and has to name the toplevel it belongs to. Keyed by the tree pointer
    /// because that is what `wlr_scene_node_at` walks back to.
    ///
    /// Read by [`Runtime::toplevel_at`], the hit test that turns a
    /// `wlr_scene_node_at` result back into a `ToplevelId`, added in 0.20.4
    /// alongside seat and pointer input. Kept in step with `toplevels` since
    /// 0.20.2 — `record_toplevel`/`forget_toplevel`/`clear_toplevels` write
    /// both together — rather than added alongside the hit test itself, so a
    /// toplevel announced under 0.20.2 was already indexed by the time
    /// 0.20.4 started reading this table.
    pub(crate) tree_to_toplevel: RefCell<HashMap<usize, ToplevelId>>,

    /// The seat, once [`Runtime::create_seat`] has run. `None` for a
    /// consumer that only wants a scene and never takes input.
    pub(crate) seat: RefCell<Option<NonNull<sys::wlr_seat>>>,
    /// The cursor, created alongside the seat and attached to this runtime's
    /// output layout. `None` exactly when `seat` is.
    pub(crate) cursor: RefCell<Option<NonNull<sys::wlr_cursor>>>,
    /// The xcursor theme manager, created alongside the seat. `None` exactly
    /// when `seat` is.
    pub(crate) xcursor: RefCell<Option<NonNull<sys::wlr_xcursor_manager>>>,
    /// Whether [`Runtime::create_seat`] has already asked the xcursor
    /// manager to load its theme at the default scale — done once, lazily,
    /// on the first pointer event rather than eagerly in `create_seat`,
    /// since loading a theme touches the filesystem and a consumer that
    /// never gets a pointer device should not pay for it.
    pub(crate) cursor_image_loaded: std::cell::Cell<bool>,

    /// Every live keyboard the backend has announced, so capabilities can be
    /// recomputed as devices arrive and leave.
    ///
    /// Never dereferenced — only its length is read, by
    /// [`Runtime::has_keyboard`], to decide whether the seat should
    /// advertise the keyboard capability. Pruned by `backend.rs`'s
    /// `on_input_destroy` — the same per-device destroy watch that unlinks
    /// the keyboard's `key`/`modifiers` listeners before wlroots' own
    /// `wlr_keyboard_finish` asserts they are gone — so this never carries a
    /// stale entry for longer than the device itself lives.
    pub(crate) keyboards: RefCell<Vec<NonNull<sys::wlr_keyboard>>>,

    /// Every live pointer the backend has announced. Same shape and same
    /// pruning as `keyboards`, for [`Runtime::has_pointer`].
    pub(crate) pointers: RefCell<Vec<NonNull<sys::wlr_pointer>>>,

    /// Every output this run has announced, so a by-id mutator
    /// ([`Runtime::output_layout_box`], [`Runtime::set_output_position`])
    /// can resolve an [`OutputId`] back to the `*mut wlr_output` it names.
    ///
    /// Mirrors `toplevels` exactly, including its lifetime: recorded by
    /// `backend.rs`'s `on_new_output` before the handler is told, purged
    /// synchronously by `on_output_destroy` before wlroots frees the
    /// output, and cleared wholesale when the `run_all` call that populated
    /// it returns (`OutputTableGuard`, the same "per-run, not per-`Runtime`"
    /// rule `ToplevelTableGuard` enforces — see that guard's own doc). An
    /// `OutputId` kept past that point reports `None` here rather than
    /// resolving to a pointer wlroots may have already reused or freed.
    pub(crate) outputs: RefCell<HashMap<OutputId, NonNull<sys::wlr_output>>>,

    /// The `wl_display` [`Runtime::init_graphics`] was given, as a `usize`
    /// — `0` before `init_graphics` has run. See `Runtime`'s own doc for
    /// the obligation this exists to catch a violation of: every clone of
    /// this handle must not outlive the `Display` it was initialised
    /// against, since wlroots frees the output layout (and the scene-output
    /// layout attached to it) when that display dies, and the graphics
    /// mutators dereference a pointer that shares the layout's lifetime.
    ///
    /// Read, not just written, at four sites — deliberately not *every*
    /// graphics mutator:
    ///
    /// - [`Backend::run_all`](crate::Backend::run_all), at entry, compares
    ///   this against the `Display` it is about to drive (its authoritative
    ///   argument, so the comparison is direct, not via
    ///   [`current_display`](crate::dispatch::current_display)). This is the
    ///   listener-linking choke point: a `run_all` for the wrong `Display`
    ///   would link the cached shell/decoration/layer-shell listeners into a
    ///   freed display's `wl_list`s.
    /// - [`Runtime::add_rect`], [`Runtime::add_rect_in_band`] and
    ///   [`Runtime::commit_output`] each `debug_assert_eq!` this against
    ///   [`current_display`](crate::dispatch::current_display) — the
    ///   `wl_display` [`Backend::run_all`](crate::Backend::run_all) is
    ///   *currently* driving on this thread, pinned for the call's duration
    ///   by [`crate::dispatch::DisplayPinGuard`]. A `0` there means "no live
    ///   `run_all` to compare against", and is skipped rather than treated as
    ///   agreement.
    ///
    /// A mismatch at any of them means this `Runtime` clone is being driven by
    /// a `run_all` call for a *different* `Display` than the one it was
    /// initialised against — exactly the misuse this field exists to catch.
    /// These four are the choke points where such a reuse first becomes
    /// observable: `run_all` is where the freed-display listeners would be
    /// linked, and `add_rect`/`add_rect_in_band`/`commit_output` are the
    /// per-run graphics calls a scene is first established through. Every
    /// other graphics mutator (`init_output`, the `add_*_in_toplevel`
    /// helpers, the `set_rect_*`/`set_buffer_*` setters) runs only *after*
    /// one of these has already fired for the same run, so it would catch the
    /// identical mismatch a beat later and no earlier — the check is not
    /// duplicated onto them to avoid that redundancy, not because they are
    /// exempt from the invariant.
    ///
    /// `debug_assert_eq!` rather than a real check: this is a bug detector for
    /// a mistake nothing here can recover from cleanly (the layout may already
    /// be freed), not a recoverable error a release build should pay to guard
    /// — see each assert site's own comment. Because it fires from inside
    /// handler callbacks, which this crate reaches across `extern "C"` frames,
    /// a tripped assertion unwinds into a C frame and so (Rust 1.81+) is
    /// turned into a process **abort**, not a catchable unwind — it terminates
    /// the compositor rather than propagating. One accepted blind spot: the
    /// comparison is of `wl_display` addresses as `usize`, so a `Display`
    /// freed and a new one allocated at the very same address would compare
    /// equal and pass falsely (ABA) — tolerable for a debug-only detector that
    /// makes no safety guarantee a release build relies on.
    pub(crate) pinned_display: std::cell::Cell<usize>,
}

#[derive(Clone, Copy)]
pub(crate) struct ToplevelEntry {
    pub(crate) raw: NonNull<sys::wlr_xdg_toplevel>,
    pub(crate) tree: NonNull<sys::wlr_scene_tree>,
}

/// A live layer surface: the role object, the scene tree
/// `wlr_scene_layer_surface_v1_create` created for it, and any configure
/// size waiting for this surface's initial commit to become safe to send.
///
/// Not `Copy`, unlike [`ToplevelEntry`]: `staged_configure` is a `Cell`, and
/// `Cell` is never `Copy` regardless of what it holds. Every accessor that
/// needs to read `raw`/`scene_tree` outside a held borrow copies just that
/// field out — `layer_surface_ptr`/`layer_surface_scene_ptr` — the same
/// narrowing [`crate::decoration::DecorationEntry`]'s own accessors do for
/// the identical reason.
pub(crate) struct LayerSurfaceEntry {
    pub(crate) raw: NonNull<sys::wlr_layer_surface_v1>,
    pub(crate) scene_tree: NonNull<sys::wlr_scene_tree>,

    /// A size [`Runtime::configure_layer_surface`] recorded instead of
    /// sending, because the surface was not yet initialized when the call
    /// was made. See `layer.rs`'s own module doc for the full argument, and
    /// `backend.rs`'s `on_layer_surface_commit` for where this is taken and
    /// actually sent.
    pub(crate) staged_configure: std::cell::Cell<Option<(u32, u32)>>,

    /// The band [`scene_tree`](LayerSurfaceEntry::scene_tree) is currently
    /// parented under — set at creation to the layer the client asked for,
    /// and updated by [`Runtime::reparent_layer_surface_if_changed`]
    /// whenever a later commit reports a different
    /// [`Layer`](crate::Layer). This is what lets that method tell "the
    /// client changed layers" apart from "the client committed again with
    /// the same layer it already had", so an unchanged layer never pays for
    /// a `wlr_scene_node_reparent` call it does not need.
    pub(crate) band: std::cell::Cell<Layer>,
}

/// A named scene band a rect (or, in principle, anything else this crate
/// later parents by band) can live in — the same five stacking bands
/// `Graphics` creates, plus [`Band::Toplevel`] for the band every
/// toplevel's own tree lives in.
///
/// Deliberately **not** [`Layer`]: `Layer` is the public four-variant
/// protocol vocabulary a layer-shell client speaks
/// (`Background`/`Bottom`/`Top`/`Overlay`, `layer.rs`'s own type), and
/// reusing it here would either strand `Band::Toplevel` outside that
/// vocabulary or force a fifth variant onto a type whose four variants are
/// already frozen as of 0.20.x's layer-shell surface. `Band` is a new,
/// separate enum instead, covering exactly the five bands
/// [`Runtime::add_rect_in_band`] can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Band {
    /// Beneath everything — `Graphics::background_band`.
    Background,
    /// Above `Background`, beneath every toplevel — `Graphics::bottom_band`.
    Bottom,
    /// Where every toplevel's own tree lives — `Graphics::toplevel_band`.
    Toplevel,
    /// Above every toplevel, beneath `Overlay` — `Graphics::top_band`.
    Top,
    /// Above everything — `Graphics::overlay_band`.
    Overlay,
}

/// Which scene tree a [`RectEntry`] is parented into, and so how (or
/// whether) it is purged when something else dies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RectParent {
    /// A root rect [`Runtime::add_rect`] created, parented into the scene's
    /// root tree. Outlives every toplevel; only [`Runtime::remove_rect`] or
    /// tearing down the whole scene destroys it.
    Root,
    /// A rect [`Runtime::add_rect_in_toplevel`] created, parented into that
    /// toplevel's own scene tree. Destroyed automatically when the
    /// toplevel is — see [`Runtime::forget_toplevel`] and
    /// [`Runtime::clear_toplevels`], which purge (without destroying) the
    /// table row for one of these, since wlroots is about to (or already
    /// did) free the node itself as part of freeing the toplevel's tree
    /// recursively.
    Toplevel(ToplevelId),
    /// A rect [`Runtime::add_rect_in_band`] created, parented into the
    /// named band's tree — a sibling of every toplevel/layer-surface tree
    /// in that band, not a descendant of any single one of them. Like
    /// `Root`, a band rect is never purged by a toplevel's death (a band
    /// outlives every toplevel that ever lived in it); only
    /// [`Runtime::remove_rect`] or tearing down the scene destroys it.
    Band(Band),
}

/// A live scene rect: the node wlroots created for it, and which tree it is
/// parented into.
#[derive(Clone, Copy)]
pub(crate) struct RectEntry {
    pub(crate) raw: NonNull<sys::wlr_scene_rect>,
    /// See [`RectParent`]. Read by [`Runtime::forget_toplevel`] and
    /// [`Runtime::clear_toplevels`] to purge every rect that dies along
    /// with a destroyed toplevel's tree, without double-destroying the
    /// node wlroots is about to free recursively — and, symmetrically, to
    /// leave `Root`/`Band` rects alone, since neither dies with any
    /// toplevel.
    pub(crate) parent: RectParent,
}

/// A live RGBA pixel-buffer scene node: the node wlroots created for it, and
/// which toplevel (if any) it is parented into.
///
/// Mirrors [`RectEntry`] exactly, including the parent tracking and its
/// purge rules: see [`Runtime::forget_toplevel`] and
/// [`Runtime::clear_toplevels`], both of which purge buffer entries
/// alongside rect entries, without destroying the node — wlroots already
/// destroys it recursively as part of freeing the parent tree.
#[derive(Clone, Copy)]
pub(crate) struct BufferEntry {
    pub(crate) node: NonNull<sys::wlr_scene_buffer>,
    /// `Some(id)` for a buffer [`Runtime::add_buffer_in_toplevel`] created,
    /// parented into that toplevel's own scene tree. `None` for a root
    /// buffer [`Runtime::add_buffer`] created, parented into the scene's
    /// root tree instead.
    pub(crate) parent: Option<ToplevelId>,
}

/// The scene graph, output layout, renderer and allocator — everything
/// [`Runtime::init_graphics`] creates in one call and every later graphics
/// operation reads.
///
/// No `Drop`, but not for one uniform reason — each field's real fate
/// differs:
///
/// - `layout` is destroyed *by wlroots* when the display dies:
///   `wlr_output_layout_create` registers its own destroy listener on the
///   display it is given (confirmed empirically — see the comment on the
///   call in [`Runtime::init_graphics`] explaining why `display`, not null,
///   is passed). `wlr_scene_attach_output_layout`'s own doc says
///   `scene_layout` is destroyed along with whichever of `scene` or
///   `layout` dies first, so it goes with `layout` here. This is exactly
///   why [`Runtime`] must not outlive the [`Display`](crate::Display) it
///   was initialised against (see `Runtime`'s own doc): once the display
///   drops, `layout` and `scene_layout` are already freed, and every
///   pointer this struct holds to them is dangling.
/// - `scene`, `renderer` and `allocator` are **not** torn down by anything
///   in wlroots. `wlr_renderer_destroy`/`wlr_allocator_destroy` exist and a
///   well-behaved compositor calls them on shutdown (tinywl does); a scene
///   is torn down with `wlr_scene_node_destroy` on its root node, not
///   automatically. This crate leaks all three for the process's life
///   instead — deliberately, not by oversight: there is exactly one
///   `Runtime` per process, modelling one compositor's whole-process state,
///   so the OS reclaims the leak at exit exactly as a `Drop` run a moment
///   earlier would have. A future `Drop` impl, should the crate grow
///   support for a `Runtime` that does not simply outlive its process,
///   would need to call `wlr_renderer_destroy`, `wlr_allocator_destroy` and
///   `wlr_scene_node_destroy` on the scene's root node — and *not* touch
///   `layout`, which wlroots may already have freed by then.
pub(crate) struct Graphics {
    pub(crate) scene: NonNull<sys::wlr_scene>,
    pub(crate) layout: NonNull<sys::wlr_output_layout>,
    pub(crate) scene_layout: NonNull<sys::wlr_scene_output_layout>,
    pub(crate) renderer: NonNull<sys::wlr_renderer>,
    pub(crate) allocator: NonNull<sys::wlr_allocator>,

    /// The five stacking bands, direct children of `scene.tree` (the scene
    /// root) in exactly this order — bottom to top:
    /// `background_band`, `bottom_band`, `toplevel_band`, `top_band`,
    /// `overlay_band`. Created once, together, right after `scene` itself
    /// (see [`Runtime::init_graphics`]), and never reordered or reparented
    /// afterward.
    ///
    /// This is the design 0.20.11 ships with, chosen over a two-band
    /// approximation that was caught and replaced before publish — never
    /// released (see `layer.rs`'s [`Layer`](crate::Layer) doc for the full
    /// argument): every toplevel now lives inside `toplevel_band`
    /// (`backend.rs`'s `on_new_toplevel`) instead of directly under the
    /// scene root, and every layer surface lives inside the band matching
    /// its own [`Layer`](crate::Layer) (`on_new_layer_surface`, reparented
    /// by `on_layer_surface_commit` if the client changes layers later).
    /// Because `wlr_scene_tree_create`'s own `scene_node_init` appends each
    /// new sibling at the *end* of its parent's children list
    /// (`wl_list_insert(parent->children.prev, ...)`), creating these five
    /// in this order at start-of-day is what fixes their relative stacking
    /// order permanently: nothing a consumer or a client does afterward can
    /// move `toplevel_band` above `top_band`/`overlay_band`, or below
    /// `background_band`/`bottom_band`, because
    /// `wlr_scene_node_raise_to_top`/`_lower_to_bottom` only reorder
    /// *siblings*, and a toplevel's or layer surface's own node is never a
    /// sibling of a band — it is a descendant, several levels down, of
    /// exactly one.
    ///
    /// [`Runtime::add_rect`]/[`Runtime::add_buffer`] deliberately do **not**
    /// go through a band: they are parented directly into `scene.tree`
    /// itself, as siblings of these five trees, exactly as before this
    /// field existed. A plain root rect/buffer therefore still lands above
    /// everything by default (it is created after every band, so it starts
    /// at the end of the root's own children list, above all five bands),
    /// and [`Runtime::lower_rect_to_bottom`]/[`lower_buffer_to_bottom`](Runtime::lower_buffer_to_bottom)
    /// still put it beneath everything, `background_band` included, because
    /// "everything" is still just its root-level siblings — the bands
    /// themselves never move, so lowering a rect below all of them lowers
    /// it below every toplevel and every layer surface too, exactly as a
    /// background rect needs.
    pub(crate) background_band: NonNull<sys::wlr_scene_tree>,
    pub(crate) bottom_band: NonNull<sys::wlr_scene_tree>,
    pub(crate) toplevel_band: NonNull<sys::wlr_scene_tree>,
    pub(crate) top_band: NonNull<sys::wlr_scene_tree>,
    pub(crate) overlay_band: NonNull<sys::wlr_scene_tree>,
}

impl Graphics {
    /// The scene tree `band` names. Total — every [`Band`] variant maps to
    /// exactly one of the five fields above.
    pub(crate) fn band_tree(&self, band: Band) -> NonNull<sys::wlr_scene_tree> {
        match band {
            Band::Background => self.background_band,
            Band::Bottom => self.bottom_band,
            Band::Toplevel => self.toplevel_band,
            Band::Top => self.top_band,
            Band::Overlay => self.overlay_band,
        }
    }
}

/// Handle to a compositor's long-lived wlroots state.
///
/// Cheap to clone (one `Rc` bump). Every clone names the same underlying
/// state, so a clone kept in a consumer's own state and the one passed to
/// [`Backend::run_all`](crate::Backend::run_all) are interchangeable.
///
/// # Lifetime obligation
///
/// Once [`init_graphics`](Runtime::init_graphics) has run, this handle (and
/// every clone of it) must not outlive the [`Display`](crate::Display) it
/// was given. wlroots frees the output layout — and, with it, the
/// scene-output layout attached to it — when the display is destroyed
/// (`wlr_output_layout_create` registers its own destroy listener on the
/// display it is given), and every later call through this `Runtime` that
/// touches either one (`init_output` directly;
/// `commit_output` and the rect methods indirectly, since they read the
/// scene that shares the layout's lifetime) dereferences whatever pointer
/// `init_graphics` stored, live or not. **This crate detects, but does not
/// prevent, a violation** (0.20.12): `init_graphics` pins the `wl_display`
/// pointer it was given (`RuntimeInner::pinned_display`), and
/// [`add_rect`](Runtime::add_rect), [`add_rect_in_band`](Runtime::add_rect_in_band)
/// and [`commit_output`](Runtime::commit_output) each `debug_assert_eq!`
/// that pin against the `Display` the current
/// [`Backend::run_all`](crate::Backend::run_all) call is actually driving.
/// This is a debug-only bug detector, not a safety net a release build can
/// rely on — `debug_assert!` compiles out entirely under `--release` (see
/// each assert site's own comment) — so it catches a consumer's mistake in
/// a debug build and does nothing to stop it in a release one. Reachability
/// is narrow in practice regardless — a handle to either type only exists
/// inside a handler call, so violating this means a consumer deliberately
/// kept a `Runtime` clone somewhere the `Display` does not reach and used
/// it afterward — but the obligation is real, is not *prevented*, and is
/// the caller's to keep.
#[derive(Clone)]
pub struct Runtime {
    pub(crate) inner: Rc<RuntimeInner>,
}

impl Runtime {
    /// Create an empty runtime.
    ///
    /// # Errors
    ///
    /// None today. It returns [`Result`] so widening this crate's own
    /// fallible setup later — the scene and output layout now live behind
    /// [`init_graphics`](Runtime::init_graphics), which already has real work
    /// that can fail — is never a breaking change to this signature.
    pub fn new() -> Result<Runtime> {
        Ok(Runtime {
            inner: Rc::new(RuntimeInner {
                sources: RefCell::new(Vec::new()),
                graphics: RefCell::new(None),
                rects: RefCell::new(HashMap::new()),
                buffers: RefCell::new(HashMap::new()),
                live_sources: RefCell::new(HashMap::new()),
                pending_close: RefCell::new(Vec::new()),
                xdg_shell: RefCell::new(None),
                xdg_decoration_manager: RefCell::new(None),
                primary_selection_manager: RefCell::new(None),
                data_control_manager: RefCell::new(None),
                virtual_keyboard_manager: RefCell::new(None),
                virtual_pointer_manager: RefCell::new(None),
                toplevels: RefCell::new(HashMap::new()),
                decorations: RefCell::new(HashMap::new()),
                layer_shell: RefCell::new(None),
                layer_surfaces: RefCell::new(HashMap::new()),
                tree_to_toplevel: RefCell::new(HashMap::new()),
                seat: RefCell::new(None),
                cursor: RefCell::new(None),
                xcursor: RefCell::new(None),
                cursor_image_loaded: std::cell::Cell::new(false),
                keyboards: RefCell::new(Vec::new()),
                pointers: RefCell::new(Vec::new()),
                outputs: RefCell::new(HashMap::new()),
                pinned_display: std::cell::Cell::new(0),
            }),
        })
    }

    /// Declare `fd` as an event source, watched for `interest`.
    ///
    /// The runtime takes ownership of the descriptor and closes it when the
    /// last clone of this handle drops. Handlers get it back as a
    /// [`BorrowedFd`](std::os::fd::BorrowedFd) in
    /// [`FdHandler::fd_ready`](crate::FdHandler::fd_ready).
    ///
    /// Registration with the event loop happens inside
    /// [`Backend::run_all`](crate::Backend::run_all) and lives for exactly
    /// that call, so declaring a source during a run has no effect until the
    /// next one. [`Runtime::remove_fd`] (0.20.5) withdraws a source before
    /// its runtime drops.
    pub fn add_fd(&self, fd: OwnedFd, interest: Interest) -> SourceId {
        let id = SourceId(next_id());
        self.inner
            .sources
            .borrow_mut()
            .push(FdSource { fd, interest, id });
        id
    }

    /// Remove `id`'s declaration, and — if a run currently has it
    /// registered with the event loop — the installed `wl_event_source`
    /// too, so no further [`FdHandler::fd_ready`](crate::FdHandler::fd_ready)
    /// fires for it.
    ///
    /// The descriptor itself is closed as a consequence — it is owned by the
    /// declaration this removes from `sources` — but **not necessarily
    /// synchronously**. Safe to call from inside `id`'s own `fd_ready`
    /// callback, which is the motivating case, and that is exactly what
    /// makes synchronous closing wrong: `fd_ready` is handed a
    /// [`BorrowedFd`](std::os::fd::BorrowedFd) whose contract is that the
    /// descriptor stays open for the whole call, and closing it out from
    /// under a live `BorrowedFd` is unsound even though nothing here is
    /// `unsafe`. So when this is called from inside a handler
    /// (`fd_ready`'s or, defensively, any other's), the descriptor is
    /// queued and closed only once `backend.rs`'s `run_inner` finishes the
    /// dispatch turn currently on the stack — by which point every
    /// `BorrowedFd` that turn could have handed out is gone. Called
    /// outside a handler (no run in progress, or between turns), no
    /// `BorrowedFd` can be live, so the descriptor closes immediately.
    ///
    /// `wl_event_source_remove` itself is documented safe to call on the
    /// very source whose callback is calling it, and this method takes no
    /// borrow that a re-entrant call into this `Runtime` would conflict
    /// with.
    ///
    /// `None` if this runtime never issued `id`, or `id` was already
    /// removed — the second call of a double removal misses cleanly.
    pub fn remove_fd(&self, id: SourceId) -> Option<()> {
        let removed = {
            let mut sources = self.inner.sources.borrow_mut();
            let pos = sources.iter().position(|s| s.id == id)?;
            sources.remove(pos)
        };
        // Deferred while a handler could be holding a `BorrowedFd` into
        // this exact descriptor (see this method's own doc); closed
        // immediately otherwise, so a consumer calling this outside a run
        // still sees the descriptor closed synchronously, matching the
        // pre-0.20.5 behaviour for every case that isn't this one's new
        // re-entrant hazard.
        if crate::dispatch::in_handler() {
            self.inner.pending_close.borrow_mut().push(removed.fd);
        } else {
            drop(removed.fd);
        }
        if let Some(source) = self.take_live_source(id) {
            use sys::wayland_sys::ffi_dispatch;
            #[allow(unused_imports)]
            use sys::wayland_sys::server::*;
            // SAFETY: `source` came from `wl_event_loop_add_fd` (recorded by
            // `register_fd_sources`), and `take_live_source` returning
            // `Some` here is the one and only removal of this id from
            // `live_sources` for this source's whole life — `FdRegistration`'s
            // own `Drop` makes the identical call through the identical
            // `take_live_source` at end-of-run, and finding this id already
            // gone is exactly what stops it repeating this call on a source
            // this branch has already removed (see that `Drop` impl's own
            // comment). So `source` has not been removed yet.
            let _ = unsafe {
                ffi_dispatch!(
                    sys::wayland_sys::server::wayland_server_handle(),
                    wl_event_source_remove,
                    source.as_ptr()
                )
            };
        }
        Some(())
    }

    /// Record a source `backend.rs`'s `register_fd_sources` just registered
    /// with the event loop, so [`remove_fd`](Runtime::remove_fd) and
    /// `backend.rs`'s `FdRegistration` teardown can find — and claim — it.
    pub(crate) fn record_live_source(&self, id: SourceId, source: NonNull<sys::wl_event_source>) {
        self.inner.live_sources.borrow_mut().insert(id, source);
    }

    /// Remove and return `id`'s live registration, if it still has one.
    ///
    /// The single choke point both removers of a live source go through —
    /// [`remove_fd`](Runtime::remove_fd), for a mid-run removal, and
    /// `backend.rs`'s `FdRegistration::drop`, for the run's own end-of-call
    /// teardown — so that whichever runs first is the one that actually
    /// calls `wl_event_source_remove`, and the other sees `None` and does
    /// nothing. Without a single shared table to race on, both sites would
    /// each call `wl_event_source_remove` on the same source: the second
    /// call is a use-after-free, since libwayland has already freed the
    /// `wl_event_source` the first call removed.
    pub(crate) fn take_live_source(&self, id: SourceId) -> Option<NonNull<sys::wl_event_source>> {
        self.inner.live_sources.borrow_mut().remove(&id)
    }

    /// Forget every live source, called once by `backend.rs`'s `run_inner`
    /// when the run that registered them returns. In the ordinary case
    /// there is nothing left to forget — every entry `record_live_source`
    /// added has already been claimed by exactly one call to
    /// [`take_live_source`](Runtime::take_live_source) by this point, from
    /// either `remove_fd` or `FdRegistration`'s own `Drop` (dropped before
    /// this guard runs; see `run_inner`'s declaration order). This exists
    /// as the belt for that suspenders: a table entry surviving past its
    /// registering run would let a later [`remove_fd`](Runtime::remove_fd)
    /// call resolve a stale `SourceId` to a pointer libwayland may since
    /// have reused, the same hazard
    /// [`clear_toplevels`](Runtime::clear_toplevels) closes for toplevels.
    pub(crate) fn clear_live_sources(&self) {
        self.inner.live_sources.borrow_mut().clear();
    }

    /// Close every descriptor [`remove_fd`](Runtime::remove_fd) deferred,
    /// called once per turn by `backend.rs`'s `run_inner`, immediately
    /// after `wl_event_loop_dispatch` returns — the point at which every
    /// handler invoked during that turn has returned, so no
    /// [`BorrowedFd`](std::os::fd::BorrowedFd) into any of these
    /// descriptors can still be alive. Dropping each `OwnedFd` is what
    /// actually closes it; an empty list (the ordinary case — most turns
    /// remove nothing) drops nothing and costs one empty `Vec::clear`.
    pub(crate) fn drain_pending_closes(&self) {
        self.inner.pending_close.borrow_mut().clear();
    }

    /// The descriptor `id` names, borrowed for the callback that resolves it.
    ///
    /// Returns `None` for an id this runtime never issued, which delivery
    /// treats as "drop the event" rather than as a fault — the same rule
    /// output delivery follows for a destroyed output.
    pub(crate) fn with_fd<R>(
        &self,
        id: SourceId,
        f: impl FnOnce(std::os::fd::BorrowedFd<'_>) -> R,
    ) -> Option<R> {
        // The borrow ends before `f` runs: `f` is consumer code, which can
        // call back into this runtime and take the same `RefCell` mutably.
        let raw = {
            let sources = self.inner.sources.borrow();
            sources
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.fd.as_raw_fd())
        }?;
        // SAFETY: `raw` came from an `OwnedFd` this runtime owns. `f` runs
        // from inside a handler (this is only ever called from
        // `backend.rs`'s `on_fd_ready`), so `crate::dispatch::in_handler()`
        // is true for the whole call — including if `f` calls
        // `Runtime::remove_fd` on this very id, the motivating case that
        // method documents. `remove_fd` checks that same flag and defers
        // closing the descriptor rather than dropping it synchronously,
        // specifically so this `OwnedFd` — and so `raw` — stays valid for
        // the whole of `f`'s call, only closing once `run_inner`'s
        // end-of-turn drain runs after `f` has already returned.
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
        Some(f(borrowed))
    }

    /// Create the scene graph and output layout, the renderer and allocator,
    /// and register the protocol globals every client needs before it can
    /// even bind a surface.
    ///
    /// Specifically: `wlr_scene_create`, five `wlr_scene_tree_create` calls
    /// for the stacking bands (see `Graphics::background_band`'s own doc),
    /// `wlr_output_layout_create`, `wlr_scene_attach_output_layout`,
    /// `wlr_renderer_autocreate`, `wlr_allocator_autocreate`,
    /// `wlr_renderer_init_wl_display` (which is what advertises `wl_shm` and
    /// the dmabuf formats), `wlr_compositor_create` at version 6,
    /// `wlr_subcompositor_create` and `wlr_data_device_manager_create`.
    ///
    /// Bundled into one call rather than exposed piecemeal, because there is
    /// no order or subset of them that works: a compositor missing any one of
    /// them is a compositor no client can connect to, and the failure is
    /// silence rather than an error. Version 6 is wl_compositor's current
    /// version in wlroots 0.20 and is fixed here for the same reason — a
    /// version parameter is a knob whose only wrong settings are "lower".
    ///
    /// Call once, after [`Backend::autocreate`](crate::Backend::autocreate)
    /// and before the first `run`; `backend` must be the alive backend that
    /// call returned; a run has not started, so it necessarily still is.
    /// Calling twice returns [`Error::Operation`] rather than leaking a
    /// second renderer.
    ///
    /// A successful call binds `self` to `display`'s lifetime: see
    /// [`Runtime`]'s own doc for the obligation this creates (the output
    /// layout wlroots creates here is freed when `display` is, and this
    /// handle must not be used past that point).
    ///
    /// # Errors
    ///
    /// [`Error::Create`] naming whichever wlroots constructor returned null;
    /// [`Error::Operation`] for a second call, or if
    /// `wlr_renderer_init_wl_display` failed.
    pub fn init_graphics(&self, display: &Display, backend: &Backend<'_>) -> Result<()> {
        if self.inner.graphics.borrow().is_some() {
            return Err(Error::Operation("Runtime::init_graphics called twice"));
        }
        // Frees `scene` (and every band already attached to it) if this
        // function returns early via `?` anywhere after the scene is
        // created. Nothing else does: `Graphics` has no `Drop`, and none of
        // the fallible steps below (the five bands, the output layout, the
        // renderer, the allocator, `wlr_renderer_init_wl_display`, the three
        // protocol globals) undoes an earlier one's work on its own way out
        // — without this, a mid-build failure would leak the scene tree and
        // whichever bands had already been created under it. `disarm`'d
        // just before `Graphics` is assembled, once every fallible step has
        // succeeded and `self` is about to take ownership going forward (see
        // `Graphics`'s own doc on that handoff).
        struct SceneGuard(Option<NonNull<sys::wlr_scene>>);
        impl Drop for SceneGuard {
            fn drop(&mut self) {
                if let Some(scene) = self.0 {
                    // SAFETY: this guard is only ever constructed with a
                    // `scene` pointer this function itself just got back
                    // from a successful `wlr_scene_create`, and is only
                    // still armed (this branch only runs) if `init_graphics`
                    // is unwinding via an early `?` return before handing
                    // `scene` off to `Graphics` — so nothing else has taken
                    // a reference to it, taken ownership of it, or freed it
                    // yet. `wlr_scene_node_destroy` on the scene's own root
                    // node recursively destroys every band already parented
                    // under it (`wlr_scene_tree_create` makes each band a
                    // child of `scene.tree`), which is exactly what needs
                    // undoing here.
                    unsafe {
                        sys::wlr_scene_node_destroy(&raw mut (*scene.as_ptr()).tree.node);
                    }
                }
            }
        }
        // Declared outside the `unsafe` block below so it stays in scope —
        // and so keeps firing its `Drop` — across every early `?` return
        // inside that block, not just the ones textually after it.
        let mut scene_guard = SceneGuard(None);
        // SAFETY: `backend` and `display` are live for this call (the backend
        // is necessarily alive, since no run — the only thing that can start
        // it and let it die — has happened yet), and each pointer is
        // null-checked before use. What happens to each object afterward is
        // not uniform — see `Graphics`'s own doc for the detail — but
        // nothing here is a double free, because nothing created here is
        // ever freed by this crate at all. What this call *does* establish,
        // per `Graphics`'s doc and restated on `Runtime` itself: `self` must
        // not outlive `display` from this point on.
        let graphics = unsafe {
            let scene = sys::wlr_scene_create();
            let scene = NonNull::new(scene).ok_or(Error::Create("wlr_scene_create"))?;
            // Armed the moment the scene exists — every `?` from here to the
            // end of this block must now go through the guard on its way
            // out.
            scene_guard.0 = Some(scene);

            // The five stacking bands, created in bottom-to-top order right
            // after the scene itself and before anything else can be
            // inserted — see `Graphics::background_band`'s own doc for why
            // this order is what fixes the stacking order permanently.
            let background_band = sys::wlr_scene_tree_create(&raw mut (*scene.as_ptr()).tree);
            let background_band = NonNull::new(background_band)
                .ok_or(Error::Create("wlr_scene_tree_create (background band)"))?;
            let bottom_band = sys::wlr_scene_tree_create(&raw mut (*scene.as_ptr()).tree);
            let bottom_band = NonNull::new(bottom_band)
                .ok_or(Error::Create("wlr_scene_tree_create (bottom band)"))?;
            let toplevel_band = sys::wlr_scene_tree_create(&raw mut (*scene.as_ptr()).tree);
            let toplevel_band = NonNull::new(toplevel_band)
                .ok_or(Error::Create("wlr_scene_tree_create (toplevel band)"))?;
            let top_band = sys::wlr_scene_tree_create(&raw mut (*scene.as_ptr()).tree);
            let top_band =
                NonNull::new(top_band).ok_or(Error::Create("wlr_scene_tree_create (top band)"))?;
            let overlay_band = sys::wlr_scene_tree_create(&raw mut (*scene.as_ptr()).tree);
            let overlay_band = NonNull::new(overlay_band)
                .ok_or(Error::Create("wlr_scene_tree_create (overlay band)"))?;

            // A live display, not null: this wlroots build has
            // `wlr_output_layout_create` register a destroy listener on the
            // display it is given (so the layout is torn down with it), and
            // it dereferences that pointer unconditionally — passing null
            // segfaults inside `wl_display_add_destroy_listener` rather than
            // producing a documented failure.
            let layout = sys::wlr_output_layout_create(display.as_ptr());
            let layout = NonNull::new(layout).ok_or(Error::Create("wlr_output_layout_create"))?;

            let scene_layout = sys::wlr_scene_attach_output_layout(scene.as_ptr(), layout.as_ptr());
            let scene_layout = NonNull::new(scene_layout)
                .ok_or(Error::Create("wlr_scene_attach_output_layout"))?;

            let renderer = sys::wlr_renderer_autocreate(backend.as_ptr());
            let renderer =
                NonNull::new(renderer).ok_or(Error::Create("wlr_renderer_autocreate"))?;

            let allocator = sys::wlr_allocator_autocreate(backend.as_ptr(), renderer.as_ptr());
            let allocator =
                NonNull::new(allocator).ok_or(Error::Create("wlr_allocator_autocreate"))?;

            if !sys::wlr_renderer_init_wl_display(renderer.as_ptr(), display.as_ptr()) {
                return Err(Error::Operation("wlr_renderer_init_wl_display"));
            }

            let compositor = sys::wlr_compositor_create(display.as_ptr(), 6, renderer.as_ptr());
            if compositor.is_null() {
                return Err(Error::Create("wlr_compositor_create"));
            }
            if sys::wlr_subcompositor_create(display.as_ptr()).is_null() {
                return Err(Error::Create("wlr_subcompositor_create"));
            }
            if sys::wlr_data_device_manager_create(display.as_ptr()).is_null() {
                return Err(Error::Create("wlr_data_device_manager_create"));
            }

            // Every fallible step above has now succeeded: `self` is about
            // to take ownership of `scene` via `Graphics` below, so the
            // guard must not free it out from under that on the way out of
            // this block.
            scene_guard.0 = None;

            Graphics {
                scene,
                layout,
                scene_layout,
                renderer,
                allocator,
                background_band,
                bottom_band,
                toplevel_band,
                top_band,
                overlay_band,
            }
        };
        // Records the Display this handle is now pinned to (see
        // `RuntimeInner::pinned_display`'s own doc). If a `run_all` call is
        // already driving this thread — unusual (init_graphics is normally
        // called before the first run) but not impossible if a consumer
        // calls it from inside a handler — it must be driving the same
        // `display` this call was just given: `init_graphics` is handed the
        // authoritative `Display` directly, so this is the one call site
        // that can compare against genuinely live ground truth rather than
        // a value another call recorded earlier.
        let display_ptr = display.as_ptr() as usize;
        if let Some(current) = crate::dispatch::current_display() {
            debug_assert_eq!(
                display_ptr, current,
                "Runtime::init_graphics called with a different Display than \
                 the run_all call currently driving this thread"
            );
        }
        self.inner.pinned_display.set(display_ptr);
        *self.inner.graphics.borrow_mut() = Some(graphics);
        Ok(())
    }

    /// Give an announced output a renderer and put it in the scene.
    ///
    /// Call from [`OutputHandler::new_output`](crate::OutputHandler::new_output),
    /// after [`Output::enable_with_preferred_mode`](crate::Output::enable_with_preferred_mode).
    /// Does `wlr_output_init_render`, `wlr_output_layout_add_auto`,
    /// `wlr_scene_output_create` and `wlr_scene_output_layout_add_output` —
    /// after which the output produces `frame` events and
    /// [`commit_output`](Runtime::commit_output) has something to commit.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if [`init_graphics`](Runtime::init_graphics) has
    /// not run, or if `wlr_output_init_render` failed;
    /// [`Error::Create`] if wlroots could not place the output in the layout
    /// or the scene.
    pub fn init_output(&self, output: &Output<'_>) -> Result<()> {
        // Copied out and the borrow dropped before any wlroots call: wlroots
        // can emit a signal from inside these, and a handler reached that way
        // may call back in here.
        let (renderer, allocator, layout, scene, scene_layout) = {
            let g = self.inner.graphics.borrow();
            match g.as_ref() {
                Some(g) => (g.renderer, g.allocator, g.layout, g.scene, g.scene_layout),
                None => {
                    return Err(Error::Operation(
                        "Runtime::init_output before init_graphics",
                    ));
                }
            }
        };

        // SAFETY: the handle's lifetime guarantees the output is live; the
        // renderer and allocator were created by `init_graphics` and are owned
        // by wlroots for the backend's life; the layout, scene and scene
        // layout are this runtime's own and outlive the call.
        unsafe {
            if !sys::wlr_output_init_render(output.as_ptr(), allocator.as_ptr(), renderer.as_ptr())
            {
                return Err(Error::Operation("wlr_output_init_render"));
            }
            let layout_output = sys::wlr_output_layout_add_auto(layout.as_ptr(), output.as_ptr());
            let layout_output =
                NonNull::new(layout_output).ok_or(Error::Create("wlr_output_layout_add_auto"))?;

            let scene_output = sys::wlr_scene_output_create(scene.as_ptr(), output.as_ptr());
            let scene_output =
                NonNull::new(scene_output).ok_or(Error::Create("wlr_scene_output_create"))?;

            sys::wlr_scene_output_layout_add_output(
                scene_layout.as_ptr(),
                layout_output.as_ptr(),
                scene_output.as_ptr(),
            );
        }
        Ok(())
    }

    /// Render and present this output's scene, then tell its surfaces they may
    /// draw again.
    ///
    /// The whole body of an
    /// [`OutputHandler::frame`](crate::OutputHandler::frame) implementation.
    /// Does `wlr_scene_output_commit` and `wlr_scene_output_send_frame_done`
    /// with the current time — the latter because a client that is never
    /// sent frame-done renders exactly one frame and then waits forever.
    ///
    /// Deliberately does **not** call `wlr_output_schedule_frame`. The scene
    /// watches its own damage — a rect moved, resized, recoloured, or a
    /// surface repainted — and reschedules the output itself once there is
    /// something new to draw (`wlr_scene_output_needs_frame`, which
    /// `wlr_scene_output_commit` consults, and the scene's own
    /// `output_needs_frame` listener). Rescheduling unconditionally from
    /// here would defeat that: a compositor whose content never changes
    /// would render forever regardless, which is exactly the "motionless
    /// desktop burns power in a loop" behaviour `wlr_scene_output_commit`'s
    /// own legitimate skip (see `wlr_scene.h`'s `wlr_scene_output_needs_frame`
    /// doc) exists to avoid. A consumer whose output has otherwise gone
    /// idle and needs a one-time kick — most notably right after
    /// [`init_output`](Runtime::init_output), to receive the very first
    /// `frame` — calls
    /// [`Output::schedule_frame`](crate::Output::schedule_frame) themselves;
    /// this call does not do it on their behalf on every commit.
    ///
    /// # Errors
    ///
    /// [`Error::Destroyed`] if this output has no scene output, which means
    /// it was never passed to [`init_output`](Runtime::init_output) (or its
    /// scene output has already gone). [`Error::Operation`] if
    /// [`init_graphics`](Runtime::init_graphics) has not run, or if wlroots
    /// rejected the commit — a genuine failure, not the routine case:
    /// `wlr_scene_output_commit` reports `Ok(())` when it legitimately finds
    /// nothing new to draw and skips the commit (see `wlr_scene.h`'s
    /// `wlr_scene_output_needs_frame` doc), so an `Err` here always means the
    /// commit was attempted and wlroots said no.
    pub fn commit_output(&self, output: &Output<'_>) -> Result<()> {
        // Debug-only bug detector, not a safety mechanism a release build
        // relies on — see `RuntimeInner::pinned_display`'s own doc and
        // `add_rect`'s identical check.
        if let Some(current) = crate::dispatch::current_display() {
            debug_assert_eq!(
                self.inner.pinned_display.get(),
                current,
                "Runtime reused across a different Display"
            );
        }
        let scene = {
            let g = self.inner.graphics.borrow();
            match g.as_ref() {
                Some(g) => g.scene,
                None => {
                    return Err(Error::Operation(
                        "Runtime::commit_output before init_graphics",
                    ));
                }
            }
        };
        // SAFETY: the handle's lifetime guarantees the output is live, and the
        // scene is this runtime's own. `wlr_scene_get_scene_output` returns
        // null for an output that was never added, which is checked.
        unsafe {
            let scene_output = sys::wlr_scene_get_scene_output(scene.as_ptr(), output.as_ptr());
            if scene_output.is_null() {
                return Err(Error::Destroyed("wlr_scene_output"));
            }
            if !sys::wlr_scene_output_commit(scene_output, std::ptr::null()) {
                return Err(Error::Operation("wlr_scene_output_commit"));
            }
            // `SystemTime` rather than `clock_gettime`: `wlr-sys`'s bindings
            // do not bind libc's `clock_gettime`/`CLOCK_MONOTONIC`, and this
            // avoids adding a `libc` dependency for one call. This is wall
            // clock, not monotonic, so unlike a real compositor's timestamp
            // it can step backwards or forwards under an NTP correction — a
            // client that measures the gap between two `frame_done`
            // timestamps spanning such a step sees a bogus duration.
            // Accepted because this crate never consumes this value itself
            // (`frame_done`'s whole job is handing clients a timestamp, not
            // reading one back), so the failure mode is a wrong number in a
            // client's own frame-pacing statistics, not anything this crate
            // can observe or get wrong.
            let now_dur = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let mut now = sys::timespec {
                tv_sec: now_dur.as_secs() as _,
                tv_nsec: now_dur.subsec_nanos() as _,
            };
            sys::wlr_scene_output_send_frame_done(scene_output, &raw mut now);
        }
        Ok(())
    }

    /// The output's box in layout coordinates: `(x, y, width, height)`.
    /// Added in 0.20.10, alongside [`set_output_position`](Runtime::set_output_position).
    ///
    /// `None` on an unknown or stale id — **an [`OutputId`] is only good for
    /// the [`Backend::run_all`](crate::Backend::run_all) call that announced
    /// it**, the same rule [`set_toplevel_size`](Runtime::set_toplevel_size)
    /// documents for [`ToplevelId`] — or on an id this crate does know but
    /// whose output was never placed in the layout: neither
    /// [`init_output`](Runtime::init_output) (auto placement) nor
    /// [`set_output_position`](Runtime::set_output_position) (explicit
    /// placement) has run for it yet, or
    /// [`init_graphics`](Runtime::init_graphics) itself has not run.
    /// `wlr_output_layout_get_box` reports that case as an empty box —
    /// `width == 0 && height == 0` — rather than a null pointer or an error
    /// code, so that is what this checks to turn it into `None`.
    ///
    /// That check is necessarily ambiguous: this cannot tell "never placed"
    /// apart from "placed, but its current mode is itself `0x0`" — both
    /// report the identical empty box, and so both come back `None` here.
    /// An output has no mode at all until its first successful commit, so
    /// calling this before that point (even after a successful
    /// [`init_output`](Runtime::init_output) or
    /// [`set_output_position`](Runtime::set_output_position)) can observe
    /// the same `None` a placement failure would. Callers that need to know
    /// specifically whether *placement* succeeded should trust
    /// [`set_output_position`](Runtime::set_output_position)'s own return
    /// value for that, rather than inferring it from this method, and should
    /// otherwise wait to call this until after the output's first successful
    /// mode commit.
    pub fn output_layout_box(&self, id: OutputId) -> Option<(i32, i32, i32, i32)> {
        let raw = self.output_ptr(id)?;
        // Copied out and the borrow dropped before the wlroots call below,
        // matching `init_output`/`commit_output`: nothing here can re-enter
        // this crate, but holding a `RefCell` borrow across an FFI call this
        // crate does not control is the one habit worth never forming.
        let layout = self.inner.graphics.borrow().as_ref().map(|g| g.layout)?;

        // SAFETY: a present `outputs` entry names an output still linked
        // into that table — removed synchronously, before wlroots frees it,
        // by `forget_output` — so `raw` is live; `layout` is this runtime's
        // own, created by `init_graphics` and never freed by this crate
        // (see [`Graphics`]'s own doc). `wlr_output_layout_get_box` always
        // fully initialises `dest_box`, including the empty-box case, so
        // reading it back after the call is sound regardless of whether
        // `raw` is in the layout.
        let wbox = unsafe {
            let mut wbox = std::mem::MaybeUninit::<sys::wlr_box>::uninit();
            sys::wlr_output_layout_get_box(layout.as_ptr(), raw.as_ptr(), wbox.as_mut_ptr());
            wbox.assume_init()
        };
        if wbox.width == 0 && wbox.height == 0 {
            None
        } else {
            Some((wbox.x, wbox.y, wbox.width, wbox.height))
        }
    }

    /// Pin the output at an explicit layout position, `(x, y)`, removing
    /// auto placement for it — `wlr_output_layout_add`'s own doc: an output
    /// already in the layout "will become manually configured and will be
    /// moved to the specified coordinates".
    ///
    /// `None` on an unknown or stale id (see
    /// [`output_layout_box`](Runtime::output_layout_box)'s own doc for that
    /// rule) or if [`init_graphics`](Runtime::init_graphics) has not run, or
    /// if wlroots itself rejected the placement.
    pub fn set_output_position(&self, id: OutputId, x: i32, y: i32) -> Option<()> {
        let raw = self.output_ptr(id)?;
        let layout = self.inner.graphics.borrow().as_ref().map(|g| g.layout)?;

        // SAFETY: as for `output_layout_box`.
        let layout_output =
            unsafe { sys::wlr_output_layout_add(layout.as_ptr(), raw.as_ptr(), x, y) };
        if layout_output.is_null() {
            None
        } else {
            Some(())
        }
    }

    /// Add a solid-colour rect to the scene, at the root, in RGBA where each
    /// channel is 0.0–1.0 and the colour is premultiplied.
    ///
    /// Positioned at (0, 0) until [`set_rect_position`](Runtime::set_rect_position)
    /// says otherwise, and on top of everything already in the scene — call
    /// [`lower_rect_to_bottom`](Runtime::lower_rect_to_bottom) for a
    /// background. And stays above them: root rects are siblings of the
    /// stacking bands (see [`Layer`](crate::Layer)'s banded-tree doc), so
    /// unlike pre-band versions of this scene, a later toplevel no longer
    /// appends above one — an un-lowered root rect now sits permanently
    /// above every toplevel and every layer surface, `Overlay` included.
    ///
    /// **That also swallows pointer input over the rect's area.** Hit
    /// testing resolves to the topmost node, and a scene rect is not a
    /// surface, so a hit on one yields no surface at all: the seat's pointer
    /// focus is cleared and click-to-focus does not reach whatever is drawn
    /// underneath. For a transient overlay (a drag preview) that is usually
    /// harmless, because the consumer's own drag machine is consuming
    /// motion for the rect's whole lifetime; for a *persistent* translucent
    /// overlay it makes the covered area permanently unclickable, and there
    /// is no way to express "above the toplevels, below `Overlay`" through
    /// this method. [`add_rect_in_band`](Runtime::add_rect_in_band)
    /// (0.20.12) is the fix: a rect parented into a band stacks *with* that
    /// band instead of sitting above every band unconditionally. Until a
    /// consumer moves to it, lower a persistent root rect or keep it
    /// transient.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if wlroots could not create the node, or if
    /// [`init_graphics`](Runtime::init_graphics) has not run yet (there is no
    /// scene to attach the rect to — in that case the payload names this
    /// call rather than a C function, since none ran; match on the variant,
    /// as this doc already tells you to).
    ///
    /// **Known hole, kept for compatibility:** `width`/`height` are not
    /// checked non-negative — see [`set_rect_size`](Runtime::set_rect_size)'s
    /// doc, which documents the identical assert on the sibling C call this
    /// one shares it with (`wlr_scene_rect_create` versus
    /// `wlr_scene_rect_set_size`).
    pub fn add_rect(&self, width: i32, height: i32, color: [f32; 4]) -> Result<RectId> {
        let scene = {
            let g = self.inner.graphics.borrow();
            match g.as_ref() {
                Some(g) => g.scene,
                // Not `wlr_scene_rect_create`: that call never runs without a
                // scene to attach to, and naming it here would claim a call
                // that did not happen. `Error::Create`'s payload is usually a
                // C function name; this is the one exception, and it names
                // the Rust entry point instead.
                None => return Err(Error::Create("Runtime::add_rect before init_graphics")),
            }
        };
        // Debug-only bug detector, not a safety mechanism a release build
        // relies on — see `RuntimeInner::pinned_display`'s own doc. Skipped
        // entirely (not "passes vacuously") when no `run_all` call is on
        // this thread, since there is then nothing live to compare against.
        if let Some(current) = crate::dispatch::current_display() {
            debug_assert_eq!(
                self.inner.pinned_display.get(),
                current,
                "Runtime reused across a different Display"
            );
        }
        // SAFETY: the scene is this runtime's own and outlives the call;
        // `color` is a live four-float array for the duration of the call,
        // which is all `wlr_scene_rect_create` reads (it copies the value).
        let raw = unsafe {
            sys::wlr_scene_rect_create(
                &raw mut (*scene.as_ptr()).tree,
                width,
                height,
                color.as_ptr(),
            )
        };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_scene_rect_create"))?;
        let id = RectId(next_id());
        self.inner.rects.borrow_mut().insert(
            id,
            RectEntry {
                raw,
                parent: RectParent::Root,
            },
        );
        Ok(id)
    }

    /// Add a solid-colour rect parented into `toplevel`'s own scene tree,
    /// in the same premultiplied RGBA [`add_rect`](Runtime::add_rect)
    /// takes. Coordinates given to
    /// [`set_rect_position`](Runtime::set_rect_position) afterward are
    /// relative to the tree's own origin — the same origin
    /// [`set_toplevel_position`](Runtime::set_toplevel_position) moves —
    /// not the scene root's.
    ///
    /// The rect is destroyed automatically when `toplevel` is: wlroots
    /// frees every child of a scene tree recursively when the tree itself
    /// is destroyed, and this crate destroys a toplevel's tree as part of
    /// tearing the toplevel down. Once that happens the returned
    /// [`RectId`] goes stale, the same way a
    /// [`ToplevelId`](crate::ToplevelId) does — every mutator on this page
    /// reports `None` for it from then on, including
    /// [`remove_rect`](Runtime::remove_rect).
    ///
    /// `None` if this runtime has no live toplevel with that id (including
    /// a stale one — see [`set_toplevel_size`](Runtime::set_toplevel_size)'s
    /// doc), or if wlroots could not create the node.
    pub fn add_rect_in_toplevel(
        &self,
        toplevel: ToplevelId,
        width: i32,
        height: i32,
        color: [f32; 4],
    ) -> Option<RectId> {
        let entry = self.toplevel_entry(toplevel)?;
        // SAFETY: a present entry names a live tree (its destroy callback
        // removes the entry before wlroots frees it); `color` is a live
        // four-float array for the duration of the call, which is all
        // `wlr_scene_rect_create` reads (it copies the value).
        let raw = unsafe {
            sys::wlr_scene_rect_create(entry.tree.as_ptr(), width, height, color.as_ptr())
        };
        let raw = NonNull::new(raw)?;
        let id = RectId(next_id());
        self.inner.rects.borrow_mut().insert(
            id,
            RectEntry {
                raw,
                parent: RectParent::Toplevel(toplevel),
            },
        );
        Some(id)
    }

    /// Add a solid-colour rect parented into `band`'s own scene tree, in the
    /// same premultiplied RGBA [`add_rect`](Runtime::add_rect) takes.
    ///
    /// Unlike [`add_rect`](Runtime::add_rect) — which is parented directly
    /// into the scene root, above every band, and so both sits above
    /// everything and swallows pointer input over its area (see that
    /// method's own doc) — a banded rect is a sibling of every
    /// toplevel/layer-surface tree already living in `band`, and so stacks
    /// *with* them: a `Band::Top` rect sits above every toplevel and every
    /// `Background`/`Bottom` layer surface, exactly where a `Top` layer
    /// surface itself would, but still beneath `Band::Overlay`.
    ///
    /// Coordinates given to [`set_rect_position`](Runtime::set_rect_position)
    /// afterward are relative to the band tree's own origin, which for every
    /// band is the scene root's origin (`init_graphics` never positions a
    /// band tree away from `(0, 0)`) — so in practice these coordinates
    /// read the same as [`add_rect`](Runtime::add_rect)'s.
    ///
    /// Removed by [`remove_rect`](Runtime::remove_rect), exactly like a root
    /// rect — **never** by a toplevel dying, even a `Band::Toplevel` rect:
    /// the toplevel band tree itself outlives every toplevel that is ever
    /// parented into it, so a rect parented into the band (a sibling of
    /// each toplevel's own tree) is never a descendant of any one of them.
    ///
    /// # Errors
    ///
    /// [`Error::Create`] if wlroots could not create the node, or if
    /// [`init_graphics`](Runtime::init_graphics) has not run yet — the
    /// identical shape [`add_rect`](Runtime::add_rect)'s own doc describes
    /// for the same case.
    ///
    /// **Known hole, shared with [`add_rect`](Runtime::add_rect):**
    /// `width`/`height` are not checked non-negative — see
    /// [`set_rect_size`](Runtime::set_rect_size)'s doc.
    pub fn add_rect_in_band(
        &self,
        band: Band,
        width: i32,
        height: i32,
        color: [f32; 4],
    ) -> Result<RectId> {
        // Same reasoning as `add_rect`'s identical branch: no
        // `wlr_scene_rect_create` ran, so the payload names this Rust entry
        // point rather than a C function that was never called.
        let tree = self.band_ptr(band).ok_or(Error::Create(
            "Runtime::add_rect_in_band before init_graphics",
        ))?;
        // Debug-only bug detector, not a safety mechanism a release build
        // relies on — see `RuntimeInner::pinned_display`'s own doc and
        // `add_rect`'s identical check just above it in this file.
        if let Some(current) = crate::dispatch::current_display() {
            debug_assert_eq!(
                self.inner.pinned_display.get(),
                current,
                "Runtime reused across a different Display"
            );
        }
        // SAFETY: `tree` names one of the five band trees `init_graphics`
        // created and this runtime owns; it outlives this call. `color` is
        // a live four-float array for the duration of the call, which is
        // all `wlr_scene_rect_create` reads (it copies the value).
        let raw =
            unsafe { sys::wlr_scene_rect_create(tree.as_ptr(), width, height, color.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_scene_rect_create"))?;
        let id = RectId(next_id());
        self.inner.rects.borrow_mut().insert(
            id,
            RectEntry {
                raw,
                parent: RectParent::Band(band),
            },
        );
        Ok(id)
    }

    /// Destroy a rect's scene node — a root rect from
    /// [`add_rect`](Runtime::add_rect), one parented into a toplevel via
    /// [`add_rect_in_toplevel`](Runtime::add_rect_in_toplevel), or one
    /// parented into a band via
    /// [`add_rect_in_band`](Runtime::add_rect_in_band).
    ///
    /// `None` if this runtime never issued `rect`, including a rect already
    /// removed (by this call or by its parent toplevel's own teardown) —
    /// double-removal misses cleanly rather than double-destroying the
    /// node.
    pub fn remove_rect(&self, rect: RectId) -> Option<()> {
        let entry = self.inner.rects.borrow_mut().remove(&rect)?;
        // SAFETY: `entry.raw` came from `add_rect`/`add_rect_in_toplevel`/
        // `add_rect_in_band`, and the table entry naming it is only ever
        // removed once — by this call, or (without a matching destroy; see
        // their own comments) by `forget_toplevel`'s per-toplevel purge or
        // `clear_toplevels`' run-granularity purge (neither of which ever
        // touches a band rect's entry — see `RectParent::Band`'s own doc) —
        // so the node has not been destroyed yet.
        unsafe { sys::wlr_scene_node_destroy(&raw mut (*entry.raw.as_ptr()).node) };
        Some(())
    }

    /// Move a rect. `None` if this runtime never issued `rect`.
    pub fn set_rect_position(&self, rect: RectId, x: i32, y: i32) -> Option<()> {
        let raw = self.rect_ptr(rect)?;
        // SAFETY: `rect_ptr` resolving `rect` means its `RectEntry` is
        // still in the table — the *only* three places that remove a row
        // (`remove_rect`, `forget_toplevel`'s per-toplevel purge, and
        // `clear_toplevels`' run-granularity purge) all drop the row before
        // or at the moment they establish the node is going away, and none
        // of them destroy a node while leaving its row behind. So a
        // resolvable id names a node wlroots has not yet destroyed,
        // regardless of whether it came from `add_rect` or
        // `add_rect_in_toplevel`.
        unsafe { sys::wlr_scene_node_set_position(&raw mut (*raw.as_ptr()).node, x, y) };
        Some(())
    }

    /// Resize a rect. `None` if this runtime never issued `rect`.
    ///
    /// **Known hole, kept for compatibility:** unlike
    /// [`set_buffer_dest_size`](Runtime::set_buffer_dest_size), this does
    /// **not** guard against a negative `width`/`height` — this signature
    /// was already published (0.20.1) before that asymmetry was noticed, and
    /// adding the guard now would silently change already-published
    /// behaviour (an abort becoming a `None`) rather than a memory-safety
    /// fix, so it is documented instead. `wlr_scene_rect_set_size` asserts
    /// both are non-negative; passing a negative one aborts the process.
    pub fn set_rect_size(&self, rect: RectId, width: i32, height: i32) -> Option<()> {
        let raw = self.rect_ptr(rect)?;
        // SAFETY: as for `set_rect_position` — see that call's own comment
        // for the current (post-`remove_rect`) argument.
        unsafe { sys::wlr_scene_rect_set_size(raw.as_ptr(), width, height) };
        Some(())
    }

    /// Recolour a rect, in the same premultiplied RGBA
    /// [`add_rect`](Runtime::add_rect) takes. `None` if this runtime never
    /// issued `rect`.
    pub fn set_rect_color(&self, rect: RectId, color: [f32; 4]) -> Option<()> {
        let raw = self.rect_ptr(rect)?;
        // SAFETY: as for `set_rect_position`; `color` is additionally live
        // for the call and wlroots copies it rather than retaining the
        // pointer.
        unsafe { sys::wlr_scene_rect_set_color(raw.as_ptr(), color.as_ptr()) };
        Some(())
    }

    /// Put a rect behind everything else in the scene. `None` if this runtime
    /// never issued `rect`.
    pub fn lower_rect_to_bottom(&self, rect: RectId) -> Option<()> {
        let raw = self.rect_ptr(rect)?;
        // SAFETY: as for `set_rect_position`.
        unsafe { sys::wlr_scene_node_lower_to_bottom(&raw mut (*raw.as_ptr()).node) };
        Some(())
    }

    /// The rect `id` names, with the table borrow released before returning —
    /// every caller then re-enters wlroots, which can emit a signal, which can
    /// take this same `RefCell` mutably.
    fn rect_ptr(&self, id: RectId) -> Option<NonNull<sys::wlr_scene_rect>> {
        self.inner.rects.borrow().get(&id).map(|e| e.raw)
    }

    /// Test-only: whether `rect`'s own node is currently parented directly
    /// under `band`'s tree. `false` for an unknown `rect`, mirroring every
    /// other by-id query in this crate rather than panicking.
    ///
    /// `#[cfg(test)]` rather than exported: this is the same "read
    /// `node.parent` back" assertion
    /// `reparent_layer_surface_if_changed_moves_the_tree_only_when_the_layer_changed`
    /// makes for a layer surface's tree, applied to a rect's node instead,
    /// and every caller of it lives in this module's own `mod tests` (which
    /// can already reach private fields directly — this exists purely to
    /// give `add_rect_in_band`'s own test a name to call through `Runtime`,
    /// the same surface its assertion is really about).
    #[cfg(test)]
    fn rect_is_in_band(&self, rect: RectId, band: NonNull<sys::wlr_scene_tree>) -> bool {
        let Some(raw) = self.rect_ptr(rect) else {
            return false;
        };
        // SAFETY: `rect_ptr` resolving `rect` means its node has not been
        // destroyed yet — the same argument `set_rect_position`'s own
        // comment makes for reading a resolvable rect's node.
        unsafe { (*raw.as_ptr()).node.parent == band.as_ptr() }
    }

    /// Add a scene node showing owned RGBA8888 pixels (bytes R, G, B, A per
    /// pixel, row-major, stride = `width * 4`), at the root, at (0, 0) until
    /// [`set_buffer_position`](Runtime::set_buffer_position) says otherwise
    /// and on top of everything already in the scene — call
    /// [`lower_buffer_to_bottom`](Runtime::lower_buffer_to_bottom) for a
    /// background. And stays above them: root buffers are siblings of the
    /// stacking bands (see [`Layer`](crate::Layer)'s banded-tree doc), so
    /// unlike pre-band versions of this scene, a later toplevel no longer
    /// appends above one — an un-lowered root buffer now sits permanently
    /// above every toplevel and every layer surface, `Overlay` included,
    /// and swallows pointer input over its area exactly as an un-lowered
    /// root rect does. See [`add_rect`](Runtime::add_rect)'s own paragraph
    /// on that (and on the planned additive `add_rect_in_band` fix); the
    /// mechanism and the consequence are identical for buffers.
    ///
    /// Pixels are copied: `rgba` need not outlive this call.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if `rgba.len() != (width * height * 4) as usize`,
    /// either dimension is less than 1, or `width` is large enough that
    /// `width * 4` would overflow `i32` (`> i32::MAX / 4`, around 512M
    /// pixels wide) — all checked before anything is allocated or handed to
    /// wlroots. [`Error::Create`] if
    /// [`init_graphics`](Runtime::init_graphics) has not run yet (mirroring
    /// [`add_rect`](Runtime::add_rect); see that method's own doc for why
    /// this names the Rust entry point rather than a C function), or if
    /// wlroots could not create the node.
    pub fn add_buffer(&self, width: i32, height: i32, rgba: &[u8]) -> Result<BufferId> {
        if !crate::buffer::validate_pixels(width, height, rgba.len()) {
            return Err(Error::Operation("pixel buffer dimensions or length"));
        }
        let scene = {
            let g = self.inner.graphics.borrow();
            match g.as_ref() {
                Some(g) => g.scene,
                None => return Err(Error::Create("Runtime::add_buffer before init_graphics")),
            }
        };
        let buf = create_pixel_buffer(width, height, rgba);
        // SAFETY: the scene is this runtime's own and outlives the call;
        // `buf` is a freshly leaked, fully initialised `wlr_buffer` from
        // `create_pixel_buffer`, at `n_locks == 0` and not yet dropped. On
        // success `wlr_scene_buffer_create` takes its own consumer lock on
        // `buf` before this returns; on failure it never does. Either way
        // `wlr_buffer_drop` below is the correct, unconditional next call —
        // see `buffer.rs`'s own "Refcount story" doc for the full argument,
        // including why this never leaks or double-frees on the failure
        // path.
        let node = unsafe { sys::wlr_scene_buffer_create(&raw mut (*scene.as_ptr()).tree, buf) };
        // SAFETY: as above — releases this call's own producer reference,
        // regardless of whether `wlr_scene_buffer_create` succeeded.
        unsafe { sys::wlr_buffer_drop(buf) };
        let node = NonNull::new(node).ok_or(Error::Create("wlr_scene_buffer_create"))?;
        let id = BufferId(next_id());
        self.inner
            .buffers
            .borrow_mut()
            .insert(id, BufferEntry { node, parent: None });
        Ok(id)
    }

    /// Add a pixel buffer parented into `toplevel`'s own scene tree, in the
    /// same RGBA shape [`add_buffer`](Runtime::add_buffer) takes.
    /// Coordinates given to
    /// [`set_buffer_position`](Runtime::set_buffer_position) afterward are
    /// relative to the tree's own origin, not the scene root's.
    ///
    /// The buffer node is destroyed automatically when `toplevel` is, the
    /// same way [`add_rect_in_toplevel`](Runtime::add_rect_in_toplevel)'s
    /// rects are — see that method's own doc for the detail. Once that
    /// happens the returned [`BufferId`] goes stale, and every mutator on
    /// this page reports `None` for it from then on.
    ///
    /// `None` if this runtime has no live toplevel with that id (including a
    /// stale one), or on any of [`add_buffer`](Runtime::add_buffer)'s error
    /// conditions (wrong pixel length, a non-positive or overflow-prone
    /// dimension, or wlroots refusing to create the node).
    pub fn add_buffer_in_toplevel(
        &self,
        toplevel: ToplevelId,
        width: i32,
        height: i32,
        rgba: &[u8],
    ) -> Option<BufferId> {
        // Resolve `toplevel` before validating pixels: a stale/unknown id is
        // the routine, expected `None` this method's own doc promises, but a
        // caller passing bad dimensions or a mismatched `rgba` length against
        // a *live* toplevel is a caller bug. Validating first would collapse
        // both into the same silent `None`, hiding the caller bug behind the
        // benign one; resolving the id first keeps the two apart — the
        // `debug_assert!` below is what actually surfaces the caller-bug
        // case in a debug build (mirrors `create_pixel_buffer`'s own
        // precedent).
        let entry = self.toplevel_entry(toplevel)?;
        debug_assert!(
            crate::buffer::validate_pixels(width, height, rgba.len()),
            "add_buffer_in_toplevel called with invalid pixel dimensions or length"
        );
        if !crate::buffer::validate_pixels(width, height, rgba.len()) {
            return None;
        }
        let buf = create_pixel_buffer(width, height, rgba);
        // SAFETY: a present entry names a live tree (its destroy callback
        // removes the entry before wlroots frees it); the refcount argument
        // for the `wlr_buffer_drop` pairing is identical to `add_buffer`'s —
        // see that method's own comment and `buffer.rs`'s module doc.
        let node = unsafe { sys::wlr_scene_buffer_create(entry.tree.as_ptr(), buf) };
        // SAFETY: as for `add_buffer`.
        unsafe { sys::wlr_buffer_drop(buf) };
        let node = NonNull::new(node)?;
        let id = BufferId(next_id());
        self.inner.buffers.borrow_mut().insert(
            id,
            BufferEntry {
                node,
                parent: Some(toplevel),
            },
        );
        Some(id)
    }

    /// Replace a buffer node's pixels (and intrinsic size) with a fresh
    /// copy, in the same RGBA shape [`add_buffer`](Runtime::add_buffer)
    /// takes.
    ///
    /// `None` if this runtime never issued `buffer`, or on
    /// [`add_buffer`](Runtime::add_buffer)'s validation error conditions
    /// (wrong pixel length, or a non-positive or overflow-prone dimension).
    ///
    /// Safe to call from a handler at any point, including while a frame is
    /// mid-render: the swap this performs (`wlr_scene_buffer_set_buffer`,
    /// below) can never land inside a renderer's own
    /// `begin_data_ptr_access`/`end_data_ptr_access` bracket on the *old*
    /// buffer, because this crate's whole event loop is single-threaded and
    /// no renderer call that opens such a bracket re-enters handler code —
    /// see `buffer.rs`'s module doc for the fuller argument. If either of
    /// those two facts ever stopped holding (a second thread, or a render
    /// callback that calls back into a handler), this call could race a
    /// renderer reading through the pointer `pixel_begin_data_ptr_access`
    /// handed out for the buffer being replaced.
    pub fn update_buffer(
        &self,
        buffer: BufferId,
        width: i32,
        height: i32,
        rgba: &[u8],
    ) -> Option<()> {
        // Resolve `buffer` before validating pixels — same reasoning as
        // `add_buffer_in_toplevel`'s identical reordering: a stale id must
        // stay a silent `None`, but bad dimensions against a *live* buffer
        // is a caller bug the `debug_assert!` below surfaces in a debug
        // build instead of silently folding into the same `None`.
        let node = self.buffer_ptr(buffer)?;
        debug_assert!(
            crate::buffer::validate_pixels(width, height, rgba.len()),
            "update_buffer called with invalid pixel dimensions or length"
        );
        if !crate::buffer::validate_pixels(width, height, rgba.len()) {
            return None;
        }
        let buf = create_pixel_buffer(width, height, rgba);
        // SAFETY: `buffer_ptr` resolving `buffer` means its `BufferEntry` is
        // still in the table, so `node` names a node wlroots has not yet
        // destroyed (see `buffer_ptr`'s own doc for the fuller argument,
        // mirroring `rect_ptr`'s). `wlr_scene_buffer_set_buffer` locks `buf`
        // (its own new consumer reference) and unlocks whatever buffer the
        // node held before, exactly mirroring the create-time handoff —
        // see `buffer.rs`'s module doc. That unlock cannot race a renderer
        // still reading the old buffer's data pointer — see this method's
        // own doc above.
        unsafe { sys::wlr_scene_buffer_set_buffer(node.as_ptr(), buf) };
        // SAFETY: as above — releases this call's own producer reference.
        unsafe { sys::wlr_buffer_drop(buf) };
        Some(())
    }

    /// Move a buffer node. `None` if this runtime never issued `buffer`.
    pub fn set_buffer_position(&self, buffer: BufferId, x: i32, y: i32) -> Option<()> {
        let node = self.buffer_ptr(buffer)?;
        // SAFETY: as for `update_buffer`.
        unsafe { sys::wlr_scene_node_set_position(&raw mut (*node.as_ptr()).node, x, y) };
        Some(())
    }

    /// Scale a buffer node's on-screen size independently of its pixel
    /// size. `None` if this runtime never issued `buffer`, **or if either
    /// `width` or `height` is negative** — `wlr_scene_buffer_set_dest_size`
    /// asserts both are non-negative, and an assert failure aborts the
    /// process, so this is checked first rather than handed through. (Zero
    /// is accepted and passed on: wlroots documents zero as "use the
    /// buffer's own size", not as an error.)
    ///
    /// [`set_rect_size`](Runtime::set_rect_size) has the identical C-side
    /// assert on `wlr_scene_rect_set_size` and is **not** guarded — that
    /// method was already published before this one existed, so adding the
    /// guard there now would be an observable behaviour change to a frozen
    /// signature. Noted here rather than silently left inconsistent.
    pub fn set_buffer_dest_size(&self, buffer: BufferId, width: i32, height: i32) -> Option<()> {
        if width < 0 || height < 0 {
            return None;
        }
        let node = self.buffer_ptr(buffer)?;
        // SAFETY: as for `update_buffer`. `width`/`height` are checked
        // non-negative just above, which is `wlr_scene_buffer_set_dest_size`'s
        // own precondition (violating it is an assert-abort in wlroots, not
        // memory-unsafety, but this crate's own "panic-free public fn" rule
        // treats an abort the same way a panic would be).
        unsafe { sys::wlr_scene_buffer_set_dest_size(node.as_ptr(), width, height) };
        Some(())
    }

    /// Put a buffer node behind everything else in the scene. `None` if
    /// this runtime never issued `buffer`.
    pub fn lower_buffer_to_bottom(&self, buffer: BufferId) -> Option<()> {
        let node = self.buffer_ptr(buffer)?;
        // SAFETY: as for `update_buffer`.
        unsafe { sys::wlr_scene_node_lower_to_bottom(&raw mut (*node.as_ptr()).node) };
        Some(())
    }

    /// Destroy a buffer node's scene node, whether it is a root buffer from
    /// [`add_buffer`](Runtime::add_buffer) or one parented into a toplevel
    /// via [`add_buffer_in_toplevel`](Runtime::add_buffer_in_toplevel).
    ///
    /// `None` if this runtime never issued `buffer`, including one already
    /// removed (by this call or by its parent toplevel's own teardown) —
    /// double-removal misses cleanly rather than double-destroying the
    /// node.
    pub fn remove_buffer(&self, buffer: BufferId) -> Option<()> {
        let entry = self.inner.buffers.borrow_mut().remove(&buffer)?;
        // SAFETY: `entry.node` came from `add_buffer`/`add_buffer_in_toplevel`
        // and the table entry naming it is only ever removed once — by this
        // call, or (without a matching destroy; see their own comments) by
        // `forget_toplevel`'s per-toplevel purge or `clear_toplevels`'
        // run-granularity purge — so the node has not been destroyed yet.
        // Destroying the node unlocks its buffer, which — since this
        // module's own handoff already dropped the producer's reference —
        // is what finally frees it (see `buffer.rs`'s module doc).
        unsafe { sys::wlr_scene_node_destroy(&raw mut (*entry.node.as_ptr()).node) };
        Some(())
    }

    /// The buffer node `id` names, with the table borrow released before
    /// returning — every caller then re-enters wlroots, which can emit a
    /// signal, which can take this same `RefCell` mutably.
    fn buffer_ptr(&self, id: BufferId) -> Option<NonNull<sys::wlr_scene_buffer>> {
        self.inner.buffers.borrow().get(&id).map(|e| e.node)
    }

    /// Advertise `xdg_wm_base` at `version`.
    ///
    /// **Call [`init_graphics`](Runtime::init_graphics) first.** A shell
    /// created before graphics exist would leave every toplevel a client
    /// creates with nowhere in the scene to go — `on_new_toplevel` cannot
    /// answer with a configure, and a client that is never configured never
    /// maps, so the visible symptom is a hung client rather than an error.
    /// This is checked and refused below rather than left as a silent trap.
    ///
    /// Registration of the `new_toplevel` listener happens inside
    /// [`Backend::run_all`](crate::Backend::run_all) and lives for that call,
    /// so creating the shell after a run has started has no effect until the
    /// next one — the same rule fd sources follow, and for the same reason.
    ///
    /// `version` is a parameter rather than fixed because a compositor
    /// deliberately advertising an older xdg-shell (to work around a client)
    /// is a real thing to want; pass 6 unless you know otherwise.
    ///
    /// # One `Runtime` per `Display`, per process
    ///
    /// **The pointer cached by this call belongs to `display` and is never
    /// taken back.** The shell object is owned by the `Display` and dies
    /// with it, while the `Runtime` goes on holding the raw pointer — and
    /// the "called twice" guard below refuses to re-create against a
    /// replacement `Display`, so there is no way to refresh it. A consumer
    /// that drops its `Display`, builds a new one, and reuses the same
    /// `Runtime` will therefore have `Backend::run_all` link its listeners
    /// into freed `wl_list`s: a use-after-free with no recovery path. The
    /// same is true of the pointers cached by
    /// [`create_xdg_decoration_manager`](Runtime::create_xdg_decoration_manager)
    /// and [`create_layer_shell`](Runtime::create_layer_shell).
    ///
    /// Build one `Runtime` per `Display`, and (because the graphics and
    /// backend state hanging off a `Runtime` is process-global in wlroots)
    /// one `Display` per process. This cannot be enforced by signature
    /// without a breaking change, so 0.20.x states it rather than checks it.
    /// A debug-only detector does back the invariant, though it cannot make
    /// the type system carry it:
    /// [`init_graphics`](Runtime::init_graphics) records (pins) the `Display`
    /// it is given, and both [`Backend::run_all`](crate::Backend::run_all) —
    /// at entry, before it links the listeners this paragraph warns about —
    /// and the graphics mutators [`add_rect`](Runtime::add_rect),
    /// [`add_rect_in_band`](Runtime::add_rect_in_band) and
    /// [`commit_output`](Runtime::commit_output) `debug_assert` that the
    /// `Display` currently in play matches the pinned one. Reusing a
    /// `Runtime` against a replacement `Display` therefore trips a debug
    /// assertion at the next `run_all` or graphics call rather than silently
    /// linking into freed `wl_list`s. Its limits: it is compiled out of
    /// release builds, and it is a bug detector, not a recovery mechanism —
    /// see `RuntimeInner::pinned_display`'s own (crate-internal) doc for
    /// exactly which sites are covered and why per-setter duplication past
    /// those choke points would be redundant.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if a shell already exists on this runtime, or if
    /// [`init_graphics`](Runtime::init_graphics) has not run yet;
    /// [`Error::Create`] if wlroots could not create the shell.
    pub fn create_xdg_shell(&self, display: &Display, version: u32) -> Result<()> {
        if self.inner.xdg_shell.borrow().is_some() {
            return Err(Error::Operation("Runtime::create_xdg_shell called twice"));
        }
        if self.inner.graphics.borrow().is_none() {
            return Err(Error::Operation(
                "Runtime::create_xdg_shell before init_graphics",
            ));
        }
        // SAFETY: `display` is live for the call; the returned shell is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_xdg_shell_create(display.as_ptr(), version) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_xdg_shell_create"))?;
        // Cached for the lifetime of this `Runtime`, but only valid for the
        // lifetime of *this* `display` — see the "One `Runtime` per
        // `Display`" section above. Nothing here can tell a second call
        // apart by display, which is why the guard above rejects it
        // outright rather than refreshing.
        *self.inner.xdg_shell.borrow_mut() = Some(raw);
        Ok(())
    }

    pub(crate) fn xdg_shell_ptr(&self) -> Option<NonNull<sys::wlr_xdg_shell>> {
        *self.inner.xdg_shell.borrow()
    }

    /// Advertise `zxdg_decoration_manager_v1`.
    ///
    /// Call once, after [`create_xdg_shell`](Runtime::create_xdg_shell) —
    /// decorations are negotiated per toplevel, so a decoration manager with
    /// no shell to hand out toplevels has nothing to attach to. Not checked
    /// here (unlike `create_xdg_shell`'s own `init_graphics` prerequisite):
    /// `wlr_xdg_decoration_manager_v1_create` does not itself require a
    /// shell to exist yet, only that a client's later
    /// `get_toplevel_decoration` name a real toplevel, which is enforced by
    /// wlroots at that point, not by this call.
    ///
    /// Registration of the `new_toplevel_decoration` listener happens inside
    /// [`Backend::run_all`](crate::Backend::run_all) and lives for that
    /// call, so creating the manager after a run has started has no effect
    /// until the next one — the same rule [`create_xdg_shell`](Runtime::create_xdg_shell)
    /// follows.
    ///
    /// The pointer cached here is tied to `display` for good: see
    /// [`create_xdg_shell`](Runtime::create_xdg_shell)'s *One `Runtime` per
    /// `Display`* section, which applies verbatim to this manager.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if a manager already exists on this runtime;
    /// [`Error::Create`] if wlroots could not create it.
    pub fn create_xdg_decoration_manager(&self, display: &Display) -> Result<()> {
        if self.inner.xdg_decoration_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_xdg_decoration_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is
        // owned by the display and destroyed with it, so this crate never
        // frees it.
        let raw = unsafe { sys::wlr_xdg_decoration_manager_v1_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_xdg_decoration_manager_v1_create"))?;
        *self.inner.xdg_decoration_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    pub(crate) fn xdg_decoration_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_xdg_decoration_manager_v1>> {
        *self.inner.xdg_decoration_manager.borrow()
    }

    /// Advertise `zwp_primary_selection_device_manager_v1` (middle-click
    /// paste). The seat's `request_set_primary_selection` event is wired to
    /// honor it in `backend.rs`'s per-run registration. Errors if called
    /// twice — a second call would advertise a second global.
    pub fn create_primary_selection_manager(&self, display: &Display) -> Result<()> {
        if self.inner.primary_selection_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_primary_selection_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is
        // owned by the display and destroyed with it, so this crate never
        // frees it.
        let raw = unsafe { sys::wlr_primary_selection_v1_device_manager_create(display.as_ptr()) };
        let raw = NonNull::new(raw)
            .ok_or(Error::Create("wlr_primary_selection_v1_device_manager_create"))?;
        *self.inner.primary_selection_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Advertise `zwlr_data_control_manager_v1` so a clipboard manager can
    /// observe and set the selection. wlroots wires it to the seat's
    /// selection automatically; no per-run listener of ours is required.
    /// Errors if called twice.
    pub fn create_data_control_manager(&self, display: &Display) -> Result<()> {
        if self.inner.data_control_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_data_control_manager called twice",
            ));
        }
        // SAFETY: as above — display-owned, never freed by this crate.
        let raw = unsafe { sys::wlr_data_control_manager_v1_create(display.as_ptr()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_data_control_manager_v1_create"))?;
        *self.inner.data_control_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    /// Advertise `zwp_virtual_keyboard_manager_v1` so a client can inject a
    /// keyboard input device. The manager's `new_virtual_keyboard` event is
    /// wired in `backend.rs`'s per-run registration to attach the injected
    /// keyboard to the seat (so the seat gains keyboard capability and its
    /// key/enter events mint serials, exactly as a physical keyboard would).
    /// Errors if called twice.
    pub fn create_virtual_keyboard_manager(&self, display: &Display) -> Result<()> {
        if self.inner.virtual_keyboard_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_virtual_keyboard_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_virtual_keyboard_manager_v1_create(display.as_ptr()) };
        let raw =
            NonNull::new(raw).ok_or(Error::Create("wlr_virtual_keyboard_manager_v1_create"))?;
        *self.inner.virtual_keyboard_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    pub(crate) fn virtual_keyboard_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_virtual_keyboard_manager_v1>> {
        *self.inner.virtual_keyboard_manager.borrow()
    }

    /// Advertise `zwlr_virtual_pointer_manager_v1` so a client can inject a
    /// pointer input device. The manager's `new_virtual_pointer` event is
    /// wired in `backend.rs`'s per-run registration to attach the injected
    /// pointer to the seat (so the seat gains pointer capability and its
    /// motion/button events mint serials, exactly as a physical pointer
    /// would). Errors if called twice.
    pub fn create_virtual_pointer_manager(&self, display: &Display) -> Result<()> {
        if self.inner.virtual_pointer_manager.borrow().is_some() {
            return Err(Error::Operation(
                "Runtime::create_virtual_pointer_manager called twice",
            ));
        }
        // SAFETY: `display` is live for the call; the returned manager is owned
        // by the display and destroyed with it, so this crate never frees it.
        let raw = unsafe { sys::wlr_virtual_pointer_manager_v1_create(display.as_ptr()) };
        let raw =
            NonNull::new(raw).ok_or(Error::Create("wlr_virtual_pointer_manager_v1_create"))?;
        *self.inner.virtual_pointer_manager.borrow_mut() = Some(raw);
        Ok(())
    }

    pub(crate) fn virtual_pointer_manager_ptr(
        &self,
    ) -> Option<NonNull<sys::wlr_virtual_pointer_manager_v1>> {
        *self.inner.virtual_pointer_manager.borrow()
    }

    /// The scene's root tree, for the callbacks in `backend.rs` that insert a
    /// toplevel into it.
    ///
    /// `None` if [`init_graphics`](Runtime::init_graphics) has not run.
    /// Nothing stops a consumer calling
    /// [`create_xdg_shell`](Runtime::create_xdg_shell) without ever calling
    /// `init_graphics` — the two are independent — and `on_new_toplevel` runs
    /// underneath an `extern "C"` frame reached the moment a client connects
    /// and creates a surface, which a consumer's own mistake does not
    /// prevent. So this reports absence rather than panicking, and the
    /// caller drops the announcement instead of aborting the process for it.
    pub(crate) fn scene_ptr(&self) -> Option<NonNull<sys::wlr_scene>> {
        self.inner.graphics.borrow().as_ref().map(|g| g.scene)
    }

    /// The scene tree every toplevel's own tree is parented into — see
    /// `Graphics::background_band`'s own doc for the full argument.
    /// `None` before [`init_graphics`](Runtime::init_graphics) has run.
    pub(crate) fn toplevel_band_ptr(&self) -> Option<NonNull<sys::wlr_scene_tree>> {
        self.inner
            .graphics
            .borrow()
            .as_ref()
            .map(|g| g.toplevel_band)
    }

    /// The scene tree `layer`'s own band — `background`/`bottom`/`top`/
    /// `overlay` — a layer surface belongs under. `None` before
    /// [`init_graphics`](Runtime::init_graphics) has run.
    pub(crate) fn layer_band_ptr(&self, layer: Layer) -> Option<NonNull<sys::wlr_scene_tree>> {
        let g = self.inner.graphics.borrow();
        let g = g.as_ref()?;
        Some(match layer {
            Layer::Background => g.background_band,
            Layer::Bottom => g.bottom_band,
            Layer::Top => g.top_band,
            Layer::Overlay => g.overlay_band,
        })
    }

    /// The scene tree `band` names — any of the five bands, including
    /// [`Band::Toplevel`], which [`layer_band_ptr`](Runtime::layer_band_ptr)
    /// cannot express since [`Layer`] has no toplevel variant. `None`
    /// before [`init_graphics`](Runtime::init_graphics) has run.
    pub(crate) fn band_ptr(&self, band: Band) -> Option<NonNull<sys::wlr_scene_tree>> {
        self.inner
            .graphics
            .borrow()
            .as_ref()
            .map(|g| g.band_tree(band))
    }

    /// Record a newly-announced toplevel under `id`, in both the id table and
    /// the tree-to-id reverse lookup.
    pub(crate) fn record_toplevel(
        &self,
        id: ToplevelId,
        raw: NonNull<sys::wlr_xdg_toplevel>,
        tree: NonNull<sys::wlr_scene_tree>,
    ) {
        self.inner
            .toplevels
            .borrow_mut()
            .insert(id, ToplevelEntry { raw, tree });
        self.inner
            .tree_to_toplevel
            .borrow_mut()
            .insert(tree.as_ptr() as usize, id);
    }

    /// Remove `id` from both tables. Called from `on_toplevel_destroy` before
    /// the toplevel is freed.
    pub(crate) fn forget_toplevel(&self, id: ToplevelId) {
        // Every rect `add_rect_in_toplevel` parented into this toplevel's
        // tree dies with that tree: wlroots destroys a scene tree's
        // children recursively when the tree itself is destroyed, which is
        // about to happen (this runs from `on_toplevel_destroy`, before
        // wlroots frees the toplevel and its tree). So these entries are
        // dropped from the table here, without calling
        // `wlr_scene_node_destroy` on any of them — the node is destroyed
        // once, by wlroots, as part of destroying the parent tree; calling
        // it again here would be a double free of memory wlroots has
        // already reclaimed.
        self.inner
            .rects
            .borrow_mut()
            .retain(|_, entry| entry.parent != RectParent::Toplevel(id));
        // Same reasoning, for buffer nodes `add_buffer_in_toplevel` parented
        // into this toplevel's tree — see `BufferEntry`'s own doc.
        self.inner
            .buffers
            .borrow_mut()
            .retain(|_, entry| entry.parent != Some(id));

        let entry = self.inner.toplevels.borrow_mut().remove(&id);
        if let Some(entry) = entry {
            self.inner
                .tree_to_toplevel
                .borrow_mut()
                .remove(&(entry.tree.as_ptr() as usize));
        }

        // The toplevel dying first is one of the two orders a decoration and
        // its toplevel can die in (see `RuntimeInner::decorations`'s own
        // doc); this is the half of that purge owned by the toplevel side.
        // wlroots does free the decoration in this order too — it posts the
        // `orphaned` protocol error and destroys the resource — but only
        // *after* this callback returns and wlroots' own internal
        // `toplevel_destroy` listener on the decoration runs, which is
        // registered after this crate's own (`get_toplevel_decoration` runs
        // after `on_new_toplevel`, and `wl_signal_add` appends), so this
        // crate's purge — of both this table and, at the call site, the
        // session's decoration registrations — is guaranteed to run first.
        // That ordering is what this table entry existing at all depends
        // on: it must be gone before wlroots frees the decoration, not
        // merely before this function returns.
        self.inner.decorations.borrow_mut().remove(&id);
    }

    /// The entry `id` names, with the borrow released before returning — the
    /// caller then re-enters wlroots, which can emit a signal, which can take
    /// this same `RefCell` mutably.
    pub(crate) fn toplevel_entry(&self, id: ToplevelId) -> Option<ToplevelEntry> {
        self.inner.toplevels.borrow().get(&id).copied()
    }

    /// Drop every toplevel this runtime knows of, without touching wlroots.
    ///
    /// Called once, by `backend.rs`'s `run_inner`, when the `run_all` call
    /// that populated these tables returns — on every exit path, including
    /// an early `?` return or a panic (see that call site's own doc for why).
    /// Toplevel ids are only meaningful for the call that announced them:
    /// the per-toplevel destroy listener that would otherwise remove a stale
    /// entry is itself torn down with that call's `Session`, the same rule
    /// already documented for outputs ("outputs announced during one `run`
    /// are not re-announced by the next one"). Without this, a consumer who
    /// kept a `Runtime` clone and drove `EventLoop::dispatch` after `run_all`
    /// returned could see a client destroy its toplevel, then call a by-id
    /// mutator that resolves the now-stale entry and dereferences memory
    /// wlroots already freed.
    ///
    /// Also purges every rect [`Runtime::add_rect_in_toplevel`] parented
    /// into one of these toplevels — mirroring [`forget_toplevel`]'s own
    /// purge, and for the identical reason, just at run granularity instead
    /// of per-toplevel: a `RectId` is only as good as the `ToplevelId` it
    /// was parented under, and that id is going stale in the very next
    /// statement. Without this, a `RectEntry` whose `parent` names a
    /// toplevel this call is about to forget would outlive the run with no
    /// listener left to purge it later — `forget_toplevel`'s own purge only
    /// ever runs while that toplevel's destroy listener is still installed,
    /// which ends here — so if that toplevel's tree is freed afterward
    /// (wlroots frees a toplevel's tree, and every child node in it,
    /// whenever the toplevel itself is eventually destroyed, in this run or
    /// a later one), the row would keep naming a node wlroots has already
    /// destroyed, and [`Runtime::remove_rect`] would call
    /// `wlr_scene_node_destroy` on it a second time.
    pub(crate) fn clear_toplevels(&self) {
        self.inner
            .rects
            .borrow_mut()
            .retain(|_, entry| !matches!(entry.parent, RectParent::Toplevel(_)));
        // Mirrors the rect purge just above, for buffer entries — the
        // identical hazard `RectEntry` closes applies verbatim to
        // `BufferEntry`: a `BufferId` is only as good as the `ToplevelId`
        // it was parented under, and that id is going stale in the very
        // next statement.
        self.inner
            .buffers
            .borrow_mut()
            .retain(|_, entry| entry.parent.is_none());
        self.inner.toplevels.borrow_mut().clear();
        self.inner.tree_to_toplevel.borrow_mut().clear();
        // Mirrors `forget_toplevel`'s own decoration purge, at run
        // granularity instead of per-toplevel — see that method's comment.
        self.inner.decorations.borrow_mut().clear();
    }

    /// Record a newly-announced output's raw pointer under `id`.
    ///
    /// Called from `backend.rs`'s `on_new_output`, before the handler is
    /// told, mirroring [`record_toplevel`](Runtime::record_toplevel).
    pub(crate) fn record_output(&self, id: OutputId, raw: NonNull<sys::wlr_output>) {
        self.inner.outputs.borrow_mut().insert(id, raw);
    }

    /// Remove `id` from the output table. Called from `on_output_destroy`
    /// before the output is freed, mirroring
    /// [`forget_toplevel`](Runtime::forget_toplevel).
    ///
    /// Also nulls this output's raw pointer out of any layer surface still
    /// holding it. [`set_layer_surface_output`](Runtime::set_layer_surface_output)
    /// plants the `*mut wlr_output` directly in the role object's `output`
    /// field, and wlroots never clears it when the output dies; without this
    /// purge, [`LayerSurface::output_id`](crate::LayerSurface::output_id)
    /// would dereference a freed output after a hotplug removal (it reads
    /// `(*output).addons`). This is the single choke point on the destroy
    /// path — `on_output_destroy` always runs it, and nothing else calls
    /// `forget_output` — so nulling here covers every layer surface that
    /// could name the dying output. Sway and the tinywl derivatives null
    /// `layer_surface->output` on output destroy for the identical reason.
    pub(crate) fn forget_output(&self, id: OutputId) {
        // Take the pointer out of the table by the same `remove` that forgets
        // the id, so the comparison below uses the exact `*mut wlr_output`
        // identity `set_layer_surface_output` planted (both come from this
        // `outputs` table). `remove` returns the entry, so no second lookup.
        let dying = self.inner.outputs.borrow_mut().remove(&id);
        if let Some(dying) = dying {
            let dying = dying.as_ptr();
            // Distinct `RefCell` from `outputs` (whose borrow above has already
            // been released by the time this borrow is taken), so no borrow
            // conflict. `values()` reads each entry's `raw` without mutating
            // the table.
            for entry in self.inner.layer_surfaces.borrow().values() {
                let ls = entry.raw.as_ptr();
                // SAFETY: a present `layer_surfaces` entry names a live layer
                // surface — its destroy callback removes the entry before
                // wlroots frees it, the same invariant
                // `set_layer_surface_output` relies on — so `ls` is a valid,
                // dereferenceable `*mut wlr_layer_surface_v1` for this call.
                // `output` is a plain `*mut wlr_output` field; writing null to
                // it is a raw-field assignment, not a call into wlroots, so
                // there is no reentrancy hazard.
                unsafe {
                    if (*ls).output == dying {
                        (*ls).output = std::ptr::null_mut();
                    }
                }
            }
        }
    }

    /// This id's recorded raw output, with the borrow released before
    /// returning — see [`toplevel_entry`](Runtime::toplevel_entry)'s own
    /// doc for why.
    pub(crate) fn output_ptr(&self, id: OutputId) -> Option<NonNull<sys::wlr_output>> {
        self.inner.outputs.borrow().get(&id).copied()
    }

    /// Drop every output this runtime knows of, without touching wlroots.
    ///
    /// Called once, by `backend.rs`'s `run_inner`, when the `run_all` call
    /// that populated this table returns — the exact rule
    /// [`clear_toplevels`](Runtime::clear_toplevels) documents for
    /// toplevels, and for the identical reason: an `OutputId` is only
    /// meaningful for the call that announced it, because the per-output
    /// destroy listener that would otherwise remove a stale entry is torn
    /// down with that call's `Session`.
    pub(crate) fn clear_outputs(&self) {
        self.inner.outputs.borrow_mut().clear();
    }

    /// Record a newly-announced decoration under the id of the toplevel it
    /// was created for.
    pub(crate) fn record_decoration(
        &self,
        id: ToplevelId,
        raw: NonNull<sys::wlr_xdg_toplevel_decoration_v1>,
    ) {
        self.inner.decorations.borrow_mut().insert(
            id,
            DecorationEntry {
                raw,
                mode_set_this_dispatch: std::cell::Cell::new(false),
                staged: std::cell::Cell::new(None),
                answered: std::cell::Cell::new(false),
            },
        );
    }

    /// Remove `id`'s decoration entry. Called from `on_toplevel_decoration_destroy`
    /// before the decoration is freed — the other half of the two-sided
    /// purge `RuntimeInner::decorations` documents.
    pub(crate) fn forget_decoration(&self, id: ToplevelId) {
        self.inner.decorations.borrow_mut().remove(&id);
    }

    /// The decoration `id`'s toplevel names, with the table borrow released
    /// before returning — the same "copy the pointer, drop the borrow"
    /// discipline every other by-id lookup here follows.
    pub(crate) fn decoration_ptr(
        &self,
        id: ToplevelId,
    ) -> Option<NonNull<sys::wlr_xdg_toplevel_decoration_v1>> {
        self.inner.decorations.borrow().get(&id).map(|e| e.raw)
    }

    /// Clear the "a mode was set for the request currently in flight" flag
    /// on `id`'s decoration, if it has one. Called right before this
    /// toplevel's `request_mode` event is delivered — see
    /// [`DecorationEntry`](crate::decoration::DecorationEntry)'s own doc for
    /// the full mechanism this is one half of.
    pub(crate) fn clear_decoration_dispatch_flag(&self, id: ToplevelId) {
        if let Some(entry) = self.inner.decorations.borrow().get(&id) {
            entry.mode_set_this_dispatch.set(false);
        }
    }

    /// Whether [`Runtime::set_decoration_mode`] has already answered the
    /// request currently in flight for `id`. `false` for an id with no
    /// decoration, which is the correct answer for "nothing has set a mode
    /// on it" whether that is because none was requested or because the
    /// decoration is gone.
    pub(crate) fn decoration_dispatch_flag(&self, id: ToplevelId) -> bool {
        self.inner
            .decorations
            .borrow()
            .get(&id)
            .map(|e| e.mode_set_this_dispatch.get())
            .unwrap_or(false)
    }

    /// Take (and clear) the mode staged for `id`'s decoration by a
    /// [`Runtime::set_decoration_mode`] call that landed before the surface
    /// was initialized. `None` if nothing is staged — no decoration, or
    /// every staged decision has already been flushed. Called by
    /// `backend.rs`'s `on_surface_commit` at the toplevel's initial commit;
    /// see [`DecorationEntry`](crate::decoration::DecorationEntry)'s own doc
    /// for the full mechanism.
    pub(crate) fn take_staged_decoration_mode(&self, id: ToplevelId) -> Option<DecorationMode> {
        self.inner
            .decorations
            .borrow()
            .get(&id)
            .and_then(|e| e.staged.take())
    }

    /// Whether `id`'s decoration has ever been given a mode — staged or
    /// sent, by the handler or by the dispatch layer's own default — since
    /// it was created.
    ///
    /// This is the guard on both synthetic "the client never asked" paths
    /// (`on_surface_commit`'s initial-commit block and
    /// `on_new_toplevel_decoration`'s late-creation block). `false` for an
    /// id with no decoration, which correctly stops either path from
    /// emitting for a toplevel that has nothing to answer — both check for
    /// a decoration's existence separately.
    ///
    /// See [`DecorationEntry::answered`](crate::decoration::DecorationEntry)
    /// for why this exists rather than reusing `staged`/`mode_set_this_dispatch`.
    pub(crate) fn decoration_answered(&self, id: ToplevelId) -> bool {
        self.inner
            .decorations
            .borrow()
            .get(&id)
            .map(|e| e.answered.get())
            .unwrap_or(false)
    }

    /// Whether `id` has a decoration, and if so, the client's current
    /// stated preference for it — read fresh from
    /// `wlr_xdg_toplevel_decoration_v1::requested_mode` through
    /// [`crate::decoration::client_side_preference`], not cached, since the
    /// only caller (`on_surface_commit`'s "nothing has ever asked for this
    /// decoration" path) wants whatever is true *now*.
    pub(crate) fn decoration_requested_preference(
        &self,
        id: ToplevelId,
    ) -> Option<Option<DecorationMode>> {
        let raw = self.decoration_ptr(id)?;
        // SAFETY: a present `decorations` entry names a decoration still
        // linked into the table — removed synchronously, before wlroots
        // frees it, by whichever of `forget_decoration`/`forget_toplevel`
        // runs first (see `RuntimeInner::decorations`'s own doc) — so `raw`
        // is live.
        let requested = unsafe { (*raw.as_ptr()).requested_mode };
        Some(crate::decoration::requested_preference(requested))
    }

    /// Stage a size on the toplevel's next configure, in **content**
    /// (client-owned) pixels.
    ///
    /// Staged, not sent: wlroots coalesces every state change made in one
    /// event-loop turn into a single configure, so setting a size, an
    /// activation and a maximized flag in the same handler produces one
    /// configure carrying all three rather than three configures.
    ///
    /// `None` if this runtime has no live toplevel with that id. **A
    /// `ToplevelId` is only good for the [`Backend::run_all`](crate::Backend::run_all)
    /// call that announced it** — every mutator on this page shares that
    /// rule. The table this and every sibling mutator below reads is cleared
    /// when that call returns, the same way an [`OutputId`](crate::OutputId)
    /// stops resolving to anything once its announcing `run` has returned,
    /// so an id kept past that point — even one whose client is still
    /// connected — reports `None` here rather than resolving to a stale
    /// pointer.
    pub fn set_toplevel_size(&self, id: ToplevelId, width: i32, height: i32) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: an entry is removed by the destroy callback, which wlroots
        // runs before it frees the toplevel, so a present entry names a live
        // one. `wlr_xdg_toplevel_set_size` only writes pending state.
        unsafe { sys::wlr_xdg_toplevel_set_size(entry.raw.as_ptr(), width, height) };
        Some(())
    }

    /// Stage the `activated` state — the one a client renders its own title
    /// bar and focus ring from. `None` for an unknown id — including a stale
    /// one; see `set_toplevel_size`'s doc.
    pub fn set_toplevel_activated(&self, id: ToplevelId, activated: bool) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as for `set_toplevel_size`.
        unsafe { sys::wlr_xdg_toplevel_set_activated(entry.raw.as_ptr(), activated) };
        Some(())
    }

    /// Stage the `maximized` state. `None` for an unknown or stale id; see
    /// `set_toplevel_size`'s doc.
    pub fn set_toplevel_maximized(&self, id: ToplevelId, maximized: bool) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as above.
        unsafe { sys::wlr_xdg_toplevel_set_maximized(entry.raw.as_ptr(), maximized) };
        Some(())
    }

    /// Stage the `fullscreen` state. `None` for an unknown or stale id; see
    /// `set_toplevel_size`'s doc.
    pub fn set_toplevel_fullscreen(&self, id: ToplevelId, fullscreen: bool) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as above.
        unsafe { sys::wlr_xdg_toplevel_set_fullscreen(entry.raw.as_ptr(), fullscreen) };
        Some(())
    }

    /// Move the toplevel's scene node. Coordinates are the scene's, which for
    /// a single output at the layout origin are the output's own.
    ///
    /// This is a compositor-side move only: it repositions what is drawn and
    /// where the pointer hit test finds it, and sends the client nothing (a
    /// client does not know where it is, by design in xdg-shell).
    ///
    /// `None` for an unknown or stale id; see `set_toplevel_size`'s doc.
    pub fn set_toplevel_position(&self, id: ToplevelId, x: i32, y: i32) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: the tree is created by this crate when the toplevel is
        // announced and destroyed with it, so a present entry names a live
        // tree.
        unsafe { sys::wlr_scene_node_set_position(&raw mut (*entry.tree.as_ptr()).node, x, y) };
        Some(())
    }

    /// Show or hide the toplevel's scene node.
    ///
    /// Hiding is not unmapping: the client keeps its buffer and its configure
    /// state, it is simply not drawn and not hit-tested. That is what a
    /// window on an inactive workspace needs.
    ///
    /// `None` for an unknown or stale id; see `set_toplevel_size`'s doc.
    pub fn set_toplevel_visible(&self, id: ToplevelId, visible: bool) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as for `set_toplevel_position`.
        unsafe { sys::wlr_scene_node_set_enabled(&raw mut (*entry.tree.as_ptr()).node, visible) };
        Some(())
    }

    /// Raise the toplevel above every other toplevel. `None` for an unknown
    /// or stale id; see `set_toplevel_size`'s doc.
    ///
    /// **Published-behavior note:** since the banded-tree scene design (see
    /// [`Layer`](crate::Layer)'s own doc), this raises the toplevel only
    /// within `toplevel_band` — above every *other toplevel*, never above a
    /// `Top` or `Overlay` layer surface. That is now correct and intended,
    /// not a limitation to work around: `wlr_scene_node_raise_to_top`
    /// reorders siblings, and after the banded-tree fix a toplevel's node is
    /// never a sibling of a layer surface's — it is several levels below
    /// `toplevel_band`, which is itself a fixed root-level sibling of
    /// `top_band`/`overlay_band` that this call never touches. A panel or
    /// launcher placed in `Top`/`Overlay` therefore stays above every
    /// toplevel unconditionally, with no raise call of its own needed —
    /// see [`Layer`](crate::Layer)'s doc for why `raise_layer_surface` does
    /// not exist and is not needed.
    pub fn raise_toplevel(&self, id: ToplevelId) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as above.
        unsafe { sys::wlr_scene_node_raise_to_top(&raw mut (*entry.tree.as_ptr()).node) };
        Some(())
    }

    /// Schedules a bare configure — the protocol-required answer to a state
    /// request ([`ToplevelHandler::request_maximize`](crate::ToplevelHandler::request_maximize)
    /// / [`request_fullscreen`](crate::ToplevelHandler::request_fullscreen))
    /// the compositor declines, or otherwise ignores.
    ///
    /// "Schedules", not "sends", same as every configure this crate
    /// produces: it goes out from an idle source, and wlroots coalesces
    /// this with any configure already staged by
    /// `Runtime::set_toplevel_*` in the same turn into one wire message
    /// rather than two — so calling this after already staging state is
    /// harmless, not a duplicate send.
    ///
    /// `None` for an unknown or stale id; see `set_toplevel_size`'s doc.
    ///
    /// A no-op — returning `Some(())`, not `None` — if the toplevel's
    /// surface has not had its initial commit yet. A client is free to send
    /// `xdg_toplevel.set_maximized`/`set_fullscreen` (and so trigger
    /// [`ToplevelHandler::request_maximize`](crate::ToplevelHandler::request_maximize)/
    /// [`request_fullscreen`](crate::ToplevelHandler::request_fullscreen), whose
    /// dispatch-layer follow-up calls this) before that first commit;
    /// wlroots asserts `surface->initialized` inside
    /// `wlr_xdg_surface_schedule_configure`, and that flag only flips true
    /// during the first commit, so calling through unconditionally would
    /// abort wlroots on that (legal) client ordering. Nothing is lost by
    /// skipping instead: `backend.rs`'s `on_surface_commit` unconditionally
    /// schedules a configure of its own once that first commit lands, and
    /// any state this call would have flushed — `set_toplevel_maximized`/
    /// `set_toplevel_fullscreen` only ever write *pending* fields, with no
    /// schedule of their own — rides that same configure regardless of
    /// whether this method ran before it.
    pub fn configure_toplevel(&self, id: ToplevelId) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: an entry is removed by the destroy callback, which
        // wlroots runs before it frees the toplevel, so a present entry
        // names a live one, and a live `wlr_xdg_toplevel` always has a
        // non-null `base` (its owning `wlr_xdg_surface`), set once at
        // role-object creation and never cleared while the toplevel is
        // alive — the same argument `Toplevel::current_size` documents.
        unsafe {
            let base = (*entry.raw.as_ptr()).base;
            if !(*base).initialized {
                return Some(());
            }
            sys::wlr_xdg_surface_schedule_configure(base);
        }
        Some(())
    }

    /// Ask the client to close.
    ///
    /// A request, not a destruction: a well-behaved client may prompt the
    /// user and decline. The toplevel goes away — and
    /// [`ToplevelHandler::toplevel_destroyed`](crate::ToplevelHandler::toplevel_destroyed)
    /// fires — only if and when the client actually destroys it.
    ///
    /// `None` for an unknown or stale id; see `set_toplevel_size`'s doc.
    pub fn close_toplevel(&self, id: ToplevelId) -> Option<()> {
        let entry = self.toplevel_entry(id)?;
        // SAFETY: as for `set_toplevel_size`; this only sends a protocol
        // event and cannot free the toplevel synchronously.
        unsafe { sys::wlr_xdg_toplevel_send_close(entry.raw.as_ptr()) };
        Some(())
    }

    /// Set the decoration mode for a toplevel that has a decoration object.
    ///
    /// Call this from
    /// [`ToplevelHandler::request_decoration_mode`](crate::ToplevelHandler::request_decoration_mode)
    /// to answer the client's request. `mode` is the answer to send, named
    /// rather than encoded: the same [`DecorationMode`] the handler received
    /// the client's preference in, so honoring that preference is passing it
    /// straight through and nothing has to be remembered about which way a
    /// `bool` points. Calling this marks the request currently in flight as
    /// answered, so the dispatch layer's server-side default (see
    /// `request_decoration_mode`'s own doc) does not also fire once the
    /// handler returns.
    ///
    /// **Staged, not always sent.** wlroots asserts `surface->initialized`
    /// inside `wlr_xdg_surface_schedule_configure`, which
    /// `wlr_xdg_toplevel_decoration_v1_set_mode` calls internally, and that
    /// flag only flips true during the toplevel's first role commit — which
    /// has not necessarily happened yet: the normal client sequence calls
    /// `set_mode` (firing `request_decoration_mode`) *before* its initial
    /// `wl_surface.commit`. So this method sends immediately only if the
    /// surface is already initialized; otherwise it records `mode`
    /// for `backend.rs`'s `on_surface_commit` to send for real at the
    /// toplevel's initial commit — overwriting whatever was staged before,
    /// the same "last write wins" shape [`set_toplevel_size`](Runtime::set_toplevel_size)
    /// already has for the base configure. Either way this returns
    /// `Some(())` and marks the request answered; the difference is
    /// invisible to a caller and exists only to keep this crate from
    /// aborting wlroots on the ordinary decoration-negotiation sequence.
    ///
    /// `None` if `id` is unknown, stale, or names a toplevel with no
    /// decoration object — a client that never created one, or whose
    /// decoration has since been destroyed. See `set_toplevel_size`'s own
    /// doc for what "stale" means for a `ToplevelId` in general.
    pub fn set_decoration_mode(&self, id: ToplevelId, mode: DecorationMode) -> Option<()> {
        let raw = self.decoration_ptr(id)?;
        let entry = self.toplevel_entry(id)?;
        // SAFETY: a present `decorations` entry implies a live toplevel too
        // — both halves of `RuntimeInner::decorations`' purge remove the
        // decoration entry the moment either object dies — and a live
        // `wlr_xdg_toplevel` always has a non-null `base`; see
        // `configure_toplevel`'s identical argument for that second claim.
        let initialized = unsafe { (*(*entry.raw.as_ptr()).base).initialized };

        if initialized {
            // SAFETY: a present entry names a decoration that is still
            // linked into `self.inner.decorations` — removed synchronously,
            // before wlroots frees it, by
            // `forget_decoration`/`forget_toplevel` (see
            // `RuntimeInner::decorations`'s own doc) — so `raw` is live,
            // and `initialized` being true means the
            // `assert(surface->initialized)` this method's own doc
            // describes cannot fire.
            unsafe { sys::wlr_xdg_toplevel_decoration_v1_set_mode(raw.as_ptr(), mode.to_raw()) };
            // This call is now the authoritative, on-the-wire answer, so
            // any earlier staged-but-unflushed decision must not survive
            // it — otherwise `on_surface_commit`'s flush (or a second call
            // to this method after this one, in either order relative to
            // the flush) would resend a stale value and silently override
            // whatever was just sent here. `mark_decoration_answered` is
            // what closes that window: an immediate send always clears
            // `staged`, whether or not one was pending — see its own doc
            // for why this is pulled out rather than inlined here.
            self.mark_decoration_answered(id);
        } else if let Some(entry) = self.inner.decorations.borrow().get(&id) {
            entry.staged.set(Some(mode));
        }

        if let Some(entry) = self.inner.decorations.borrow().get(&id) {
            entry.mode_set_this_dispatch.set(true);
            // Latched here, on both branches, because "a mode was chosen"
            // is true whether it went out on the wire or is waiting for the
            // initial commit to send it. This is what stops either
            // synthetic "the client never asked" path from later
            // overriding this decision with the server-side default; see
            // `DecorationEntry::answered`'s own doc.
            entry.answered.set(true);
        }
        Some(())
    }

    /// The bookkeeping half of an *immediate* decoration answer — clearing
    /// any value staged by an earlier, not-yet-flushed call — pulled out of
    /// [`set_decoration_mode`](Runtime::set_decoration_mode)'s immediate
    /// branch into its own method for exactly one reason: so it can be
    /// exercised by a test without going through the real
    /// `wlr_xdg_toplevel_decoration_v1_set_mode` FFI call, which asserts
    /// `surface->initialized` and then reaches deep enough into wlroots
    /// (`configure_list`, the surface's real `wl_resource`) that a
    /// fabricated surface segfaults rather than merely misbehaving — this
    /// was verified empirically, not assumed, before this method existed as
    /// a separate, safely-testable unit. Every real caller reaches this
    /// only from `set_decoration_mode`'s immediate branch, i.e. only after
    /// the FFI call has actually run; this method itself makes no such
    /// claim and cannot enforce it, so it stays private to this module
    /// (visible to `tests` as a descendant of it) rather than becoming a
    /// second public entry point a consumer could call out of order.
    fn mark_decoration_answered(&self, id: ToplevelId) {
        if let Some(entry) = self.inner.decorations.borrow().get(&id) {
            entry.staged.set(None);
        }
    }

    /// Advertise `zwlr_layer_shell_v1`.
    ///
    /// Call once at boot, after [`create_xdg_shell`](Runtime::create_xdg_shell)
    /// — nothing here actually requires an xdg shell to exist (layer surfaces
    /// are independent of it), but this crate places layer surfaces into the
    /// same scene [`init_graphics`](Runtime::init_graphics) creates, and
    /// pairing the two calls keeps a compositor's boot sequence in one
    /// order. Unlike `create_xdg_shell`, `init_graphics` having run is
    /// *not* checked here: a scene is only needed once a client actually
    /// creates a surface (`backend.rs`'s `on_new_layer_surface` drops the
    /// announcement if there is none yet), not at the point the global
    /// itself is advertised — mirroring
    /// [`create_xdg_decoration_manager`](Runtime::create_xdg_decoration_manager)'s
    /// own reasoning for the same omission.
    ///
    /// Registration of the `new_surface` listener happens inside
    /// [`Backend::run_all`](crate::Backend::run_all) and lives for that
    /// call, so creating the shell after a run has started has no effect
    /// until the next one — the rule every global-advertising call in this
    /// crate follows.
    ///
    /// `version` is a parameter for the same reason
    /// [`create_xdg_shell`](Runtime::create_xdg_shell)'s is; pass 4 unless
    /// you know otherwise.
    ///
    /// The pointer cached here is tied to `display` for good: see
    /// [`create_xdg_shell`](Runtime::create_xdg_shell)'s *One `Runtime` per
    /// `Display`* section, which applies verbatim to this shell.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if a layer shell already exists on this runtime;
    /// [`Error::Create`] if wlroots could not create it.
    pub fn create_layer_shell(&self, display: &Display, version: u32) -> Result<()> {
        if self.inner.layer_shell.borrow().is_some() {
            return Err(Error::Operation("Runtime::create_layer_shell called twice"));
        }
        // SAFETY: `display` is live for the call; the returned shell is
        // owned by the display and destroyed with it, so this crate never
        // frees it.
        let raw = unsafe { sys::wlr_layer_shell_v1_create(display.as_ptr(), version) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_layer_shell_v1_create"))?;
        *self.inner.layer_shell.borrow_mut() = Some(raw);
        Ok(())
    }

    pub(crate) fn layer_shell_ptr(&self) -> Option<NonNull<sys::wlr_layer_shell_v1>> {
        *self.inner.layer_shell.borrow()
    }

    /// Record a newly-announced layer surface under `id`. `band` is the
    /// layer its scene tree was placed under at creation (`backend.rs`'s
    /// `on_new_layer_surface`) — the starting point
    /// [`reparent_layer_surface_if_changed`](Runtime::reparent_layer_surface_if_changed)
    /// compares every later commit's layer against.
    pub(crate) fn record_layer_surface(
        &self,
        id: LayerSurfaceId,
        raw: NonNull<sys::wlr_layer_surface_v1>,
        scene_tree: NonNull<sys::wlr_scene_tree>,
        band: Layer,
    ) {
        self.inner.layer_surfaces.borrow_mut().insert(
            id,
            LayerSurfaceEntry {
                raw,
                scene_tree,
                staged_configure: std::cell::Cell::new(None),
                band: std::cell::Cell::new(band),
            },
        );
    }

    /// Remove `id`'s entry. Called from `on_layer_surface_destroy` before
    /// the layer surface is freed, mirroring
    /// [`forget_toplevel`](Runtime::forget_toplevel).
    pub(crate) fn forget_layer_surface(&self, id: LayerSurfaceId) {
        self.inner.layer_surfaces.borrow_mut().remove(&id);
    }

    /// This id's recorded raw layer surface, with the borrow released before
    /// returning — see [`toplevel_entry`](Runtime::toplevel_entry)'s own doc
    /// for why.
    pub(crate) fn layer_surface_ptr(
        &self,
        id: LayerSurfaceId,
    ) -> Option<NonNull<sys::wlr_layer_surface_v1>> {
        self.inner.layer_surfaces.borrow().get(&id).map(|e| e.raw)
    }

    /// This id's recorded scene tree, with the borrow released before
    /// returning — as [`layer_surface_ptr`](Runtime::layer_surface_ptr).
    pub(crate) fn layer_surface_scene_ptr(
        &self,
        id: LayerSurfaceId,
    ) -> Option<NonNull<sys::wlr_scene_tree>> {
        self.inner
            .layer_surfaces
            .borrow()
            .get(&id)
            .map(|e| e.scene_tree)
    }

    /// Move `id`'s scene tree into the band matching `layer`, but only if
    /// that differs from the band it is already parented under — called by
    /// `backend.rs`'s `on_layer_surface_commit` on every commit with the
    /// layer that commit just made current.
    ///
    /// A client is free to send `zwlr_layer_surface_v1.set_layer` (protocol
    /// version 2+) after its surface is already mapped, and unlike `anchor`/
    /// `exclusive_zone`/size, which this crate leaves the handler to notice
    /// and act on itself, a stale *band* would misstack the surface with no
    /// way for a consumer to fix it — there is no `raise_layer_surface`, and
    /// there does not need to be one; see [`Layer`](crate::Layer)'s own doc.
    /// So this runs unconditionally from the dispatch layer rather than
    /// waiting on a handler to ask for it.
    ///
    /// `None` if this runtime has no live layer surface with that id
    /// (including a stale one). Reparenting itself never fails: a live
    /// entry's `scene_tree` is a real node in this runtime's own scene, and
    /// [`layer_band_ptr`](Runtime::layer_band_ptr) resolving to `None` here
    /// would mean `init_graphics` was undone after this surface's tree was
    /// created, which cannot happen — the tree could not have been created
    /// without it in the first place.
    pub(crate) fn reparent_layer_surface_if_changed(
        &self,
        id: LayerSurfaceId,
        layer: Layer,
    ) -> Option<()> {
        let (tree, changed) = {
            let surfaces = self.inner.layer_surfaces.borrow();
            let entry = surfaces.get(&id)?;
            let changed = entry.band.get() != layer;
            if changed {
                entry.band.set(layer);
            }
            (entry.scene_tree, changed)
        };
        if !changed {
            return Some(());
        }
        let Some(band) = self.layer_band_ptr(layer) else {
            return Some(());
        };
        // SAFETY: `tree` came from a live `LayerSurfaceEntry` (the table
        // lookup above just resolved it), so it names a scene node this
        // runtime's own scene still owns; `band` is one of the five band
        // trees created once in `init_graphics` and never destroyed while
        // this runtime is.
        unsafe { sys::wlr_scene_node_reparent(&raw mut (*tree.as_ptr()).node, band.as_ptr()) };
        Some(())
    }

    /// Record a size for `id` to send once its surface is initialized,
    /// overwriting whatever was staged before — the same "last write wins"
    /// shape [`set_toplevel_size`](Runtime::set_toplevel_size) has for the
    /// base xdg-shell configure.
    fn stage_layer_configure(&self, id: LayerSurfaceId, width: u32, height: u32) {
        if let Some(entry) = self.inner.layer_surfaces.borrow().get(&id) {
            entry.staged_configure.set(Some((width, height)));
        }
    }

    /// Take (and clear) the size staged for `id`, if any. Called by
    /// `backend.rs`'s `on_layer_surface_commit` at the surface's initial
    /// commit, and by [`configure_layer_surface`](Runtime::configure_layer_surface)'s
    /// own immediate branch — see that method's own doc for why an
    /// immediate send also clears this.
    pub(crate) fn take_staged_layer_configure(&self, id: LayerSurfaceId) -> Option<(u32, u32)> {
        self.inner
            .layer_surfaces
            .borrow()
            .get(&id)
            .and_then(|e| e.staged_configure.take())
    }

    /// Drop every layer surface this runtime knows of, without touching
    /// wlroots. Called once, by `backend.rs`'s `run_inner`, when the
    /// `run_all` call that populated this table returns — mirroring
    /// [`clear_toplevels`](Runtime::clear_toplevels), and for the identical
    /// reason.
    pub(crate) fn clear_layer_surfaces(&self) {
        self.inner.layer_surfaces.borrow_mut().clear();
    }

    /// Answer a layer surface's (re)configure with the size the compositor
    /// chose, in surface-local pixels.
    ///
    /// **Staged, not always sent, because the early call would abort the
    /// process otherwise.** See `layer.rs`'s own module doc for the full
    /// argument: `wlr_layer_surface_v1_configure` asserts
    /// `surface->initialized` (`types/wlr_layer_shell_v1.c:318`,
    /// `assert(surface->initialized)`, confirmed compiled into the shipped
    /// `libwlroots-0.20.so` — the distribution does not build wlroots with
    /// `NDEBUG`), and that flag only flips true during the surface's first
    /// commit, so calling this before that commit and sending unconditionally
    /// would abort the whole compositor process on an entirely legal client
    /// ordering. This method sends immediately only if the surface is
    /// already initialized; otherwise it records `width`/`height` for
    /// `backend.rs`'s `on_layer_surface_commit` to send for real at the
    /// surface's initial commit. Either way this returns `Some(())`; the
    /// difference is invisible to a caller.
    ///
    /// The same guard also protects a second window this crate mirrors
    /// without being told to by wlroots' own docs: `initialized` is reset
    /// to `false` again on the surface's *unmap* commit
    /// (`layer_surface_reset`, invoked from `layer_surface_role_commit`),
    /// so a layer surface re-enters the uninitialized state after every
    /// unmap. A call to this method made from
    /// [`ToplevelHandler::layer_surface_unmapped`](crate::ToplevelHandler::layer_surface_unmapped)
    /// (or from any point after an unmap and before the surface's next
    /// commit) would abort exactly the same way without this staging — this
    /// is not incidental, and the staging must never be simplified away to
    /// an unconditional send.
    ///
    /// The immediate branch also clears any earlier staged value (there
    /// should not be one — a caller only reaches the immediate branch once
    /// `initialized` has gone true, at which point `on_layer_surface_commit`
    /// has either already flushed whatever was staged or found nothing to
    /// flush — but doing so unconditionally is what stops a stale staged
    /// size from being sent a second time by a *later* commit if this
    /// method is ever called more than once for the same surface before its
    /// first commit lands).
    ///
    /// `None` if this runtime has no live layer surface with that id. **A
    /// `LayerSurfaceId` is only good for the
    /// [`Backend::run_all`](crate::Backend::run_all) call that announced
    /// it** — the same rule every by-id mutator in this crate follows; see
    /// [`set_toplevel_size`](Runtime::set_toplevel_size)'s own doc.
    ///
    /// **If nothing ever calls this for a given layer surface, that
    /// surface's client blocks forever** — unlike xdg-shell, nothing in this
    /// crate's dispatch layer sends a fallback configure, because there is
    /// no universally sane default size to invent for a surface that asked
    /// for `0x0`. Call this from
    /// [`ToplevelHandler::new_layer_surface`](crate::ToplevelHandler::new_layer_surface)
    /// or [`ToplevelHandler::layer_surface_commit`](crate::ToplevelHandler::layer_surface_commit)
    /// for every layer surface this crate hands you; see `layer.rs`'s "Answering
    /// `new_layer_surface` is mandatory" module section for the full argument.
    pub fn configure_layer_surface(
        &self,
        id: LayerSurfaceId,
        width: u32,
        height: u32,
    ) -> Option<()> {
        let raw = self.layer_surface_ptr(id)?;
        // SAFETY: an entry is removed by `on_layer_surface_destroy`, which
        // wlroots runs before it frees the layer surface, so a present
        // entry names a live one.
        let initialized = unsafe { (*raw.as_ptr()).initialized };
        if initialized {
            // SAFETY: `initialized` being true is exactly the precondition
            // this method's own doc describes as making the call safe.
            unsafe { sys::wlr_layer_surface_v1_configure(raw.as_ptr(), width, height) };
            self.take_staged_layer_configure(id);
        } else {
            self.stage_layer_configure(id, width, height);
        }
        Some(())
    }

    /// Position the layer surface's scene node in layout coordinates.
    ///
    /// This is a compositor-side move only, exactly like
    /// [`set_toplevel_position`](Runtime::set_toplevel_position): it
    /// repositions what is drawn and where the pointer hit test finds it,
    /// and sends the client nothing. A compositor implementing anchoring
    /// itself calls this from
    /// [`ToplevelHandler::layer_surface_commit`](crate::ToplevelHandler::layer_surface_commit),
    /// after reading [`LayerSurface::anchor`](crate::LayerSurface::anchor)
    /// and the surface's actual size.
    ///
    /// `None` for an unknown or stale id; see
    /// [`set_toplevel_size`](Runtime::set_toplevel_size)'s own doc for what
    /// "stale" means.
    pub fn set_layer_surface_position(&self, id: LayerSurfaceId, x: i32, y: i32) -> Option<()> {
        let tree = self.layer_surface_scene_ptr(id)?;
        // SAFETY: the tree is created by this crate when the layer surface
        // is announced and destroyed with it (wlroots' own
        // `wlr_scene_layer_surface_v1` links its tree's destruction to the
        // layer surface's own), so a present entry names a live tree.
        unsafe { sys::wlr_scene_node_set_position(&raw mut (*tree.as_ptr()).node, x, y) };
        Some(())
    }

    /// Give the keyboard focus to `id`.
    ///
    /// Mirrors [`focus_toplevel_keyboard`](Runtime::focus_toplevel_keyboard)
    /// exactly, targeting a layer surface's `wlr_surface` instead of a
    /// toplevel's — same idempotence, same modifier/held-key replay on
    /// enter, and the same reasons for each. The next call to
    /// [`focus_toplevel_keyboard`](Runtime::focus_toplevel_keyboard) or
    /// [`clear_keyboard_focus`](Runtime::clear_keyboard_focus) replaces
    /// this focus, exactly as it would replace another toplevel's — this
    /// crate's model has no separate "layer focus" slot, only "whatever the
    /// seat's keyboard focus currently is".
    ///
    /// `None` if this runtime has no seat, no live layer surface with that
    /// id, or the surface is **not currently mapped** — the identical
    /// "resolves or it does not" reasoning
    /// [`focus_toplevel_keyboard`](Runtime::focus_toplevel_keyboard)'s own
    /// doc gives for its toplevel counterpart, checked against
    /// `wlr_surface::mapped` here too rather than tracked separately.
    pub fn focus_layer_keyboard(&self, id: LayerSurfaceId) -> Option<()> {
        let seat = *self.inner.seat.borrow();
        let seat = seat?;
        let raw = self.layer_surface_ptr(id)?;

        // SAFETY: a present entry names a live layer surface (its destroy
        // callback removes the entry before wlroots frees it), so
        // `.surface` is either null (checked below) or a live surface.
        // `wlr_seat_get_keyboard` returns null when no keyboard is
        // attached, which the enter call tolerates by taking no keycodes —
        // the identical shape `focus_toplevel_keyboard` follows.
        unsafe {
            let surface = (*raw.as_ptr()).surface;
            if surface.is_null() {
                return None;
            }
            if !(*surface).mapped {
                return None;
            }
            if (*seat.as_ptr()).keyboard_state.focused_surface == surface {
                return Some(());
            }
            let kb = sys::wlr_seat_get_keyboard(seat.as_ptr());
            if kb.is_null() {
                sys::wlr_seat_keyboard_notify_enter(
                    seat.as_ptr(),
                    surface,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                );
            } else {
                sys::wlr_seat_keyboard_notify_enter(
                    seat.as_ptr(),
                    surface,
                    (*kb).keycodes.as_ptr(),
                    (*kb).num_keycodes,
                    &raw mut (*kb).modifiers,
                );
            }
        }
        Some(())
    }

    /// Assign `id`'s layer surface to `output`.
    ///
    /// Discharges the responsibility wlr-layer-shell's own doc for
    /// `wlr_layer_shell_v1::events::new_surface` states plainly — "the
    /// output may be NULL. In this case, it is your responsibility to
    /// assign an output before returning" (see
    /// [`LayerSurface::output_id`](crate::LayerSurface::output_id)'s own
    /// doc, which named this exact gap and deferred it here). Call it from
    /// [`ToplevelHandler::new_layer_surface`](crate::ToplevelHandler::new_layer_surface)
    /// when [`LayerSurface::output_id`](crate::LayerSurface::output_id)
    /// reports `None` for a surface that just arrived — typically picking
    /// whichever output currently has the seat's cursor, or simply the
    /// first output this runtime knows of for a single-output compositor.
    ///
    /// Sets the role object's `output` field directly
    /// (`(*raw).output = output_ptr`) rather than going through a wlroots
    /// setter function: `wlr_layer_shell_v1.h` exposes none — `output` is a
    /// plain `struct wlr_output *` on `wlr_layer_surface_v1`, assigned by
    /// convention rather than through an accessor (confirmed against the
    /// installed 0.20 header; every in-tree wlroots compositor that
    /// implements this same responsibility, e.g. tinywl, assigns the field
    /// the identical way). No `wlr_scene_layer_surface_v1` re-arrange
    /// follows: this crate's whole layer-surface model is manual
    /// positioning — [`set_layer_surface_position`](Runtime::set_layer_surface_position)'s
    /// own doc — so nothing here auto-arranges by output layout either; a
    /// consumer that anchors to the output reads its size itself (from
    /// wherever it tracks `output_layout_box`/output mode) and calls
    /// [`set_layer_surface_position`](Runtime::set_layer_surface_position),
    /// exactly as it already does for the anchor-driven case.
    ///
    /// `None` if this runtime has no live layer surface with that id
    /// (including a stale one; see
    /// [`set_toplevel_size`](Runtime::set_toplevel_size)'s own doc for what
    /// "stale" means) or no live output with that id. The layer-surface id
    /// is resolved first, so an unknown/stale `output` is only ever
    /// reached — and only ever the reason for a `None` — once the layer
    /// surface itself is known to be live.
    pub fn set_layer_surface_output(&self, id: LayerSurfaceId, output: OutputId) -> Option<()> {
        let raw = self.layer_surface_ptr(id)?;
        let out = self.output_ptr(output)?;
        // SAFETY: a present `layer_surfaces` entry names a live layer
        // surface (its destroy callback removes the entry before wlroots
        // frees it), and a present `outputs` entry names a live output
        // (the identical rule `output_ptr`'s own doc states) — both
        // outlive this call. This assigns a raw field, not a call into
        // wlroots, so there is no reentrancy hazard to guard against here.
        unsafe {
            (*raw.as_ptr()).output = out.as_ptr();
        }
        Some(())
    }

    /// Create the seat, its cursor, and the cursor theme.
    ///
    /// One call rather than three for the same reason
    /// [`init_graphics`](Runtime::init_graphics) bundles its six: a seat with
    /// no cursor attached to the output layout produces pointer coordinates
    /// that mean nothing, and there is no useful compositor that wants one
    /// without the other.
    ///
    /// Works whether or not [`init_graphics`](Runtime::init_graphics) has run
    /// yet — a compositor is free to create its seat first — but a cursor
    /// created before graphics has no output layout to attach to, and
    /// `wlr_cursor_attach_output_layout`'s own doc says a cursor left that
    /// way "allows infinite movement in any direction and does not support
    /// absolute input events" until one is attached. This crate does
    /// not attach one retroactively if `init_graphics` runs later; call
    /// `init_graphics` first if that matters to you.
    ///
    /// The seat's capabilities are recomputed as devices arrive (see
    /// `backend.rs`'s `on_new_input`), so a session with no keyboard yet does
    /// not advertise one.
    ///
    /// # Errors
    ///
    /// [`Error::Operation`] if a seat already exists on this runtime;
    /// [`Error::Create`] naming whichever wlroots constructor returned null.
    pub fn create_seat(&self, display: &Display, name: &str) -> Result<()> {
        if self.inner.seat.borrow().is_some() {
            return Err(Error::Operation("Runtime::create_seat called twice"));
        }
        let layout = self.inner.graphics.borrow().as_ref().map(|g| g.layout);
        let c_name = std::ffi::CString::new(name)
            .map_err(|_| Error::Operation("Runtime::create_seat name contains a NUL"))?;

        // SAFETY: `display` is live for the call; the seat and the cursor are
        // owned by wlroots and torn down with the display; each pointer is
        // null-checked before use. `layout`, when present, is this runtime's
        // own from `init_graphics` and outlives the call (see `Graphics`'s
        // own doc); when absent, `wlr_cursor_attach_output_layout` is simply
        // not called, which is the documented "no layout attached" state.
        unsafe {
            let seat = sys::wlr_seat_create(display.as_ptr(), c_name.as_ptr());
            let seat = NonNull::new(seat).ok_or(Error::Create("wlr_seat_create"))?;

            let cursor = sys::wlr_cursor_create();
            let cursor = NonNull::new(cursor).ok_or(Error::Create("wlr_cursor_create"))?;
            if let Some(layout) = layout {
                sys::wlr_cursor_attach_output_layout(cursor.as_ptr(), layout.as_ptr());
            }

            let xcursor = sys::wlr_xcursor_manager_create(std::ptr::null(), 24);
            let xcursor =
                NonNull::new(xcursor).ok_or(Error::Create("wlr_xcursor_manager_create"))?;

            *self.inner.seat.borrow_mut() = Some(seat);
            *self.inner.cursor.borrow_mut() = Some(cursor);
            *self.inner.xcursor.borrow_mut() = Some(xcursor);
        }
        Ok(())
    }

    /// Give the keyboard focus to `id`.
    ///
    /// Sends the client the modifier state and the currently-held keys along
    /// with the enter, which is what stops a newly-focused client believing
    /// no keys are down when one is. Idempotent: focusing the already-focused
    /// surface sends nothing, so a compositor may call this on every geometry
    /// sync without churning leave/enter pairs at the client.
    ///
    /// `None` if this runtime has no seat, no live toplevel with that id, or
    /// the toplevel is **not currently mapped**. The last case is the ledger
    /// entry this method exists to close: this crate's own model has no
    /// concept of "unmapped" visible to a by-id caller (an id resolves or it
    /// does not), but wlroots does — a toplevel can exist, and be announced,
    /// before its client ever attaches a buffer, and again after that buffer
    /// is withdrawn — and asking wlroots to focus an unmapped surface's
    /// keyboard is not something this crate lets through silently. Checked
    /// directly against `wlr_surface::mapped`, which is wlroots' own flag,
    /// rather than tracked separately here, so it can never drift out of
    /// step with the map/unmap events
    /// [`ToplevelHandler`](crate::ToplevelHandler) delivers.
    pub fn focus_toplevel_keyboard(&self, id: ToplevelId) -> Option<()> {
        let seat = *self.inner.seat.borrow();
        let seat = seat?;
        let entry = self.toplevel_entry(id)?;

        // SAFETY: a present entry names a live toplevel (its destroy callback
        // removes the entry before wlroots frees it), so `base->surface` is a
        // live surface. `wlr_seat_get_keyboard` returns null when no keyboard
        // is attached, which the enter call tolerates by taking no keycodes.
        unsafe {
            let base = (*entry.raw.as_ptr()).base;
            if base.is_null() {
                return None;
            }
            let surface = (*base).surface;
            if surface.is_null() {
                return None;
            }
            // The unmapped check: see this method's own doc for why.
            if !(*surface).mapped {
                return None;
            }
            if (*seat.as_ptr()).keyboard_state.focused_surface == surface {
                return Some(());
            }
            let kb = sys::wlr_seat_get_keyboard(seat.as_ptr());
            if kb.is_null() {
                sys::wlr_seat_keyboard_notify_enter(
                    seat.as_ptr(),
                    surface,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                );
            } else {
                sys::wlr_seat_keyboard_notify_enter(
                    seat.as_ptr(),
                    surface,
                    (*kb).keycodes.as_ptr(),
                    (*kb).num_keycodes,
                    &raw mut (*kb).modifiers,
                );
            }
        }
        Some(())
    }

    /// Take the keyboard focus away from whatever has it.
    ///
    /// Harmless when nothing is focused, which matters: this is what every
    /// "nothing is focused now" path calls, unconditionally.
    pub fn clear_keyboard_focus(&self) {
        let seat = *self.inner.seat.borrow();
        let Some(seat) = seat else { return };
        // SAFETY: the seat is owned by the display and live for as long as
        // this runtime can be used; the call is a no-op when nothing has
        // focus.
        unsafe { sys::wlr_seat_keyboard_notify_clear_focus(seat.as_ptr()) };
    }

    /// The topmost **toplevel** at scene coordinates `(x, y)`, and the
    /// position of `(x, y)` **within that toplevel** — relative to the same
    /// origin [`set_toplevel_position`](Runtime::set_toplevel_position) sets,
    /// so a caller that has just been handed a click never has to look the
    /// window's position up and subtract it itself.
    ///
    /// Both halves name the same thing on purpose. `wlr_scene_node_at`'s own
    /// out-parameters are relative to the *leaf node it struck* — a
    /// subsurface's or a popup's buffer node, several levels below the
    /// toplevel's root — which is a different surface from the one the
    /// returned id names whenever the hit is not on the toplevel's own
    /// buffer. Reporting those raw would make the tuple mean two
    /// unrelated things at once, and a caller computing a drag offset from
    /// them would see the window jump the moment a click landed on a
    /// client-side decoration. The leaf offset is subtracted back out here:
    /// the toplevel's scene tree is a direct child of `toplevel_band` (see
    /// `backend.rs`'s `wlr_scene_xdg_surface_create` call), and
    /// `toplevel_band` itself is never repositioned away from the scene
    /// root's own origin (see `Graphics::background_band`'s doc), so its
    /// node's `x`/`y` *are* still its scene-absolute origin, and `(x, y)`
    /// minus that is the window-relative position regardless of how deep
    /// the struck leaf was.
    ///
    /// This is **not** the method pointer forwarding uses, and a caller must
    /// not build one out of it: notifying the toplevel's own root surface
    /// when the hit landed on a popup delivers the click to the wrong
    /// surface (a popup menu gets no input at all). `backend.rs` uses
    /// `leaf_surface_at` for that, which resolves the actual struck surface.
    /// This method answers "which window, and where in it" — raising,
    /// focusing or starting a move on whatever is under the pointer.
    ///
    /// Walks the scene with `wlr_scene_node_at` and then walks *up* from
    /// whatever node it found to the tree this crate created for a toplevel.
    ///
    /// `None` when nothing is there, which includes hitting the background
    /// rect, and when [`init_graphics`](Runtime::init_graphics) has not run
    /// (there is no scene to test against).
    pub fn toplevel_at(&self, x: f64, y: f64) -> Option<(ToplevelId, f64, f64)> {
        let scene = self.scene_ptr()?;
        let mut nx = 0.0;
        let mut ny = 0.0;
        // SAFETY: the scene is this runtime's own and outlives the call; the
        // two out-parameters are live stack locals.
        let node = unsafe {
            sys::wlr_scene_node_at(
                &raw mut (*scene.as_ptr()).tree.node,
                x,
                y,
                &raw mut nx,
                &raw mut ny,
            )
        };
        if node.is_null() {
            return None;
        }
        // `nx`/`ny` are leaf-relative and deliberately go unused: see this
        // method's own doc for why they are not what a caller is given.
        let _ = (nx, ny);

        // The borrow is taken and released inside the loop rather than held
        // across it: nothing in the loop calls out, but the rule is absolute.
        // SAFETY: `node` is a live node in this scene, and every `parent`
        // pointer in a scene is either a live tree or null at the root.
        let mut tree = unsafe { (*node).parent };
        while !tree.is_null() {
            let found = self
                .inner
                .tree_to_toplevel
                .borrow()
                .get(&(tree as usize))
                .copied();
            if let Some(id) = found {
                // SAFETY: `tree` is a live scene tree — it is in this
                // runtime's table, whose entries are removed when the
                // toplevel is destroyed, and it was just reached by walking
                // live parent pointers.
                let (tx, ty) = unsafe { ((*tree).node.x, (*tree).node.y) };
                return Some((id, x - f64::from(tx), y - f64::from(ty)));
            }
            // SAFETY: as above.
            tree = unsafe { (*tree).node.parent };
        }
        None
    }

    /// The actual surface under `(x, y)`, and the position within *that*
    /// surface — as opposed to [`toplevel_at`](Runtime::toplevel_at), which
    /// answers "which window, and where in that window". A hit on a popup
    /// belonging to a toplevel is reported here as the popup's own surface
    /// and the popup's own local coordinates; `toplevel_at` reports the
    /// owning window and a position relative to the window.
    ///
    /// A hit inside a subsurface or a popup lands on a different
    /// `wlr_surface` than the toplevel's own root, at different local
    /// coordinates; this resolves the node `wlr_scene_node_at` struck all
    /// the way down to the specific surface it belongs to
    /// (`wlr_scene_buffer_from_node` then `wlr_scene_surface_try_from_buffer`
    /// — the pattern wlroots' own `tinywl` uses), which is what pointer
    /// forwarding needs: notifying the wrong surface either loses the input
    /// entirely (a popup menu that never receives a click) or delivers it at
    /// a skewed offset (a subsurface notified with its parent's coordinates).
    ///
    /// `None` when nothing is there, or the struck node is not a buffer —
    /// confirmed against wlroots 0.20's own `wlr_scene.c`, a hit can also
    /// land on a `WLR_SCENE_NODE_RECT` (this crate's own background rect,
    /// say), which `wlr_scene_buffer_from_node` documents itself as
    /// undefined behaviour to call on — or the buffer is not backed by any
    /// surface at all.
    pub(crate) fn leaf_surface_at(
        &self,
        x: f64,
        y: f64,
    ) -> Option<(*mut sys::wlr_surface, f64, f64)> {
        let scene = self.scene_ptr()?;
        let mut nx = 0.0;
        let mut ny = 0.0;
        // SAFETY: the scene is this runtime's own and outlives the call; the
        // two out-parameters are live stack locals.
        let node = unsafe {
            sys::wlr_scene_node_at(
                &raw mut (*scene.as_ptr()).tree.node,
                x,
                y,
                &raw mut nx,
                &raw mut ny,
            )
        };
        if node.is_null() {
            return None;
        }
        // SAFETY: `node` is a live node in this scene (just returned by
        // `wlr_scene_node_at` above); reading `type_` is always sound, and
        // the check below is exactly what makes the following
        // `wlr_scene_buffer_from_node` call legal — its own doc requires the
        // node to represent a `wlr_scene_buffer`, which only
        // `WLR_SCENE_NODE_BUFFER` does (a hit can also land on
        // `WLR_SCENE_NODE_RECT`, per `wlr_scene.c`'s own hit-test
        // candidates, which this guards against).
        unsafe {
            if (*node).type_ != sys::wlr_scene_node_type::WLR_SCENE_NODE_BUFFER {
                return None;
            }
            let buffer = sys::wlr_scene_buffer_from_node(node);
            let scene_surface = sys::wlr_scene_surface_try_from_buffer(buffer);
            if scene_surface.is_null() {
                // A buffer node not backed by a surface at all (a plain
                // texture, say) — nothing for the seat to notify.
                return None;
            }
            let surface = (*scene_surface).surface;
            if surface.is_null() {
                return None;
            }
            Some((surface, nx, ny))
        }
    }

    /// Test-only: synthesize a touch-down at `(x, y)` so a headless test can
    /// drive a touch drag. Resolves the struck surface and its local
    /// coordinates via [`leaf_surface_at`](Runtime::leaf_surface_at) — the
    /// same hit test pointer forwarding uses — and forwards to
    /// `wlr_seat_touch_notify_down`, which defers to any active touch grab.
    /// Returns the down serial (a valid touch grab serial) on success,
    /// `None` when there is no seat or nothing under `(x, y)`.
    ///
    /// No production caller — there is no virtual-touch Wayland protocol,
    /// so this exists solely for a headless harness to synthesize touch
    /// input in tests.
    #[doc(hidden)]
    pub fn inject_touch_down(&self, x: f64, y: f64, id: i32, time_msec: u32) -> Option<u32> {
        let seat = self.seat_ptr()?;
        let (surface, sx, sy) = self.leaf_surface_at(x, y)?;
        // SAFETY: `seat` is this runtime's own live seat (from
        // `seat_ptr`); `surface` was just resolved from a hit test against
        // this runtime's own live scene and is therefore live too.
        // wlroots reads `sx`/`sy` by value and does not retain them.
        Some(unsafe { sys::wlr_seat_touch_notify_down(seat.as_ptr(), surface, time_msec, id, sx, sy) })
    }

    /// Test-only: synthesize a touch-motion to `(x, y)` for the touch point
    /// `id`, continuing a drag started with
    /// [`inject_touch_down`](Runtime::inject_touch_down). Re-resolves the
    /// surface-local coordinates at the new position via
    /// [`leaf_surface_at`](Runtime::leaf_surface_at) and forwards to
    /// `wlr_seat_touch_notify_motion`.
    ///
    /// `wlr_seat_touch_notify_motion` takes no surface parameter — it
    /// addresses the touch point `id` already registered by the matching
    /// `notify_down` and delivers to whichever client owns it. When `(x,
    /// y)` has moved off every surface, there is nothing to resolve
    /// coordinates against, so this is a deliberate no-op rather than
    /// notifying with stale or zeroed coordinates: a drag that exits scene
    /// bounds simply stops updating position until it re-enters one.
    ///
    /// No production caller — see [`inject_touch_down`](Runtime::inject_touch_down).
    #[doc(hidden)]
    pub fn inject_touch_motion(&self, x: f64, y: f64, id: i32, time_msec: u32) {
        let Some(seat) = self.seat_ptr() else {
            return;
        };
        let Some((_surface, sx, sy)) = self.leaf_surface_at(x, y) else {
            return;
        };
        // SAFETY: `seat` is this runtime's own live seat; `sx`/`sy` are
        // read by value and not retained.
        unsafe { sys::wlr_seat_touch_notify_motion(seat.as_ptr(), time_msec, id, sx, sy) };
    }

    /// Test-only: synthesize a touch-up for the touch point `id`, ending a
    /// drag started with [`inject_touch_down`](Runtime::inject_touch_down).
    /// Forwards to `wlr_seat_touch_notify_up`, which defers to any active
    /// touch grab and removes the touch point. A no-op when there is no
    /// seat.
    ///
    /// No production caller — see [`inject_touch_down`](Runtime::inject_touch_down).
    #[doc(hidden)]
    pub fn inject_touch_up(&self, id: i32, time_msec: u32) {
        let Some(seat) = self.seat_ptr() else {
            return;
        };
        // SAFETY: `seat` is this runtime's own live seat.
        unsafe { sys::wlr_seat_touch_notify_up(seat.as_ptr(), time_msec, id) };
    }

    /// Where the cursor is, in scene coordinates. `(0.0, 0.0)` with no seat.
    pub fn pointer_position(&self) -> (f64, f64) {
        let cursor = *self.inner.cursor.borrow();
        let Some(cursor) = cursor else {
            return (0.0, 0.0);
        };
        // SAFETY: the cursor was created by `create_seat` and lives as long
        // as this runtime.
        unsafe { ((*cursor.as_ptr()).x, (*cursor.as_ptr()).y) }
    }

    pub(crate) fn seat_ptr(&self) -> Option<NonNull<sys::wlr_seat>> {
        *self.inner.seat.borrow()
    }

    pub(crate) fn cursor_ptr(&self) -> Option<NonNull<sys::wlr_cursor>> {
        *self.inner.cursor.borrow()
    }

    /// Make sure the cursor has an image, loading the default xcursor theme
    /// on the first call and setting the `left_ptr` image whenever the
    /// cursor has none. Called from every pointer motion/button callback in
    /// `backend.rs` rather than once at `create_seat` time, so a consumer
    /// that never gets a pointer device pays nothing for a theme it never
    /// needed.
    ///
    /// A no-op with no seat.
    pub(crate) fn ensure_cursor_image(&self) {
        let (Some(cursor), Some(xcursor)) = (self.cursor_ptr(), *self.inner.xcursor.borrow())
        else {
            return;
        };
        // SAFETY: both pointers were created together by `create_seat` and
        // live as long as this runtime. `wlr_xcursor_manager_load` is safe
        // to call more than once (idempotent per its own header doc); this
        // crate calls it at most once per process via the `Cell` guard, and
        // `wlr_cursor_set_xcursor` is safe to call unconditionally.
        unsafe {
            if !self.inner.cursor_image_loaded.get() {
                sys::wlr_xcursor_manager_load(xcursor.as_ptr(), 1.0);
                self.inner.cursor_image_loaded.set(true);
            }
            sys::wlr_cursor_set_xcursor(cursor.as_ptr(), xcursor.as_ptr(), c"left_ptr".as_ptr());
        }
    }

    /// Record a keyboard the backend announced, for
    /// [`has_keyboard`](Runtime::has_keyboard) to count.
    pub(crate) fn record_keyboard(&self, kb: NonNull<sys::wlr_keyboard>) {
        self.inner.keyboards.borrow_mut().push(kb);
    }

    /// Forget a keyboard, called from `backend.rs`'s `on_input_destroy` once
    /// its device's own destroy has fired, so
    /// [`has_keyboard`](Runtime::has_keyboard) stops counting a device that
    /// is gone.
    pub(crate) fn forget_keyboard(&self, kb: NonNull<sys::wlr_keyboard>) {
        self.inner
            .keyboards
            .borrow_mut()
            .retain(|&recorded| recorded != kb);
    }

    /// Whether this runtime currently has a live keyboard. Used to decide
    /// whether the seat should advertise the keyboard capability.
    pub(crate) fn has_keyboard(&self) -> bool {
        !self.inner.keyboards.borrow().is_empty()
    }

    /// Record a pointer the backend announced, for
    /// [`has_pointer`](Runtime::has_pointer) to count.
    pub(crate) fn record_pointer(&self, p: NonNull<sys::wlr_pointer>) {
        self.inner.pointers.borrow_mut().push(p);
    }

    /// Forget a pointer; see [`forget_keyboard`](Runtime::forget_keyboard).
    pub(crate) fn forget_pointer(&self, p: NonNull<sys::wlr_pointer>) {
        self.inner
            .pointers
            .borrow_mut()
            .retain(|&recorded| recorded != p);
    }

    /// Whether this runtime currently has a live pointer. Used to decide
    /// whether the seat should advertise the pointer capability — mirroring
    /// [`has_keyboard`](Runtime::has_keyboard), which the original release of
    /// this method advertised the pointer capability unconditionally instead
    /// of gating on: a seat with no pointer attached was still telling
    /// clients it had one.
    pub(crate) fn has_pointer(&self) -> bool {
        !self.inner.pointers.borrow().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions::assert_not_impl_any;

    // The premise `dispatch.rs`'s thread-local guard rests on. An `Rc` and a
    // `RefCell` give this incidentally today; a future `Arc` field, or a
    // well-meant `unsafe impl Send`, would void the guard in silence.
    assert_not_impl_any!(Runtime: Send, Sync);

    fn pipe_read_end() -> OwnedFd {
        let (read, _write) = rustix::pipe::pipe().expect("pipe");
        read
    }

    #[test]
    fn ids_are_unique_and_resolve_to_the_fd_they_were_issued_for() {
        let rt = Runtime::new().expect("runtime");
        let a = rt.add_fd(pipe_read_end(), Interest::READABLE);
        let b = rt.add_fd(pipe_read_end(), Interest::READABLE);
        assert_ne!(a, b);

        let a_raw = rt.with_fd(a, |fd| fd.as_raw_fd()).expect("a resolves");
        let b_raw = rt.with_fd(b, |fd| fd.as_raw_fd()).expect("b resolves");
        assert_ne!(a_raw, b_raw, "each id names its own descriptor");
    }

    #[test]
    fn an_unknown_id_resolves_to_nothing_rather_than_panicking() {
        let rt = Runtime::new().expect("runtime");
        assert!(rt.with_fd(SourceId(u64::MAX), |_| ()).is_none());
    }

    /// Clones must share state, or a consumer's stored clone and the one
    /// `run_all` was given would disagree about which sources exist.
    #[test]
    fn a_clone_sees_sources_added_through_the_original() {
        let rt = Runtime::new().expect("runtime");
        let clone = rt.clone();
        let id = rt.add_fd(pipe_read_end(), Interest::READABLE);
        assert!(clone.with_fd(id, |_| ()).is_some());
    }

    /// The primitive `backend.rs`'s `ToplevelTableGuard` calls when a run
    /// returns: everything a run recorded must stop resolving.
    ///
    /// This cannot be exercised against a real `wlr_xdg_toplevel` without a
    /// live client (see the crate's own integration test's doc for why), but
    /// `record_toplevel`/`toplevel_entry`/`clear_toplevels` never dereference
    /// the pointers they store — they only insert, look up and clear by
    /// `ToplevelId` — so a dangling `NonNull` exercises the real code path
    /// exactly as a live one would, without wlroots in the loop at all.
    #[test]
    fn clear_toplevels_makes_every_recorded_id_resolve_to_nothing() {
        let rt = Runtime::new().expect("runtime");
        let id = ToplevelId(next_id());
        let raw = NonNull::<sys::wlr_xdg_toplevel>::dangling();
        let tree = NonNull::<sys::wlr_scene_tree>::dangling();

        rt.record_toplevel(id, raw, tree);
        assert!(
            rt.toplevel_entry(id).is_some(),
            "record_toplevel must make the id resolve"
        );

        rt.clear_toplevels();
        assert!(
            rt.toplevel_entry(id).is_none(),
            "clear_toplevels must make every previously-recorded id miss, \
             the same outcome an id from a run that has already returned \
             must produce"
        );
    }

    /// Mirrors the test above, for the additional purge `clear_toplevels`
    /// performs: a rect parented into a toplevel via
    /// `add_rect_in_toplevel` must not survive its announcing run once the
    /// toplevel owning its tree is forgotten, or a later `remove_rect`
    /// could call `wlr_scene_node_destroy` on a node wlroots already freed
    /// along with that tree. Dangling pointers throughout, for the same
    /// reason the test above uses one: `clear_toplevels` never
    /// dereferences a `RectEntry`'s pointer, only inserts/removes rows by
    /// id, so this exercises the real code path without a live wlroots
    /// object.
    #[test]
    fn clear_toplevels_also_purges_rects_parented_into_those_toplevels() {
        let rt = Runtime::new().expect("runtime");
        let toplevel = ToplevelId(next_id());
        let raw = NonNull::<sys::wlr_xdg_toplevel>::dangling();
        let tree = NonNull::<sys::wlr_scene_tree>::dangling();
        rt.record_toplevel(toplevel, raw, tree);

        let rect = RectId(next_id());
        rt.inner.rects.borrow_mut().insert(
            rect,
            RectEntry {
                raw: NonNull::<sys::wlr_scene_rect>::dangling(),
                parent: RectParent::Toplevel(toplevel),
            },
        );
        assert!(
            rt.inner.rects.borrow().contains_key(&rect),
            "the rect must be recorded before the purge"
        );

        rt.clear_toplevels();

        assert!(
            !rt.inner.rects.borrow().contains_key(&rect),
            "clear_toplevels must purge a rect parented into a toplevel it \
             just forgot, the same way forget_toplevel does within a \
             single run"
        );
    }

    /// One of the two orders a decoration and its toplevel can die in: the
    /// decoration dies first (the client destroys the resource). Only
    /// `forget_decoration`'s own table is purged; the toplevel is
    /// untouched. `record_decoration`/`forget_decoration` never dereference
    /// the pointer they store, only insert/remove by id (the same argument
    /// `clear_toplevels_makes_every_recorded_id_resolve_to_nothing` makes
    /// for `record_toplevel`), so a dangling `NonNull` exercises the real
    /// code path without a live wlroots object.
    #[test]
    fn forget_decoration_purges_the_decoration_entry_and_leaves_the_toplevel_alone() {
        let rt = Runtime::new().expect("runtime");
        let id = ToplevelId(next_id());
        rt.record_toplevel(
            id,
            NonNull::<sys::wlr_xdg_toplevel>::dangling(),
            NonNull::<sys::wlr_scene_tree>::dangling(),
        );
        rt.record_decoration(
            id,
            NonNull::<sys::wlr_xdg_toplevel_decoration_v1>::dangling(),
        );
        assert!(rt.decoration_ptr(id).is_some(), "must be recorded first");

        rt.forget_decoration(id);

        assert!(
            rt.decoration_ptr(id).is_none(),
            "forget_decoration must make the id resolve to no decoration"
        );
        assert!(
            rt.toplevel_entry(id).is_some(),
            "the decoration dying first must not touch the toplevel table"
        );
    }

    /// The other order: the toplevel dies first. `forget_toplevel` must
    /// purge the decoration table too — see `RuntimeInner::decorations`'s
    /// own doc for why this is load-bearing rather than tidiness (wlroots'
    /// own destroy handler for the decoration asserts its listener lists
    /// are empty, which is `backend.rs`'s job, but this is the data-side
    /// half of the same purge).
    #[test]
    fn forget_toplevel_also_purges_its_decoration_entry() {
        let rt = Runtime::new().expect("runtime");
        let id = ToplevelId(next_id());
        rt.record_toplevel(
            id,
            NonNull::<sys::wlr_xdg_toplevel>::dangling(),
            NonNull::<sys::wlr_scene_tree>::dangling(),
        );
        rt.record_decoration(
            id,
            NonNull::<sys::wlr_xdg_toplevel_decoration_v1>::dangling(),
        );
        assert!(rt.decoration_ptr(id).is_some(), "must be recorded first");

        rt.forget_toplevel(id);

        assert!(
            rt.toplevel_entry(id).is_none(),
            "forget_toplevel must make the id resolve to no toplevel"
        );
        assert!(
            rt.decoration_ptr(id).is_none(),
            "forget_toplevel must also purge the decoration recorded under \
             the same id — the toplevel-first half of the two-sided purge"
        );
    }

    /// `clear_toplevels` (the run-granularity purge, not the per-toplevel
    /// one) must take decorations with it too, for the identical reason it
    /// takes rects and buffers: a `ToplevelId` is going stale in the very
    /// next statement, and a decoration entry keyed by it must not outlive
    /// that.
    #[test]
    fn clear_toplevels_also_purges_decorations() {
        let rt = Runtime::new().expect("runtime");
        let id = ToplevelId(next_id());
        rt.record_toplevel(
            id,
            NonNull::<sys::wlr_xdg_toplevel>::dangling(),
            NonNull::<sys::wlr_scene_tree>::dangling(),
        );
        rt.record_decoration(
            id,
            NonNull::<sys::wlr_xdg_toplevel_decoration_v1>::dangling(),
        );

        rt.clear_toplevels();

        assert!(
            rt.decoration_ptr(id).is_none(),
            "clear_toplevels must purge decorations at run granularity too"
        );
    }

    /// `set_decoration_mode`'s central hazard (see its own doc): calling
    /// straight through to `wlr_xdg_toplevel_decoration_v1_set_mode` before
    /// the toplevel's surface is initialized asserts inside wlroots. This
    /// cannot be exercised end-to-end without a live client (same
    /// limitation the crate's own integration test doc states), but the
    /// staging decision itself is plain Rust bookkeeping — it reads
    /// `toplevel->base->initialized` and, when false, never dereferences
    /// the decoration pointer at all — so a fake but real (zeroed,
    /// heap-allocated) `wlr_xdg_surface`/`wlr_xdg_toplevel` pair exercises
    /// it exactly, with a dangling decoration pointer standing in safely
    /// for the reason `forget_decoration_purges_the_decoration_entry_and_leaves_the_toplevel_alone`
    /// already relies on: nothing on this path reads through it.
    #[test]
    fn set_decoration_mode_stages_rather_than_sends_before_the_surface_is_initialized() {
        use std::alloc::{Layout, alloc_zeroed};

        let rt = Runtime::new().expect("runtime");
        let id = ToplevelId(next_id());

        // Allocated rather than `std::mem::zeroed`-ed, for the reason
        // `backend.rs`'s `ScratchOutput` documents: both structs embed
        // `wl_listener`s whose bare function pointers are UB to
        // *materialise* as a zero value, so the bytes are only ever
        // touched through a raw pointer, and `initialized`/`base` are the
        // only two fields this test — or the code path it exercises — ever
        // reads.
        //
        // SAFETY: both layouts are non-zero-sized, so `alloc_zeroed`
        // returns either null (checked below) or a suitably aligned, zeroed
        // allocation of exactly that size, for the duration of this test
        // (leaked deliberately; a scratch fixture, not a `wlr_*` object
        // this crate ever tears down).
        let surface = unsafe { alloc_zeroed(Layout::new::<sys::wlr_xdg_surface>()) }
            .cast::<sys::wlr_xdg_surface>();
        assert!(!surface.is_null(), "allocation failed");
        let toplevel = unsafe { alloc_zeroed(Layout::new::<sys::wlr_xdg_toplevel>()) }
            .cast::<sys::wlr_xdg_toplevel>();
        assert!(!toplevel.is_null(), "allocation failed");
        // SAFETY: both allocations are freshly zeroed and exclusively owned;
        // `initialized` (a `bool`) and `base` (a `*mut wlr_xdg_surface`) are
        // both in bounds, and zero is already their correct starting value
        // for this test (`initialized = false`) or is about to be
        // overwritten (`base`).
        unsafe {
            (*surface).initialized = false;
            (*toplevel).base = surface;
        }

        let toplevel_raw = NonNull::new(toplevel).expect("allocation succeeded, so non-null");
        rt.record_toplevel(id, toplevel_raw, NonNull::<sys::wlr_scene_tree>::dangling());
        rt.record_decoration(
            id,
            NonNull::<sys::wlr_xdg_toplevel_decoration_v1>::dangling(),
        );

        assert_eq!(
            rt.set_decoration_mode(id, DecorationMode::ServerSide),
            Some(()),
            "staging still reports success to the caller"
        );
        assert!(
            rt.decoration_dispatch_flag(id),
            "the request-answered flag is set even when the send is only staged"
        );
        assert_eq!(
            rt.take_staged_decoration_mode(id),
            Some(DecorationMode::ServerSide),
            "the staged decision must be retrievable, and must be the value passed in"
        );
        assert_eq!(
            rt.take_staged_decoration_mode(id),
            None,
            "taking it clears it, so a second flush cannot resend a stale decision"
        );
    }

    /// The exact regression the fix closes: an immediate answer (the
    /// surface already initialized) must clear any earlier staged decision,
    /// or a later flush resends it and silently overrides whatever was just
    /// sent.
    ///
    /// This calls the real production code that enforces it —
    /// [`Runtime::mark_decoration_answered`], the bookkeeping
    /// `set_decoration_mode`'s immediate branch calls right after its FFI
    /// send — rather than the public method itself, which cannot be driven
    /// here: an earlier version of this test called `set_decoration_mode`
    /// directly, against a real `wl_client`/`wl_resource` and a `initialized
    /// = true` (but otherwise blank) `wlr_xdg_surface`, and it segfaulted
    /// inside wlroots' real `wlr_xdg_surface_schedule_configure` — verified
    /// empirically, not assumed, before `mark_decoration_answered` was
    /// pulled out into its own, separately-callable unit specifically so
    /// this invariant could be pinned without that crash. See that
    /// method's own doc for the full argument.
    #[test]
    fn mark_decoration_answered_clears_any_staged_decision() {
        let rt = Runtime::new().expect("runtime");
        let id = ToplevelId(next_id());
        rt.record_toplevel(
            id,
            NonNull::<sys::wlr_xdg_toplevel>::dangling(),
            NonNull::<sys::wlr_scene_tree>::dangling(),
        );
        rt.record_decoration(
            id,
            NonNull::<sys::wlr_xdg_toplevel_decoration_v1>::dangling(),
        );

        // Stage a pre-commit decision, exactly as `set_decoration_mode`'s
        // own staging branch does when a `request_mode` fires before the
        // initial commit — the scenario the review traced: a consumer's
        // `initial_commit` handler later answers explicitly (the immediate
        // branch, reached because the surface is initialized by then),
        // while this earlier staged value is still sitting in the table.
        rt.inner
            .decorations
            .borrow()
            .get(&id)
            .expect("just recorded")
            .staged
            .set(Some(DecorationMode::ServerSide));
        assert_eq!(
            rt.inner.decorations.borrow().get(&id).unwrap().staged.get(),
            Some(DecorationMode::ServerSide),
            "the stage must be visible before `mark_decoration_answered` \
             runs, or this test would not be exercising the hazard at all"
        );

        rt.mark_decoration_answered(id);

        assert_eq!(
            rt.take_staged_decoration_mode(id),
            None,
            "an answered request must leave nothing staged behind — a \
             non-empty `staged` here is exactly the bug: `on_surface_commit`'s \
             later flush would resend it and override the answer that was \
             just sent on the immediate branch"
        );
    }

    /// Record a toplevel with a heap-allocated `wlr_xdg_surface` whose
    /// `initialized` flag is `init`, plus a decoration for it, and return
    /// the id.
    ///
    /// `alloc_zeroed` rather than `Box::new(mem::zeroed())`, for the reason
    /// the staged-decision test above already documents: both structs embed
    /// `wl_listener`s whose bare function pointers are UB to *materialise*
    /// as a zero value, so the bytes are only ever touched through a raw
    /// pointer. `initialized` and `base` are the only fields these tests, or
    /// the code paths they exercise, ever read.
    ///
    /// Both allocations are deliberately leaked. These tests never enter
    /// wlroots, so nothing frees them, and an allocation reclaimed at end of
    /// scope would leave the table holding a dangling `base` for any later
    /// assertion.
    fn record_toplevel_with_surface(rt: &Runtime, init: bool) -> ToplevelId {
        use std::alloc::{Layout, alloc_zeroed};

        let id = ToplevelId(next_id());
        // SAFETY: both layouts are non-zero-sized, so `alloc_zeroed` returns
        // either null (checked) or a suitably aligned, zeroed allocation of
        // exactly that size.
        let surface = unsafe { alloc_zeroed(Layout::new::<sys::wlr_xdg_surface>()) }
            .cast::<sys::wlr_xdg_surface>();
        assert!(!surface.is_null(), "allocation failed");
        let toplevel = unsafe { alloc_zeroed(Layout::new::<sys::wlr_xdg_toplevel>()) }
            .cast::<sys::wlr_xdg_toplevel>();
        assert!(!toplevel.is_null(), "allocation failed");
        // SAFETY: both allocations are freshly zeroed and exclusively owned;
        // `initialized` (a `bool`) and `base` (a `*mut wlr_xdg_surface`) are
        // both in bounds.
        unsafe {
            (*surface).initialized = init;
            (*toplevel).base = surface;
        }
        rt.record_toplevel(
            id,
            NonNull::new(toplevel).expect("allocation succeeded"),
            NonNull::<sys::wlr_scene_tree>::dangling(),
        );
        rt.record_decoration(
            id,
            NonNull::<sys::wlr_xdg_toplevel_decoration_v1>::dangling(),
        );
        id
    }

    /// H2, at the table level: a decision staged before the initial commit
    /// marks the decoration answered, and stays answered after the flush
    /// takes it.
    ///
    /// `on_surface_commit` picks between "flush the staged decision" and
    /// "nobody has answered, so ask the handler" — and in 0.20.8 the second
    /// arm's guard was "nothing is staged", which the flush itself has just
    /// made true. `decoration_answered` is the guard that survives the
    /// flush; this pins that it does.
    #[test]
    fn a_staged_decision_marks_the_decoration_answered_and_stays_answered_after_the_flush() {
        let rt = Runtime::new().expect("runtime");
        let id = record_toplevel_with_surface(&rt, false);

        assert!(
            !rt.decoration_answered(id),
            "a freshly recorded decoration has been answered by nobody"
        );

        rt.set_decoration_mode(id, DecorationMode::ClientSide);
        assert!(
            rt.decoration_answered(id),
            "staging is answering — the decision exists, it is merely not on the wire yet"
        );

        assert_eq!(
            rt.take_staged_decoration_mode(id),
            Some(DecorationMode::ClientSide),
            "the flush must retrieve exactly what was staged"
        );
        assert!(
            rt.decoration_answered(id),
            "and taking the staged value must NOT un-answer the decoration: this is \
             the 0.20.8 bug — `on_surface_commit` would fall through to the \
             synthetic-request arm and override a real decision with the server-side default"
        );
    }

    /// H2's other half: an *immediate* answer (surface already initialized,
    /// so nothing is ever staged) also marks the decoration answered.
    ///
    /// This is the sequence that actually bit — a handler answering from
    /// `initial_commit`, where `initialized` is already true. `staged` is
    /// empty the whole way through, so a staged-only guard cannot tell this
    /// apart from a decoration nobody touched.
    ///
    /// Driven through `mark_decoration_answered` plus the latch rather than
    /// through `set_decoration_mode`, because the immediate branch makes a
    /// real FFI call that segfaults on a fabricated surface — see
    /// `mark_decoration_answered`'s own doc for why that is established
    /// empirically rather than assumed.
    #[test]
    fn an_immediate_answer_leaves_nothing_staged_but_still_reads_as_answered() {
        let rt = Runtime::new().expect("runtime");
        let id = record_toplevel_with_surface(&rt, true);

        {
            let decorations = rt.inner.decorations.borrow();
            let entry = decorations.get(&id).expect("just recorded");
            entry.answered.set(true);
            entry.mode_set_this_dispatch.set(true);
        }
        rt.mark_decoration_answered(id);

        assert_eq!(
            rt.take_staged_decoration_mode(id),
            None,
            "an immediate answer stages nothing"
        );
        assert!(
            rt.decoration_answered(id),
            "yet the decoration is answered — the distinction a staged-only guard \
             cannot make, and the reason `answered` exists"
        );
    }

    /// H3, at the table level: a decoration created after its toplevel's
    /// initial commit starts out unanswered on an already-initialized
    /// surface, which is exactly the condition
    /// `on_new_toplevel_decoration`'s late-creation branch tests before
    /// giving the handler its say.
    ///
    /// Without that branch such a decoration is never answered by anything
    /// — `on_surface_commit`'s once-only path has already run — and a client
    /// that never calls `set_mode` waits forever for a configure.
    #[test]
    fn a_decoration_created_after_the_initial_commit_starts_unanswered() {
        let rt = Runtime::new().expect("runtime");
        let id = record_toplevel_with_surface(&rt, true);

        let entry = rt.toplevel_entry(id).expect("recorded");
        // SAFETY: `record_toplevel_with_surface` leaked a live, non-null
        // `base` for this id and nothing has freed it.
        let initialized = unsafe {
            let base = (*entry.raw.as_ptr()).base;
            !base.is_null() && (*base).initialized
        };
        assert!(
            initialized,
            "the premise of the late-creation branch: the commit has already happened"
        );
        assert!(
            !rt.decoration_answered(id),
            "and nothing has answered this decoration — so the branch must fire, \
             or this client never gets a decoration configure at all"
        );

        // Once answered, the branch must not fire again on any later
        // trigger. The latch is set directly rather than through
        // `set_decoration_mode`: this surface is `initialized`, so that call
        // would take the immediate branch and make the real
        // `wlr_xdg_toplevel_decoration_v1_set_mode` FFI call, which
        // segfaults on a fabricated surface — see `mark_decoration_answered`'s
        // own doc. `set_decoration_mode`'s latching on both branches is
        // covered by the staged test above.
        rt.inner
            .decorations
            .borrow()
            .get(&id)
            .expect("recorded")
            .answered
            .set(true);
        assert!(
            rt.decoration_answered(id),
            "the latch holds, so a second announcement cannot re-ask and override"
        );
    }

    /// `answered` is per-decoration, not global: answering one toplevel's
    /// decoration must not suppress the synthetic request for another's.
    #[test]
    fn answered_is_scoped_to_one_decoration() {
        let rt = Runtime::new().expect("runtime");
        let a = record_toplevel_with_surface(&rt, false);
        let b = record_toplevel_with_surface(&rt, false);

        rt.set_decoration_mode(a, DecorationMode::ClientSide);

        assert!(rt.decoration_answered(a), "the one that was answered");
        assert!(
            !rt.decoration_answered(b),
            "must not answer for its neighbour, or a second window silently \
             loses its negotiation"
        );
    }

    /// A decoration with no entry at all reads as unanswered.
    ///
    /// Both callers check for the decoration's existence separately, so this
    /// is the safe default: it can only lead to *not* suppressing a request
    /// that was never going to be emitted.
    #[test]
    fn decoration_answered_is_false_for_an_unknown_id() {
        let rt = Runtime::new().expect("runtime");
        assert!(!rt.decoration_answered(ToplevelId(next_id())));
    }

    // M-2 — banded scene trees.
    //
    // Every test below needs a real `wlr_scene` (band creation and
    // `wlr_scene_node_reparent`/`raise_to_top` walk and mutate real
    // intrusive `wl_list`s, which a dangling pointer cannot stand in for),
    // so each one brings up a headless backend and calls `init_graphics`
    // for real, exactly like `tests/scene.rs` does. What each test does
    // *not* need is a real client: a toplevel's or a layer surface's own
    // scene tree is created directly with `wlr_scene_tree_create`, parented
    // where `on_new_toplevel`/`on_new_layer_surface` would parent it, and
    // recorded with `record_toplevel`/`record_layer_surface` exactly as
    // those callbacks do — which is enough to exercise the real mutators
    // (`raise_toplevel`, `reparent_layer_surface_if_changed`) against a real
    // scene without needing a client library in this crate's own test
    // suite (see `tests/layers.rs`'s own doc for why that is out of reach
    // here).

    /// Ensures `WLR_BACKENDS`/`WLR_HEADLESS_OUTPUTS` are set exactly once for
    /// this whole `--lib` unit-test binary.
    ///
    /// Delegates to `interest::tests::headless_env`, the one shared
    /// `Once`-guarded writer for this binary — see that function's own doc
    /// for why a second, independent `Once` here would race it rather than
    /// merely duplicate it. Mirrors the integration tests' own per-binary
    /// `headless_env` helper (`tests/scene.rs`, `tests/layers.rs`, ...),
    /// which each need their own copy because each integration-test file is
    /// a separate binary; this `--lib` binary has exactly one.
    fn headless_env() {
        crate::interest::tests::headless_env();
    }

    /// A runtime with a real scene, brought up exactly the way a consumer's
    /// `main` does: `Display::new`, `Backend::autocreate`, `init_graphics`.
    ///
    /// The `Display` and `Backend` are deliberately leaked (`Box::leak`)
    /// rather than returned to the caller: [`Runtime`]'s own doc requires
    /// both to outlive it, and this crate's own `Graphics` already leaks
    /// the scene, renderer and allocator for the identical "one per
    /// process, the OS reclaims it at exit" reason (see `Graphics`'s own
    /// doc) — a short-lived unit test process is exactly that "process"
    /// with nothing else asking for the memory back before it exits.
    /// Returning owned `Display`/`Backend` values instead would require
    /// giving each caller a way to keep them alive for exactly as long as
    /// `rt`, which is more machinery than any test below needs.
    fn headless_runtime() -> Runtime {
        headless_env();
        let display: &'static crate::Display =
            Box::leak(Box::new(crate::Display::new().expect("display")));
        let backend: &'static Backend<'static> = Box::leak(Box::new(
            Backend::autocreate(&display.event_loop()).expect("backend"),
        ));
        let rt = Runtime::new().expect("runtime");
        rt.init_graphics(display, backend).expect("init_graphics");
        rt
    }

    /// Every node directly under `tree`, in scene-child order (oldest/
    /// bottom-most first) — the same order `wlr_scene_node_at`'s hit
    /// testing and the compositor's own paint walk see. Identifies each
    /// child by the address of its own `node.link` — a `wl_list` entry's
    /// address is stable and unique for the life of the node, so comparing
    /// these addresses is exactly as precise as comparing the nodes
    /// themselves, without needing a `container_of`-style cast back to
    /// whatever concrete type each child actually is (a mix of
    /// `wlr_scene_tree`, `wlr_scene_rect`, `wlr_scene_buffer`, ... in the
    /// general case).
    fn scene_children(tree: NonNull<sys::wlr_scene_tree>) -> Vec<*const sys::wl_list> {
        // SAFETY: `tree` is a live scene tree for the whole of this call
        // (every caller below holds the runtime, and so the scene, alive
        // throughout); `children` is `wl_list`, valid as long as the tree
        // is, and this only reads `next` pointers, never mutates.
        unsafe {
            let head = &raw const (*tree.as_ptr()).children;
            let mut out = Vec::new();
            let mut cur = (*head).next;
            while cur != head.cast_mut() {
                out.push(cur as *const sys::wl_list);
                cur = (*cur).next;
            }
            out
        }
    }

    /// The frozen fix for M-2: the five bands exist, are direct children of
    /// the scene root, and are created in exactly this bottom-to-top order,
    /// which is what makes every later toplevel/layer-surface placement
    /// stack correctly with no further bookkeeping — see
    /// `Graphics::background_band`'s own doc for the mechanism.
    #[test]
    fn scene_bands_are_created_bottom_to_top_in_a_fixed_order() {
        let rt = headless_runtime();

        let scene = rt.scene_ptr().expect("scene");
        // SAFETY: `scene` is live for the whole of this call (owned by `rt`,
        // which outlives it).
        let root = NonNull::from(unsafe { &(*scene.as_ptr()).tree });
        let actual = scene_children(root);

        let g = rt.inner.graphics.borrow();
        let g = g.as_ref().expect("graphics");
        let band_link = |t: NonNull<sys::wlr_scene_tree>| -> *const sys::wl_list {
            // SAFETY: every band tree is live for the whole of this call.
            unsafe { &raw const (*t.as_ptr()).node.link }
        };
        let expected = vec![
            band_link(g.background_band),
            band_link(g.bottom_band),
            band_link(g.toplevel_band),
            band_link(g.top_band),
            band_link(g.overlay_band),
        ];

        assert_eq!(
            actual, expected,
            "the five bands must be the scene root's first five children, \
             in Background < Bottom < toplevels < Top < Overlay order"
        );
    }

    /// A layer surface whose `layer` changes on a later commit is
    /// reparented into the new band — the fix for the "Top panel pushed
    /// below the next toplevel" failure M-2 describes for the old
    /// once-at-creation placement. A commit that reports the *same* layer
    /// it already has must not touch the scene at all.
    #[test]
    fn reparent_layer_surface_if_changed_moves_the_tree_only_when_the_layer_changed() {
        let rt = headless_runtime();

        let background = rt.layer_band_ptr(Layer::Background).expect("background");
        let top = rt.layer_band_ptr(Layer::Top).expect("top");

        // Stands in for the tree `wlr_scene_layer_surface_v1_create` would
        // have handed `on_new_layer_surface`, parented under `Background`
        // exactly as that callback would for a surface that asked for it.
        // SAFETY: `background` is a live tree owned by `rt`'s own scene.
        let tree =
            NonNull::new(unsafe { sys::wlr_scene_tree_create(background.as_ptr()) }).unwrap();

        let id = LayerSurfaceId(next_id());
        rt.record_layer_surface(
            id,
            NonNull::<sys::wlr_layer_surface_v1>::dangling(),
            tree,
            Layer::Background,
        );

        // Same layer again: no-op.
        rt.reparent_layer_surface_if_changed(id, Layer::Background);
        assert_eq!(
            unsafe { (*tree.as_ptr()).node.parent },
            background.as_ptr(),
            "an unchanged layer must not be reparented"
        );

        // The client asked for `Top` on a later commit: must move.
        rt.reparent_layer_surface_if_changed(id, Layer::Top);
        assert_eq!(
            unsafe { (*tree.as_ptr()).node.parent },
            top.as_ptr(),
            "a changed layer must reparent the surface's tree into the new band"
        );

        // Asking for `Top` again, now that it is already there: no-op,
        // proven by the parent staying exactly `top` (a bogus second
        // reparent onto the same tree would not be observable by this
        // assertion alone, but this at minimum proves nothing moved it
        // *out* of `top`).
        rt.reparent_layer_surface_if_changed(id, Layer::Top);
        assert_eq!(unsafe { (*tree.as_ptr()).node.parent }, top.as_ptr());
    }

    /// `reparent_layer_surface_if_changed` misses cleanly for an id this
    /// runtime never recorded, the same "unknown id is not a fault" rule
    /// every other by-id operation in this crate follows.
    #[test]
    fn reparent_layer_surface_if_changed_is_none_for_an_unknown_id() {
        let rt = headless_runtime();
        let dead = LayerSurfaceId::dangling_for_test();
        assert_eq!(rt.reparent_layer_surface_if_changed(dead, Layer::Top), None);
    }

    /// `raise_toplevel` reorders a toplevel only among its own siblings
    /// inside the toplevel band — it must never promote the toplevel's node
    /// out of that band, and the band order itself (toplevel band still
    /// below the top band) must be completely unaffected by the raise. This
    /// is the frozen, tested claim behind `raise_toplevel`'s
    /// published-behavior doc note: a `Top`/`Overlay` layer surface stays
    /// above every toplevel unconditionally, raise or no raise.
    #[test]
    fn raise_toplevel_reorders_only_within_the_toplevel_band() {
        let rt = headless_runtime();

        let toplevel_band = rt.toplevel_band_ptr().expect("toplevel band");
        let top_band = rt.layer_band_ptr(Layer::Top).expect("top band");

        // Two toplevels' own trees, standing in for what `on_new_toplevel`
        // would have created for two real windows.
        // SAFETY: `toplevel_band` is a live tree owned by `rt`'s own scene.
        let win_a =
            NonNull::new(unsafe { sys::wlr_scene_tree_create(toplevel_band.as_ptr()) }).unwrap();
        let win_b =
            NonNull::new(unsafe { sys::wlr_scene_tree_create(toplevel_band.as_ptr()) }).unwrap();

        let id_a = ToplevelId(next_id());
        rt.record_toplevel(id_a, NonNull::<sys::wlr_xdg_toplevel>::dangling(), win_a);
        let id_b = ToplevelId(next_id());
        rt.record_toplevel(id_b, NonNull::<sys::wlr_xdg_toplevel>::dangling(), win_b);

        // `win_a` was created first, so it starts below `win_b` in the
        // toplevel band; raise it above its sibling.
        rt.raise_toplevel(id_a).expect("id_a is known");

        assert_eq!(
            unsafe { (*win_a.as_ptr()).node.parent },
            toplevel_band.as_ptr(),
            "raising a toplevel must not change which band it is parented \
             under"
        );
        assert_eq!(
            scene_children(toplevel_band),
            vec![
                // SAFETY: both are live trees for the whole of this call.
                unsafe { &raw const (*win_b.as_ptr()).node.link },
                unsafe { &raw const (*win_a.as_ptr()).node.link },
            ],
            "within the band, the raised toplevel must now be the topmost \
             sibling"
        );

        // The band order itself — the actual guarantee a `Top` panel relies
        // on — is a root-level property this raise call never touches.
        let scene = rt.scene_ptr().expect("scene");
        // SAFETY: `scene` is live for the whole of this call.
        let root = NonNull::from(unsafe { &(*scene.as_ptr()).tree });
        let root_children = scene_children(root);
        let toplevel_band_link = unsafe { &raw const (*toplevel_band.as_ptr()).node.link };
        let top_band_link = unsafe { &raw const (*top_band.as_ptr()).node.link };
        let toplevel_pos = root_children
            .iter()
            .position(|&p| p == toplevel_band_link)
            .expect("toplevel band is a root child");
        let top_pos = root_children
            .iter()
            .position(|&p| p == top_band_link)
            .expect("top band is a root child");
        assert!(
            toplevel_pos < top_pos,
            "raising a toplevel must never move the toplevel band above the \
             top band: a Top layer surface must stay above every toplevel \
             regardless of any raise_toplevel call"
        );
    }

    /// The frozen fix for M-3 task 1: `add_rect_in_band` parents the rect
    /// into the named band's own tree, not the scene root — the whole
    /// point of the method versus [`Runtime::add_rect`] (see that method's
    /// own doc on the "swallows pointer input, sits above everything"
    /// tradeoff `add_rect_in_band` exists to avoid).
    #[test]
    fn add_rect_in_band_parents_into_the_named_band() {
        let rt = headless_runtime();
        let rect = rt
            .add_rect_in_band(Band::Overlay, 4, 4, [1.0, 0.0, 0.0, 1.0])
            .expect("rect");
        let overlay = rt.inner.graphics.borrow().as_ref().unwrap().overlay_band;
        assert!(
            rt.rect_is_in_band(rect, overlay),
            "a Band::Overlay rect must be a direct child of the overlay band"
        );
        assert_eq!(rt.remove_rect(rect), Some(()));
    }

    /// `add_rect_in_band` must not parent into a *different* band than the
    /// one asked for — the only way the assertion above could pass
    /// vacuously is if every band happened to share a tree, which they do
    /// not (see `scene_bands_are_created_bottom_to_top_in_a_fixed_order`).
    #[test]
    fn add_rect_in_band_does_not_parent_into_a_different_band() {
        let rt = headless_runtime();
        let rect = rt
            .add_rect_in_band(Band::Background, 4, 4, [0.0, 1.0, 0.0, 1.0])
            .expect("rect");
        let overlay = rt.inner.graphics.borrow().as_ref().unwrap().overlay_band;
        assert!(
            !rt.rect_is_in_band(rect, overlay),
            "a Band::Background rect must not be parented into the overlay band"
        );
        assert_eq!(rt.remove_rect(rect), Some(()));
    }

    /// `add_rect_in_band` before `init_graphics` must error, exactly like
    /// `add_rect` does, rather than panicking on the missing scene.
    #[test]
    fn add_rect_in_band_before_init_graphics_errors() {
        let rt = Runtime::new().expect("runtime");
        assert!(
            rt.add_rect_in_band(Band::Overlay, 8, 8, [0.0, 0.0, 0.0, 1.0])
                .is_err()
        );
    }

    /// A rect parented into `Band::Toplevel` survives `clear_toplevels` —
    /// the band tree itself outlives every toplevel ever parented into it,
    /// so a rect that is a *sibling* of toplevel trees (not a descendant of
    /// any one of them) must never be purged just because some toplevel
    /// died. This is the property `RectParent::Band` exists to keep
    /// distinct from `RectParent::Toplevel`.
    #[test]
    fn clear_toplevels_does_not_purge_a_band_rect() {
        let rt = headless_runtime();
        let toplevel_band = rt.toplevel_band_ptr().expect("toplevel band");
        let tree =
            NonNull::new(unsafe { sys::wlr_scene_tree_create(toplevel_band.as_ptr()) }).unwrap();
        let toplevel = ToplevelId(next_id());
        rt.record_toplevel(toplevel, NonNull::<sys::wlr_xdg_toplevel>::dangling(), tree);

        let rect = rt
            .add_rect_in_band(Band::Toplevel, 4, 4, [0.0, 0.0, 1.0, 1.0])
            .expect("rect");

        rt.clear_toplevels();

        assert!(
            rt.inner.rects.borrow().contains_key(&rect),
            "a Band::Toplevel rect must survive clear_toplevels: it is a \
             sibling of toplevel trees, not a child of one"
        );
        assert_eq!(rt.remove_rect(rect), Some(()));
    }

    /// `set_layer_surface_output` on a dead layer-surface id must be `None`
    /// without ever reaching output resolution — proven by handing it an
    /// `OutputId` this runtime never issued either (an unresolvable id on
    /// both sides still misses cleanly, and the layer-surface check must
    /// short-circuit before touching the outputs table at all).
    #[test]
    fn set_layer_surface_output_on_a_dead_layer_id_is_none() {
        let rt = headless_runtime();
        let dead_layer = LayerSurfaceId::dangling_for_test();
        let dead_output = OutputId(next_id());
        assert_eq!(rt.set_layer_surface_output(dead_layer, dead_output), None);
    }

    /// `set_layer_surface_output` assigns the raw `output` field on a live
    /// layer surface — the sanctioned mechanism `layer.rs`'s
    /// `LayerSurface::output_id` doc now points back to (there is no
    /// wlroots setter function; see `set_layer_surface_output`'s own doc).
    #[test]
    fn set_layer_surface_output_assigns_the_raw_output_field() {
        let rt = headless_runtime();

        // A zeroed, stack-local `wlr_layer_surface_v1` stands in for a live
        // one: `set_layer_surface_output` only ever writes `(*raw).output`,
        // and this test only ever reads that one field back, so a zeroed
        // value (rather than a real wlroots object) is enough — the same
        // "never dereferenced beyond one known field" trick
        // `raise_toplevel_reorders_only_within_the_toplevel_band` and
        // friends use for dangling `wlr_xdg_toplevel`/`wlr_scene_tree`
        // pointers.
        //
        // SAFETY: `wlr_layer_surface_v1` is a plain `repr(C)` struct of
        // pointers/integers/bools with no validity invariant tighter than
        // "well-defined bit pattern" for any of them (a null pointer, a `0`
        // integer and `false` are all valid), so its all-zero bit pattern
        // is a valid value of the type. Every field but `output` stays
        // zero/null for this test's whole life and is never read.
        let mut layer_surface: sys::wlr_layer_surface_v1 = unsafe { std::mem::zeroed() };
        let raw = NonNull::from(&mut layer_surface);
        let id = LayerSurfaceId(next_id());
        rt.record_layer_surface(
            id,
            raw,
            NonNull::<sys::wlr_scene_tree>::dangling(),
            Layer::Background,
        );

        let output_raw = NonNull::<sys::wlr_output>::dangling();
        let output_id = OutputId(next_id());
        rt.record_output(output_id, output_raw);

        assert_eq!(rt.set_layer_surface_output(id, output_id), Some(()));
        assert_eq!(layer_surface.output, output_raw.as_ptr());
    }

    /// M1 regression: `forget_output` — the single choke point
    /// `on_output_destroy` runs before wlroots frees an output — must null
    /// this output's raw pointer out of every layer surface still holding
    /// it, or [`LayerSurface::output_id`](crate::LayerSurface::output_id)
    /// would later dereference the freed output (it reads
    /// `(*output).addons`). A layer surface assigned to a *different* output
    /// must be left untouched.
    #[test]
    fn forget_output_nulls_the_planted_pointer_only_in_matching_layer_surfaces() {
        use std::alloc::{Layout, alloc_zeroed};

        let rt = headless_runtime();

        // Two heap-allocated, zeroed `wlr_output`s at distinct addresses
        // stand in for two live outputs. `forget_output` never dereferences
        // an output pointer — it only compares its identity and writes null —
        // so a zeroed region never read beyond that comparison is enough.
        // `alloc_zeroed` (touched only through a raw pointer, never
        // *materialised* as a `wlr_output` value) rather than
        // `mem::zeroed()`, for the reason `record_toplevel_with_surface`
        // documents: `wlr_output` embeds function pointers that must be
        // non-null, so producing one by value is UB even if it is never read.
        // `NonNull::dangling()` cannot serve either: it is alignment-derived
        // and so identical for both, which would defeat the "different output
        // left untouched" half. Deliberately leaked — a scratch fixture this
        // crate never tears down.
        //
        // SAFETY: the layout is non-zero-sized, so `alloc_zeroed` returns
        // either null (checked) or a suitably aligned zeroed allocation; the
        // bytes are only ever read as an opaque pointer identity, never as a
        // `wlr_output`.
        let out_a = {
            let p = unsafe { alloc_zeroed(Layout::new::<sys::wlr_output>()) }
                .cast::<sys::wlr_output>();
            NonNull::new(p).expect("allocation failed")
        };
        let out_b = {
            let p = unsafe { alloc_zeroed(Layout::new::<sys::wlr_output>()) }
                .cast::<sys::wlr_output>();
            NonNull::new(p).expect("allocation failed")
        };
        let id_a = OutputId(next_id());
        let id_b = OutputId(next_id());
        rt.record_output(id_a, out_a);
        rt.record_output(id_b, out_b);

        // Two zeroed layer surfaces whose only ever-touched field is
        // `output` — as the assign-the-raw-field test above.
        // SAFETY: as that test.
        let mut ls_on_a: sys::wlr_layer_surface_v1 = unsafe { std::mem::zeroed() };
        let mut ls_on_b: sys::wlr_layer_surface_v1 = unsafe { std::mem::zeroed() };
        let ls_a = LayerSurfaceId(next_id());
        let ls_b = LayerSurfaceId(next_id());
        rt.record_layer_surface(
            ls_a,
            NonNull::from(&mut ls_on_a),
            NonNull::<sys::wlr_scene_tree>::dangling(),
            Layer::Background,
        );
        rt.record_layer_surface(
            ls_b,
            NonNull::from(&mut ls_on_b),
            NonNull::<sys::wlr_scene_tree>::dangling(),
            Layer::Top,
        );

        // Plant each output's pointer through the real assignment path, so
        // the identity `forget_output` compares against is exactly the one
        // production code stores.
        assert_eq!(rt.set_layer_surface_output(ls_a, id_a), Some(()));
        assert_eq!(rt.set_layer_surface_output(ls_b, id_b), Some(()));
        assert_eq!(ls_on_a.output, out_a.as_ptr());
        assert_eq!(ls_on_b.output, out_b.as_ptr());

        // Destroy output A. Its pointer must be nulled out of the surface
        // that named it; the surface on B must be left alone.
        rt.forget_output(id_a);
        assert!(
            ls_on_a.output.is_null(),
            "the dying output's pointer must be nulled out of its layer surface"
        );
        assert_eq!(
            ls_on_b.output,
            out_b.as_ptr(),
            "a layer surface assigned to a different output must be untouched"
        );

        // And the id itself is gone — set_layer_surface_output can no longer
        // resolve it, confirming the same call also forgot the output.
        assert_eq!(rt.set_layer_surface_output(ls_a, id_a), None);
    }
}
